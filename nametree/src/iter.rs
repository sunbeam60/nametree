//! Readdir: an ordered walk along the leaf chain, resumable from any cookie.

use alloc::vec::Vec;
use core::iter::FusedIterator;

use crate::error::{CorruptKind, Error};
use crate::format::{self, copy_value, leaf_slots, Value, LEAF_LEVEL, NO_LINK};
use crate::hasher::NameHasher;
use crate::leaf;
use crate::store::PageStore;
use crate::tree::{Ctx, NameTree, Path};

/// One name yielded by [`Iter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry<V> {
    /// The name's bytes.
    pub name: Vec<u8>,
    /// The value stored under the name.
    pub value: V,
    /// Pass to [`NameTree::iter_from`] to resume after this entry. Opaque, positive and
    /// non-zero, using all 63 bits: valid as a `getdents` `d_off`, a `telldir` position or an
    /// NFS cookie.
    pub cookie: u64,
}

/// Iterator over a tree's names in cookie order. See [`NameTree::iter_from`].
///
/// Holds one leaf in memory at a time. Each item is a `Result`: after an error the iterator
/// ends.
pub struct Iter<'a, S, H, V, F = fn(&V) -> bool> {
    tree: &'a NameTree<S, H, V>,
    filter: F,
    state: State,
    /// The current leaf.
    leaf: Vec<u8>,
    /// Next slot to look at in `leaf`.
    next_slot: usize,
    /// Every key yielded or skipped so far is at most this; keys must keep rising.
    last_key: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Start,
    InLeaf,
    Done,
}

impl<'a, S: PageStore, H: NameHasher, V: Value, F: FnMut(&V) -> bool> Iter<'a, S, H, V, F> {
    pub(crate) fn new(tree: &'a NameTree<S, H, V>, cookie: u64, filter: F) -> Self {
        Self {
            tree,
            filter,
            state: State::Start,
            leaf: Vec::new(),
            next_slot: 0,
            last_key: cookie,
        }
    }

    /// Loads the leaf holding the first key above the cookie. `false` if the tree is empty.
    fn start(&mut self) -> Result<bool, Error<S::Error>> {
        let mut workspace = self.tree.workspace.borrow_mut();
        workspace.begin()?;
        if workspace.count() == 0 {
            workspace.discard();
            return Ok(false);
        }
        let first = self.last_key.saturating_add(1);
        self.leaf = workspace.take_buf();
        let mut path = Path::new();
        let mut ctx = Ctx::<S, V>::new(&mut workspace);
        let loaded = ctx
            .descend(first, &mut path)
            .and_then(|()| ctx.workspace.copy_into(path.leaf(), &mut self.leaf));
        workspace.discard();
        loaded?;
        self.next_slot = leaf::lower_bound::<V>(&self.leaf, first);
        Ok(true)
    }

    /// Moves to the next leaf along the chain. `false` at the end.
    fn next_leaf(&mut self) -> Result<bool, Error<S::Error>> {
        let next = format::link(&self.leaf);
        if next == NO_LINK {
            return Ok(false);
        }
        self.tree
            .workspace
            .borrow()
            .read_detached(next, &mut self.leaf)?;
        if format::level(&self.leaf) != LEAF_LEVEL {
            return Err(Error::Corrupt(CorruptKind::LeafChain));
        }
        self.next_slot = 0;
        Ok(true)
    }

    /// The next entry passing the filter in the current leaf, if any.
    fn next_in_leaf(&mut self) -> Option<Result<Entry<V>, Error<S::Error>>> {
        let slots = leaf_slots::<V>(&self.leaf);
        while let Some(slot) = slots.get(self.next_slot) {
            self.next_slot += 1;
            let key = slot.key.get();
            // Keys rise strictly along the chain; anything else means a corrupt chain, and
            // checking it also guarantees the walk ends.
            if key <= self.last_key {
                return Some(Err(Error::Corrupt(CorruptKind::KeyOrder)));
            }
            self.last_key = key;
            if (self.filter)(&slot.value) {
                return Some(Ok(Entry {
                    name: leaf::name(&self.leaf, slot).to_vec(),
                    value: copy_value(&slot.value),
                    cookie: key,
                }));
            }
        }
        None
    }

    fn advance(&mut self) -> Option<Result<Entry<V>, Error<S::Error>>> {
        loop {
            match self.state {
                State::Done => return None,
                State::Start => match self.start() {
                    Ok(true) => self.state = State::InLeaf,
                    Ok(false) => return None,
                    Err(e) => return Some(Err(e)),
                },
                State::InLeaf => {
                    if let Some(item) = self.next_in_leaf() {
                        return Some(item);
                    }
                    match self.next_leaf() {
                        Ok(true) => {}
                        Ok(false) => return None,
                        Err(e) => return Some(Err(e)),
                    }
                }
            }
        }
    }
}

impl<S: PageStore, H: NameHasher, V: Value, F: FnMut(&V) -> bool> Iterator
    for Iter<'_, S, H, V, F>
{
    type Item = Result<Entry<V>, Error<S::Error>>;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.advance();
        if !matches!(item, Some(Ok(_))) {
            self.state = State::Done;
        }
        item
    }
}

impl<S: PageStore, H: NameHasher, V: Value, F: FnMut(&V) -> bool> FusedIterator
    for Iter<'_, S, H, V, F>
{
}
