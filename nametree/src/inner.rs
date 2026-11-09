//! Inner pages, as pure functions on a page buffer (no I/O).
//!
//! An inner page is the header plus a sorted array of (separator, child) slots. The header's
//! `link` is the leftmost child, so a page with `n` slots has `n + 1` children. Children are
//! addressed by *position*: position 0 is `link`, position `i` is slot `i - 1`'s child.

use core::ops::Range;

use crate::format::{self, inner_slots, InnerSlot, HEADER_SIZE, INNER_SLOT_SIZE};

/// Turns a buffer into an empty inner page at `level` whose only child is `link`.
pub(crate) fn init(page: &mut [u8], level: u32, link: u32) {
    format::init_page(page, level, link, 0);
}

/// Slots an inner page of `page_size` bytes can hold.
pub(crate) const fn capacity(page_size: usize) -> usize {
    (page_size - HEADER_SIZE) / INNER_SLOT_SIZE
}

/// Bytes holding live data: header and slots.
pub(crate) fn used(page: &[u8]) -> usize {
    HEADER_SIZE + INNER_SLOT_SIZE * format::slot_count(page)
}

/// The child position and page covering `key`: the largest separator at or below `key`, or the
/// leftmost child if every separator is above it.
pub(crate) fn route(page: &[u8], key: u64) -> (usize, u32) {
    let pos = inner_slots(page).partition_point(|slot| slot.key.get() <= key);
    (pos, child(page, pos))
}

/// The child page at position `pos`.
pub(crate) fn child(page: &[u8], pos: usize) -> u32 {
    match pos {
        0 => format::link(page),
        _ => inner_slots(page)[pos - 1].child.get(),
    }
}

/// Repoints child position `pos` at page `child`.
pub(crate) fn set_child(page: &mut [u8], pos: usize, child: u32) {
    match pos {
        0 => format::set_link(page, child),
        _ => slot_mut(page, pos - 1).child.set(child),
    }
}

/// The separator key in slot `index`.
pub(crate) fn key(page: &[u8], index: usize) -> u64 {
    inner_slots(page)[index].key.get()
}

/// Replaces the separator key in slot `index`.
pub(crate) fn set_key(page: &mut [u8], index: usize, key: u64) {
    slot_mut(page, index).key.set(key);
}

fn slot_mut(page: &mut [u8], index: usize) -> &mut InnerSlot {
    format::slot_mut(page, HEADER_SIZE + index * INNER_SLOT_SIZE)
}

/// Inserts (`key`, `child`) as slot `index`. The caller has checked the page has room.
pub(crate) fn insert_at(page: &mut [u8], index: usize, key: u64, child: u32) {
    let count = format::slot_count(page);
    let at = HEADER_SIZE + index * INNER_SLOT_SIZE;
    page.copy_within(
        at..HEADER_SIZE + count * INNER_SLOT_SIZE,
        at + INNER_SLOT_SIZE,
    );
    let slot = slot_mut(page, index);
    slot.key.set(key);
    slot.child.set(child);
    format::header_mut(page).slot_count.set(count as u32 + 1);
}

/// Removes slot `index`.
pub(crate) fn remove_at(page: &mut [u8], index: usize) {
    let count = format::slot_count(page);
    let at = HEADER_SIZE + index * INNER_SLOT_SIZE;
    page.copy_within(
        at + INNER_SLOT_SIZE..HEADER_SIZE + count * INNER_SLOT_SIZE,
        at,
    );
    format::header_mut(page).slot_count.set(count as u32 - 1);
}

/// Rebuilds `page` as an inner page at `level` with leftmost child `link` and the given slots.
pub(crate) fn build(
    page: &mut [u8],
    level: u32,
    link: u32,
    slots: impl Iterator<Item = (u64, u32)>,
) {
    init(page, level, link);
    for (key, child) in slots {
        insert_at(page, format::slot_count(page), key, child);
    }
}

/// A sequence of inner slots made of `a`, one extra slot, then `b`, read without copying.
///
/// Inserting into a full page reads it as `a = slots[..i]`, the new slot, `b = slots[i..]`.
/// Merging two siblings reads them as the left page's slots, the parent's separator paired with
/// the right page's leftmost child, then the right page's slots.
pub(crate) struct Spliced<'a> {
    a: &'a [InnerSlot],
    middle: (u64, u32),
    b: &'a [InnerSlot],
}

impl<'a> Spliced<'a> {
    pub(crate) fn new(a: &'a [InnerSlot], middle: (u64, u32), b: &'a [InnerSlot]) -> Self {
        Self { a, middle, b }
    }

    /// Number of slots in the sequence.
    pub(crate) fn count(&self) -> usize {
        self.a.len() + 1 + self.b.len()
    }

    /// Slot `j` as (key, child).
    pub(crate) fn get(&self, j: usize) -> (u64, u32) {
        let pair = |slot: &InnerSlot| (slot.key.get(), slot.child.get());
        match j.cmp(&self.a.len()) {
            core::cmp::Ordering::Less => pair(&self.a[j]),
            core::cmp::Ordering::Equal => self.middle,
            core::cmp::Ordering::Greater => pair(&self.b[j - self.a.len() - 1]),
        }
    }

    /// Slots `range` as (key, child) pairs.
    pub(crate) fn slots(&self, range: Range<usize>) -> impl Iterator<Item = (u64, u32)> + '_ {
        range.map(|j| self.get(j))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{level, link, slot_count};
    use alloc::vec;
    use alloc::vec::Vec;

    const PAGE: usize = 2048;

    fn pairs(page: &[u8]) -> Vec<(u64, u32)> {
        inner_slots(page)
            .iter()
            .map(|s| (s.key.get(), s.child.get()))
            .collect()
    }

    #[test]
    fn insert_remove_and_route_match_sorted_model() {
        let mut page = vec![0; PAGE];
        init(&mut page, 2, 100);
        let mut model: Vec<(u64, u32)> = Vec::new();
        for i in 0..50u64 {
            let k = (i * 7919) % 1000 * 16 + 1;
            if model.iter().any(|e| e.0 == k) {
                continue;
            }
            let at = model.partition_point(|e| e.0 < k);
            insert_at(&mut page, at, k, i as u32 + 1);
            model.insert(at, (k, i as u32 + 1));
            assert_eq!(pairs(&page), model);
        }
        assert_eq!(level(&page), 2);
        assert_eq!(link(&page), 100);
        // Below every separator: the leftmost child.
        assert_eq!(route(&page, 0), (0, 100));
        // Exactly on a separator, and just past it: that separator's child.
        for (i, &(k, c)) in model.iter().enumerate() {
            assert_eq!(route(&page, k), (i + 1, c));
            assert_eq!(route(&page, k + 3), (i + 1, c));
        }
        remove_at(&mut page, 3);
        model.remove(3);
        assert_eq!(pairs(&page), model);
        set_child(&mut page, 0, 7);
        set_child(&mut page, 2, 8);
        set_key(&mut page, 1, model[1].0 + 1);
        assert_eq!(child(&page, 0), 7);
        assert_eq!(child(&page, 2), 8);
        assert_eq!(key(&page, 1), model[1].0 + 1);
        assert_eq!(
            used(&page),
            HEADER_SIZE + INNER_SLOT_SIZE * slot_count(&page)
        );
    }

    #[test]
    fn capacity_matches_design() {
        assert_eq!(capacity(4096), 339);
        assert_eq!(capacity(65536), 5459);
    }

    #[test]
    fn spliced_reads_three_parts_in_order() {
        let mut a = vec![0; PAGE];
        let mut b = vec![0; PAGE];
        build(&mut a, 2, 1, [(10, 2), (20, 3)].into_iter());
        build(&mut b, 2, 5, [(40, 6)].into_iter());
        let s = Spliced::new(inner_slots(&a), (30, 5), inner_slots(&b));
        assert_eq!(s.count(), 4);
        let all: Vec<_> = s.slots(0..4).collect();
        assert_eq!(all, vec![(10, 2), (20, 3), (30, 5), (40, 6)]);
    }
}
