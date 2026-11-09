//! Remove, with merge and rebalance propagating up the path, root collapse, and stream
//! compaction (freed pages are filled by moving the last page, so the store stays dense).

use super::{Ctx, Path, Res};
use crate::error::{CorruptKind, Error};
use crate::format::{
    self, inner_slots, key_prefix, leaf_slots, run_first, Value, HEADER_SIZE, LEAF_LEVEL, ROOT,
    UNDERFLOW_DIVISOR,
};
use crate::store::PageStore;
use crate::{inner, leaf};

/// Two adjacent siblings and the parent slot separating them.
#[derive(Clone, Copy)]
struct Siblings {
    left: u32,
    right: u32,
    parent: u32,
    /// Index of the parent slot whose child is `right`.
    separator: usize,
}

impl<S: PageStore, V: Value> Ctx<'_, S, V> {
    /// Removes `name` (key prefix `prefix`), returning its value if it existed.
    pub(crate) fn remove(&mut self, prefix: u64, name: &[u8]) -> Res<Option<V>, S> {
        let mut path = Path::new();
        let Some(slot) = self.find(prefix, name, &mut path)? else {
            return Ok(None);
        };
        let leaf_index = path.leaf();
        let page = self.pager.write(leaf_index)?;
        let old = leaf::value::<V>(page, slot);
        leaf::remove_at::<V>(page, slot);
        let now_empty = format::slot_count(page) == 0;
        let used = leaf::used::<V>(page);

        if leaf_index == ROOT {
            if now_empty {
                // An empty directory is a zero-length store.
                self.pager.drop_all();
            }
        } else if used < self.underflow_limit() {
            self.rebalance(&mut path, LEAF_LEVEL)?;
        }
        Ok(Some(old))
    }

    /// Pages using fewer bytes than this (and not the root) are merged or rebalanced.
    fn underflow_limit(&self) -> usize {
        self.pager.page_size() / UNDERFLOW_DIVISOR
    }

    /// Fixes an underfull page at `level` of `path` by folding it into a sibling or borrowing
    /// from one, then repeats one level up while the parent underflows.
    fn rebalance(&mut self, path: &mut Path, level: u32) -> Res<(), S> {
        let mut level = level;
        loop {
            let parent = path.page(level + 1);
            let pos = path.pos(level + 1);
            let page = self.pager.read(parent)?;
            if format::slot_count(page) == 0 {
                return Err(Error::Corrupt(CorruptKind::EmptyPage));
            }
            // Pair with the left sibling; the leftmost child pairs with its right sibling.
            let pair = if pos > 0 {
                Siblings {
                    left: inner::child(page, pos - 1),
                    right: path.page(level),
                    parent,
                    separator: pos - 1,
                }
            } else {
                Siblings {
                    left: path.page(level),
                    right: inner::child(page, 1),
                    parent,
                    separator: 0,
                }
            };
            // The sibling comes from the parent's slots, not the descent path, so its level was
            // never checked. A corrupt pointer could also name the same page twice.
            if pair.left == pair.right {
                return Err(Error::Corrupt(CorruptKind::DuplicatePage));
            }
            self.expect_level(pair.left, level)?;
            self.expect_level(pair.right, level)?;
            let folded = if level == LEAF_LEVEL {
                self.merge_leaves(pair)?
            } else {
                self.merge_inner(pair, level)?
            };
            if !folded {
                return Ok(());
            }
            self.free_page(pair.right, path)?;

            let parent = path.page(level + 1);
            let page = self.pager.read(parent)?;
            if parent == ROOT {
                if format::slot_count(page) == 0 {
                    self.collapse_root(path)?;
                }
                return Ok(());
            }
            if inner::used(page) >= self.underflow_limit() {
                return Ok(());
            }
            level += 1;
        }
    }

    /// Folds the right leaf into the left if both fit in one page; otherwise moves entries
    /// across the boundary to even them out. Returns `true` if the right leaf was emptied (its
    /// separator is already removed from the parent; the caller frees the page).
    fn merge_leaves(&mut self, pair: Siblings) -> Res<bool, S> {
        let capacity = self.pager.page_size() - HEADER_SIZE;
        let mut left = self.pager.take_buf();
        let mut right = self.pager.take_buf();
        self.pager.copy_into(pair.left, &mut left)?;
        self.pager.copy_into(pair.right, &mut right)?;
        let folded = self.redistribute_leaves(pair, &left, &right, capacity);
        self.pager.give_buf(left);
        self.pager.give_buf(right);
        folded
    }

    fn redistribute_leaves(
        &mut self,
        pair: Siblings,
        left: &[u8],
        right: &[u8],
        capacity: usize,
    ) -> Res<bool, S> {
        let left_count = format::slot_count(left);
        let total = left_count + format::slot_count(right);

        if leaf::entry_bytes::<V>(left) + leaf::entry_bytes::<V>(right) <= capacity {
            let page = self.pager.write(pair.left)?;
            leaf::init(page);
            leaf::append_joined::<V>(page, left, right, 0..total);
            format::set_link(page, format::link(right));
            inner::remove_at(self.pager.write(pair.parent)?, pair.separator);
            return Ok(true);
        }

        let (left_slots, right_slots) = (leaf_slots::<V>(left), leaf_slots::<V>(right));
        let cut = leaf::choose_cut(total, capacity, |j| {
            if j < left_count {
                leaf::slot_entry(&left_slots[j])
            } else {
                leaf::slot_entry(&right_slots[j - left_count])
            }
        });
        // The current boundary is always a valid cut; only rewrite if a better one exists.
        let Some(cut) = cut.filter(|&cut| cut != left_count) else {
            return Ok(false);
        };
        let page = self.pager.write(pair.left)?;
        leaf::init(page);
        leaf::append_joined::<V>(page, left, right, 0..cut);
        format::set_link(page, pair.right);

        let page = self.pager.write(pair.right)?;
        leaf::init(page);
        leaf::append_joined::<V>(page, left, right, cut..total);
        format::set_link(page, format::link(right));
        let separator = run_first(key_prefix(leaf_slots::<V>(page)[0].key.get()));

        inner::set_key(self.pager.write(pair.parent)?, pair.separator, separator);
        Ok(false)
    }

    /// Inner-page form of [`merge_leaves`](Self::merge_leaves). The parent's separator comes
    /// down between the two pages' slots, paired with the right page's leftmost child.
    fn merge_inner(&mut self, pair: Siblings, level: u32) -> Res<bool, S> {
        let capacity = inner::capacity(self.pager.page_size());
        let separator = inner::key(self.pager.read(pair.parent)?, pair.separator);
        let mut left = self.pager.take_buf();
        let mut right = self.pager.take_buf();
        self.pager.copy_into(pair.left, &mut left)?;
        self.pager.copy_into(pair.right, &mut right)?;

        let left_count = format::slot_count(&left);
        let all = inner::Spliced::new(
            inner_slots(&left),
            (separator, format::link(&right)),
            inner_slots(&right),
        );
        let total = all.count();
        let link = format::link(&left);
        let folded = if total <= capacity {
            inner::build(
                self.pager.write(pair.left)?,
                level,
                link,
                all.slots(0..total),
            );
            inner::remove_at(self.pager.write(pair.parent)?, pair.separator);
            true
        } else {
            // Rotate through the parent: the middle slot's key becomes the new separator and
            // its child the right page's leftmost child.
            let mid = total / 2;
            if mid != left_count {
                let (up_key, up_child) = all.get(mid);
                inner::build(self.pager.write(pair.left)?, level, link, all.slots(0..mid));
                inner::build(
                    self.pager.write(pair.right)?,
                    level,
                    up_child,
                    all.slots(mid + 1..total),
                );
                inner::set_key(self.pager.write(pair.parent)?, pair.separator, up_key);
            }
            false
        };
        self.pager.give_buf(left);
        self.pager.give_buf(right);
        Ok(folded)
    }

    /// Frees page `index`, which nothing references any more, keeping the store dense: the
    /// last page moves into its place and its parent (and, for a leaf, the previous leaf) is
    /// repointed. Pages on `path` are renumbered to match.
    fn free_page(&mut self, index: u32, path: &mut Path) -> Res<(), S> {
        let last = self.pager.count() - 1;
        if index == last {
            self.pager.drop_last();
            return Ok(());
        }
        let (level, key) = self.first_key_below(last)?;
        let mut route = Path::new();
        if self.descend_to(key, level, &mut route)? != last {
            return Err(Error::Corrupt(CorruptKind::Unreachable));
        }
        let previous = if level == LEAF_LEVEL {
            self.previous_leaf(&route)?
        } else {
            None
        };
        // Repoint first, while every page still sees the old page count: a page read from the
        // store after the move would fail validation for pointing at the old last page.
        let parent = self.pager.write(route.page(level + 1))?;
        inner::set_child(parent, route.pos(level + 1), index);
        if let Some(previous) = previous {
            format::set_link(self.pager.write(previous)?, index);
        }
        self.pager.move_last(index)?;
        path.renumber(last, index);
        Ok(())
    }

    /// The level of page `index` and the first key of the leftmost leaf below it, which routes
    /// from the root back down through it.
    fn first_key_below(&mut self, index: u32) -> Res<(u32, u64), S> {
        let level = format::level(self.pager.read(index)?);
        let mut page_index = index;
        for at in (LEAF_LEVEL..level).rev() {
            page_index = format::link(self.pager.read(page_index)?);
            self.expect_level(page_index, at)?;
        }
        match leaf_slots::<V>(self.pager.read(page_index)?).first() {
            Some(slot) => Ok((level, slot.key.get())),
            None => Err(Error::Corrupt(CorruptKind::EmptyPage)),
        }
    }

    /// The leaf before the one at the bottom of `route`: step left at the deepest level where
    /// the route did not take the leftmost child, then go down the rightmost edge.
    fn previous_leaf(&mut self, route: &Path) -> Res<Option<u32>, S> {
        let Some(level) = (LEAF_LEVEL + 1..=route.height).find(|&level| route.pos(level) > 0)
        else {
            return Ok(None);
        };
        // These pages are off the route, so check their levels: on a corrupt tree, treating a
        // leaf as an inner page would pick an arbitrary page and overwrite its link.
        let mut index = inner::child(self.pager.read(route.page(level))?, route.pos(level) - 1);
        for at in (LEAF_LEVEL + 1..level).rev() {
            self.expect_level(index, at)?;
            let page = self.pager.read(index)?;
            index = inner::child(page, format::slot_count(page));
        }
        self.expect_level(index, LEAF_LEVEL)?;
        Ok(Some(index))
    }

    /// While page 0 is an inner page with a single child, copies that child into page 0 and
    /// frees it. The tree loses a level each time.
    fn collapse_root(&mut self, path: &mut Path) -> Res<(), S> {
        loop {
            let root = self.pager.read(ROOT)?;
            if format::level(root) == LEAF_LEVEL || format::slot_count(root) > 0 {
                return Ok(());
            }
            let child = format::link(root);
            self.pager.copy_page(child, ROOT)?;
            self.free_page(child, path)?;
        }
    }
}
