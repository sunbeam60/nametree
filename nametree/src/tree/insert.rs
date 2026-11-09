//! Insert, with leaf split, inner split and root split propagating up the path.

use core::cmp::Ordering;

use super::{Ctx, Path, Res};
use crate::error::{CorruptKind, Error};
use crate::format::{
    self, key_prefix, leaf_slots, run_first, Value, HEADER_SIZE, LEAF_LEVEL, MAX_LEVEL, ROOT,
};
use crate::store::PageStore;
use crate::{inner, leaf};

impl<S: PageStore, V: Value> Ctx<'_, S, V> {
    /// Stores `value` under `name` (key prefix `prefix`), returning the previous value.
    pub(crate) fn insert(&mut self, prefix: u64, name: &[u8], value: &V) -> Res<Option<V>, S> {
        if self.pager.count() == 0 {
            // An empty tree is a zero-length store: page 0 appears with the first name.
            let root = self.pager.append()?;
            let page = self.pager.write(root)?;
            leaf::init(page);
            leaf::insert_at::<V>(page, 0, run_first(prefix), name, value);
            return Ok(None);
        }
        let mut path = Path::new();
        if let Some(slot) = self.find(prefix, name, &mut path)? {
            let page = self.pager.write(path.leaf())?;
            return Ok(Some(leaf::replace_value::<V>(page, slot, value)));
        }

        let leaf_index = path.leaf();
        let page = self.pager.read(leaf_index)?;
        let counter = leaf::free_counter::<V>(page, leaf::run::<V>(page, prefix))
            .ok_or(Error::CollisionOverflow)?;
        let key = prefix | counter;
        let pos = leaf::lower_bound::<V>(page, key);
        let needed = leaf::entry_size::<V>(name.len());
        let free = leaf::free_space::<V>(page);
        let reclaimable = leaf::reclaimable::<V>(page);

        if needed > free && needed <= reclaimable {
            self.compact_leaf(leaf_index)?;
        }
        if needed <= reclaimable {
            let page = self.pager.write(leaf_index)?;
            leaf::insert_at::<V>(page, pos, key, name, value);
        } else {
            self.split_leaf(
                &mut path,
                NewEntry {
                    pos,
                    key,
                    name,
                    value,
                },
            )?;
        }
        Ok(None)
    }

    fn compact_leaf(&mut self, index: u32) -> Res<(), S> {
        let mut scratch = self.pager.take_buf();
        leaf::compact::<V>(self.pager.write(index)?, &mut scratch);
        self.pager.give_buf(scratch);
        Ok(())
    }

    /// Splits the full leaf at the bottom of `path` and inserts `entry` into the correct half.
    ///
    /// The cut is chosen with the new entry already in place, so both halves are guaranteed to
    /// fit. Pages written: the leaf, its new right sibling and the parent.
    fn split_leaf(&mut self, path: &mut Path, entry: NewEntry<'_, V>) -> Res<(), S> {
        let capacity = self.pager.page_size() - HEADER_SIZE;
        let new_size = leaf::entry_size::<V>(entry.name.len());
        let page = self.pager.read(path.leaf())?;
        let slots = leaf_slots::<V>(page);
        let count = slots.len();
        let cut = leaf::choose_cut(count + 1, capacity, |j| match j.cmp(&entry.pos) {
            Ordering::Less => leaf::slot_entry(&slots[j]),
            Ordering::Equal => (entry.key, new_size),
            Ordering::Greater => leaf::slot_entry(&slots[j - 1]),
        })
        .ok_or(Error::CollisionOverflow)?;

        if path.leaf() == ROOT {
            self.grow_root(path)?;
        }
        let left = path.leaf();
        let right = self.pager.append()?;
        let mut src = self.pager.take_buf();
        self.pager.copy_into(left, &mut src)?;
        // Old slots before `keep` stay left; the new entry goes right if the cut is at or
        // before its position.
        let goes_right = cut <= entry.pos;
        let keep = if goes_right { cut } else { cut - 1 };

        let page = self.pager.write(right)?;
        leaf::init(page);
        leaf::append_from::<V>(page, &src, keep..count);
        format::set_link(page, format::link(&src));
        if goes_right {
            leaf::insert_at::<V>(page, entry.pos - keep, entry.key, entry.name, entry.value);
        }
        let separator = run_first(key_prefix(leaf_slots::<V>(page)[0].key.get()));

        let page = self.pager.write(left)?;
        leaf::init(page);
        leaf::append_from::<V>(page, &src, 0..keep);
        format::set_link(page, right);
        if !goes_right {
            leaf::insert_at::<V>(page, entry.pos, entry.key, entry.name, entry.value);
        }
        self.pager.give_buf(src);

        self.insert_separator(path, LEAF_LEVEL + 1, separator, right)
    }

    /// Adds (`key`, `child`) to the inner page at `level` of `path`, just after the child the
    /// path went through, splitting full pages on the way up.
    fn insert_separator(
        &mut self,
        path: &mut Path,
        level: u32,
        key: u64,
        child: u32,
    ) -> Res<(), S> {
        let capacity = inner::capacity(self.pager.page_size());
        let (mut level, mut key, mut child) = (level, key, child);
        loop {
            let parent = path.page(level);
            // The split child sits at position `at`, so its new sibling becomes slot `at`.
            let at = path.pos(level);
            if format::slot_count(self.pager.read(parent)?) < capacity {
                inner::insert_at(self.pager.write(parent)?, at, key, child);
                return Ok(());
            }
            if parent == ROOT {
                self.grow_root(path)?;
            }
            let left = path.page(level);
            let right = self.pager.append()?;
            let mut src = self.pager.take_buf();
            self.pager.copy_into(left, &mut src)?;
            let slots = format::inner_slots(&src);
            let all = inner::Spliced::new(&slots[..at], (key, child), &slots[at..]);
            // The middle slot moves up: its key becomes the separator for the new right page,
            // and its child becomes the right page's leftmost child.
            let mid = all.count() / 2;
            let (up_key, up_child) = all.get(mid);
            let link = format::link(&src);
            inner::build(self.pager.write(left)?, level, link, all.slots(0..mid));
            inner::build(
                self.pager.write(right)?,
                level,
                up_child,
                all.slots(mid + 1..all.count()),
            );
            self.pager.give_buf(src);
            (level, key, child) = (level + 1, up_key, right);
        }
    }

    /// Moves the root's contents to a new page and makes page 0 an inner page one level up,
    /// whose only child is that page. The root never moves, so nothing records its location.
    fn grow_root(&mut self, path: &mut Path) -> Res<(), S> {
        let height = path.height;
        if height >= MAX_LEVEL {
            // The level field cannot describe a deeper tree. Unreachable with u32 page numbers.
            return Err(Error::Corrupt(CorruptKind::Level));
        }
        let copy = self.pager.append()?;
        self.pager.copy_page(ROOT, copy)?;
        inner::init(self.pager.write(ROOT)?, height + 1, copy);
        path.set(height, copy, path.pos(height));
        path.set(height + 1, ROOT, 0);
        path.height = height + 1;
        Ok(())
    }
}

/// An entry waiting to be inserted at slot position `pos` of a leaf.
struct NewEntry<'a, V> {
    pos: usize,
    key: u64,
    name: &'a [u8],
    value: &'a V,
}
