//! Full-tree consistency check (fsck), used by tests and available to hosts.

use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::error::{CorruptKind, Error};
use crate::format::{
    self, hash_prefix, inner_slots, is_valid_key, key_counter, key_prefix, leaf_slots, Value,
    LEAF_LEVEL, MIN_COUNTER, NO_LINK, ROOT,
};
use crate::hasher::NameHasher;
use crate::page_workspace::PageWorkspace;
use crate::store::PageStore;
use crate::tree::NameTree;
use crate::{inner, leaf};

/// What a clean [`NameTree::check`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CheckReport {
    /// Pages in the store.
    pub pages: u32,
    /// Leaf pages.
    pub leaves: u32,
    /// Levels from root to leaves: 0 for an empty tree, 1 for a single leaf.
    pub height: u32,
    /// Names stored.
    pub names: u64,
}

impl<S: PageStore, H: NameHasher, V: Value> NameTree<S, H, V> {
    /// Walks the whole tree and checks every format invariant, including the expensive ones
    /// that are not checked on each read: key order, separator ranges, name overlap,
    /// fragmentation counts, the leaf chain, unreachable pages and every key against its
    /// name's hash.
    pub fn check(&self) -> Result<CheckReport, Error<S::Error>> {
        self.check_with(|_| true)
    }

    /// Like [`check`](Self::check), and also rejects any stored value for which `value_ok`
    /// returns `false` (for example, a directory host might reject an entry type of 0). `value_ok` may read from
    /// the tree.
    pub fn check_with(
        &self,
        value_ok: impl FnMut(&V) -> bool,
    ) -> Result<CheckReport, Error<S::Error>> {
        let pages = {
            let mut workspace = self.workspace.borrow_mut();
            workspace.begin()?;
            workspace.discard();
            workspace.count()
        };
        if pages == 0 {
            return Ok(CheckReport::default());
        }
        let mut checker = Checker {
            workspace: &self.workspace,
            hasher: &self.hasher,
            value_ok,
            visited: vec![false; pages as usize],
            leaves: Vec::new(),
            names: 0,
            spare: Vec::new(),
        };
        let result = checker.run::<V>();
        checker.release();
        let height = result?;
        Ok(CheckReport {
            pages,
            leaves: checker.leaves.len() as u32,
            height,
            names: checker.names,
        })
    }
}

/// A leaf seen during the walk, in key order.
struct LeafInfo {
    index: u32,
    link: u32,
    first_prefix: u64,
    last_prefix: u64,
}

struct Checker<'a, S, H, F> {
    /// Borrowed only for each page read, so `value_ok` can call back into the tree.
    workspace: &'a RefCell<PageWorkspace<S>>,
    hasher: &'a H,
    value_ok: F,
    visited: Vec<bool>,
    leaves: Vec<LeafInfo>,
    names: u64,
    /// Page buffers not in use. The walk holds one page per level, so this stays as small as
    /// the tree is high; the buffers come from, and go back to, the workspace.
    spare: Vec<Vec<u8>>,
}

type Res<T, S> = Result<T, Error<<S as PageStore>::Error>>;

fn corrupt<T, E>(kind: CorruptKind) -> Result<T, Error<E>> {
    Err(Error::Corrupt(kind))
}

impl<S: PageStore, H: NameHasher, F> Checker<'_, S, H, F> {
    /// Checks the whole tree and returns its height.
    fn run<V: Value>(&mut self) -> Res<u32, S>
    where
        F: FnMut(&V) -> bool,
    {
        let root = self.read(ROOT)?;
        let height = format::level(&root);
        self.spare.push(root);
        self.walk::<V>(ROOT, height, 0, None)?;
        self.finish()?;
        Ok(height)
    }

    /// A validated copy of page `index`, in a buffer to hand back to `spare` when done.
    fn read(&mut self, index: u32) -> Res<Vec<u8>, S> {
        let mut page = match self.spare.pop() {
            Some(buf) => buf,
            None => self.workspace.borrow_mut().take_buf(),
        };
        let read = self.workspace.borrow().read_detached(index, &mut page);
        match read {
            Ok(()) => Ok(page),
            Err(e) => {
                self.spare.push(page);
                Err(e)
            }
        }
    }

    /// Gives every spare buffer back to the workspace, for reuse by later calls.
    fn release(&mut self) {
        let mut workspace = self.workspace.borrow_mut();
        for buf in self.spare.drain(..) {
            workspace.give_buf(buf);
        }
    }

    /// Checks the subtree at `index`, which must be at `level` and hold only keys in
    /// `low..high` (`high` of `None` meaning no upper bound).
    fn walk<V: Value>(&mut self, index: u32, level: u32, low: u64, high: Option<u64>) -> Res<(), S>
    where
        F: FnMut(&V) -> bool,
    {
        let seen = &mut self.visited[index as usize];
        if *seen {
            return corrupt(CorruptKind::DuplicatePage);
        }
        *seen = true;
        let page = self.read(index)?;
        let result = self.check_page::<V>(index, level, low, high, &page);
        self.spare.push(page);
        result
    }

    /// Checks page `index`, already read into `page`, and the subtree below it.
    fn check_page<V: Value>(
        &mut self,
        index: u32,
        level: u32,
        low: u64,
        high: Option<u64>,
        page: &[u8],
    ) -> Res<(), S>
    where
        F: FnMut(&V) -> bool,
    {
        if format::level(page) != level {
            return corrupt(CorruptKind::LevelMismatch);
        }
        let keys: Vec<u64> = if level == LEAF_LEVEL {
            leaf_slots::<V>(page).iter().map(|s| s.key.get()).collect()
        } else {
            inner_slots(page).iter().map(|s| s.key.get()).collect()
        };
        if keys.windows(2).any(|w| w[0] >= w[1]) {
            return corrupt(CorruptKind::KeyOrder);
        }
        if keys
            .iter()
            .any(|&k| k < low || high.is_some_and(|h| k >= h))
        {
            return corrupt(CorruptKind::Separator);
        }
        if level == LEAF_LEVEL {
            return self.check_leaf::<V>(index, page);
        }
        // Lookups route by a run's highest possible key, which only reaches the run's leaf if
        // every separator is a run's lowest possible key (collision counter 1).
        if keys
            .iter()
            .any(|&k| !is_valid_key(k) || key_counter(k) != MIN_COUNTER)
        {
            return corrupt(CorruptKind::Separator);
        }
        for pos in 0..=keys.len() {
            let child_low = if pos == 0 { low } else { keys[pos - 1] };
            let child_high = keys.get(pos).copied().or(high);
            self.walk::<V>(inner::child(page, pos), level - 1, child_low, child_high)?;
        }
        Ok(())
    }

    fn check_leaf<V: Value>(&mut self, index: u32, page: &[u8]) -> Res<(), S>
    where
        F: FnMut(&V) -> bool,
    {
        let slots = leaf_slots::<V>(page);
        for (i, slot) in slots.iter().enumerate() {
            let key = slot.key.get();
            let name = leaf::name(page, slot);
            if !is_valid_key(key) {
                return corrupt(CorruptKind::KeyCounter);
            }
            if hash_prefix(self.hasher.hash(name)) != key_prefix(key) {
                return corrupt(CorruptKind::KeyHash);
            }
            if !(self.value_ok)(&slot.value) {
                return corrupt(CorruptKind::Value);
            }
            // Names are only ever compared within a run, so duplicates can only hide there.
            let mut earlier_in_run = slots[..i]
                .iter()
                .rev()
                .take_while(|s| key_prefix(s.key.get()) == key_prefix(key));
            if earlier_in_run.any(|s| leaf::name(page, s) == name) {
                return corrupt(CorruptKind::DuplicateName);
            }
        }
        check_overlap::<V>(page)?;

        let (Some(first), Some(last)) = (slots.first(), slots.last()) else {
            return corrupt(CorruptKind::EmptyPage);
        };
        self.names += slots.len() as u64;
        self.leaves.push(LeafInfo {
            index,
            link: format::link(page),
            first_prefix: key_prefix(first.key.get()),
            last_prefix: key_prefix(last.key.get()),
        });
        Ok(())
    }

    /// Checks the leaf chain and that every page was reached.
    fn finish(&self) -> Res<(), S> {
        for (i, leaf) in self.leaves.iter().enumerate() {
            let next = self.leaves.get(i + 1);
            if leaf.link != next.map_or(NO_LINK, |n| n.index) {
                return corrupt(CorruptKind::LeafChain);
            }
            if next.is_some_and(|n| n.first_prefix == leaf.last_prefix) {
                return corrupt(CorruptKind::RunSplit);
            }
        }
        if self.visited.iter().any(|&seen| !seen) {
            return corrupt(CorruptKind::Unreachable);
        }
        Ok(())
    }
}

/// Names must not overlap. (Every read already checks that `frag_bytes` matches the heap bytes
/// not covered by names, and that each name lies inside the heap.)
fn check_overlap<V: Value>(page: &[u8]) -> Result<(), CorruptKind> {
    let mut names: Vec<(usize, usize)> = leaf_slots::<V>(page)
        .iter()
        .map(|s| (s.offset.get() as usize, s.name_len as usize))
        .collect();
    names.sort_unstable();
    if names.windows(2).any(|w| w[0].0 + w[0].1 > w[1].0) {
        return Err(CorruptKind::NameOverlap);
    }
    Ok(())
}
