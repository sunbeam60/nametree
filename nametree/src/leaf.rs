//! Slotted leaf pages, as pure functions on a page buffer (no I/O).
//!
//! A leaf is the header, a sorted array of fixed-size slots growing up from the header, and name
//! bytes packed down from the end of the page, with free space in between.

use core::ops::Range;

use crate::format::{
    self, copy_value, header, header_mut, key_prefix, leaf_slot_size, leaf_slots, run_first,
    LeafSlot, Value, HEADER_SIZE, LEAF_LEVEL, MAX_COUNTER, MIN_COUNTER, NO_LINK,
};

/// Turns a fresh buffer into an empty leaf (`slot_count` 0, `heap_start` = page size).
pub(crate) fn init(page: &mut [u8]) {
    // Page sizes are checked against MAX_PAGE_SIZE (u32::MAX) when the tree is opened.
    let page_size = page.len() as u32;
    format::init_page(page, LEAF_LEVEL, NO_LINK, page_size);
}

/// Bytes one entry takes in a leaf: its slot plus its name.
pub(crate) const fn entry_size<V>(name_len: usize) -> usize {
    leaf_slot_size::<V>() + name_len
}

/// Key and size of the entry in `slot`, as used to choose split points.
pub(crate) fn slot_entry<V: Value>(slot: &LeafSlot<V>) -> (u64, usize) {
    (slot.key.get(), entry_size::<V>(slot.name_len as usize))
}

/// The name bytes of `slot`.
pub(crate) fn name<'p, V: Value>(page: &'p [u8], slot: &LeafSlot<V>) -> &'p [u8] {
    let offset = slot.offset.get() as usize;
    &page[offset..offset + slot.name_len as usize]
}

/// Bytes between the end of the slot array and the start of the name heap.
pub(crate) fn free_space<V: Value>(page: &[u8]) -> usize {
    let slots_end = HEADER_SIZE + leaf_slot_size::<V>() * format::slot_count(page);
    header(page).heap_start.get() as usize - slots_end
}

/// Free space plus bytes that compaction would recover from deleted names.
pub(crate) fn reclaimable<V: Value>(page: &[u8]) -> usize {
    free_space::<V>(page) + header(page).frag_bytes.get() as usize
}

/// Bytes holding live data: header, slots and live names.
pub(crate) fn used<V: Value>(page: &[u8]) -> usize {
    page.len() - reclaimable::<V>(page)
}

/// Bytes the live entries take, not counting the header.
pub(crate) fn entry_bytes<V: Value>(page: &[u8]) -> usize {
    used::<V>(page) - HEADER_SIZE
}

/// Index of the first slot whose key is at least `key`.
pub(crate) fn lower_bound<V: Value>(page: &[u8], key: u64) -> usize {
    leaf_slots::<V>(page).partition_point(|slot| slot.key.get() < key)
}

/// Slots holding the run of names whose keys start with `prefix` (almost always 0 or 1 slots).
pub(crate) fn run<V: Value>(page: &[u8], prefix: u64) -> Range<usize> {
    let slots = leaf_slots::<V>(page);
    let start = lower_bound::<V>(page, run_first(prefix));
    let len = slots[start..]
        .iter()
        .take_while(|slot| key_prefix(slot.key.get()) == prefix)
        .count();
    start..start + len
}

/// Finds `name` among the run with key prefix `prefix`. Reads name bytes only for run members
/// whose length matches.
pub(crate) fn find<V: Value>(page: &[u8], prefix: u64, name_bytes: &[u8]) -> Option<usize> {
    let slots = leaf_slots::<V>(page);
    run::<V>(page, prefix).find(|&i| {
        let slot = &slots[i];
        slot.name_len as usize == name_bytes.len() && name(page, slot) == name_bytes
    })
}

/// The lowest collision counter not used by the run in `slots`, if any.
pub(crate) fn free_counter<V: Value>(page: &[u8], run: Range<usize>) -> Option<u64> {
    let slots = leaf_slots::<V>(page);
    let used = slots[run].iter().fold(0u32, |mask, slot| {
        mask | 1 << format::key_counter(slot.key.get())
    });
    (MIN_COUNTER..=MAX_COUNTER).find(|&counter| used & (1 << counter) == 0)
}

/// A copy of the value in slot `index`.
pub(crate) fn value<V: Value>(page: &[u8], index: usize) -> V {
    copy_value(&leaf_slots::<V>(page)[index].value)
}

/// Overwrites the value in slot `index` and returns the old one. Never touches the heap.
pub(crate) fn replace_value<V: Value>(page: &mut [u8], index: usize, new: &V) -> V {
    let old = value::<V>(page, index);
    let at = HEADER_SIZE + index * leaf_slot_size::<V>();
    let slot: &mut LeafSlot<V> = format::slot_mut(page, at);
    slot.value.as_mut_bytes().copy_from_slice(new.as_bytes());
    old
}

/// Inserts an entry at slot position `pos`. The caller has checked that `free_space` is at
/// least `entry_size(name.len())` and that `key` belongs at `pos`.
pub(crate) fn insert_at<V: Value>(
    page: &mut [u8],
    pos: usize,
    key: u64,
    name_bytes: &[u8],
    value: &V,
) {
    let slot_size = leaf_slot_size::<V>();
    let count = format::slot_count(page);
    let len = name_bytes.len();
    let heap = header(page).heap_start.get() as usize - len;
    page[heap..heap + len].copy_from_slice(name_bytes);

    let at = HEADER_SIZE + pos * slot_size;
    page.copy_within(at..HEADER_SIZE + count * slot_size, at + slot_size);
    let slot: &mut LeafSlot<V> = format::slot_mut(page, at);
    slot.key.set(key);
    slot.offset.set(heap as u32);
    // Names are checked against MAX_NAME_LEN (u8::MAX) before they reach the tree.
    slot.name_len = len as u8;
    slot.value.as_mut_bytes().copy_from_slice(value.as_bytes());

    let header = header_mut(page);
    header.slot_count.set(count as u32 + 1);
    header.heap_start.set(heap as u32);
}

/// Appends an entry after the last slot. `key` must be above every key in the page.
fn push<V: Value>(page: &mut [u8], key: u64, name_bytes: &[u8], value: &V) {
    insert_at::<V>(page, format::slot_count(page), key, name_bytes, value);
}

/// Removes slot `pos`. Its name bytes become fragmentation, unless the name sits at
/// `heap_start`, in which case the heap simply shrinks.
pub(crate) fn remove_at<V: Value>(page: &mut [u8], pos: usize) {
    let slot_size = leaf_slot_size::<V>();
    let count = format::slot_count(page);
    let (offset, len) = {
        let slot = &leaf_slots::<V>(page)[pos];
        (slot.offset.get(), u32::from(slot.name_len))
    };
    let at = HEADER_SIZE + pos * slot_size;
    page.copy_within(at + slot_size..HEADER_SIZE + count * slot_size, at);

    let header = header_mut(page);
    header.slot_count.set(count as u32 - 1);
    if offset == header.heap_start.get() {
        header.heap_start.set(offset + len);
    } else {
        header.frag_bytes.set(header.frag_bytes.get() + len);
    }
}

/// Appends slots `range` of leaf `src` (and their names) to `dst`, in order. Building a page this
/// way packs names contiguously from the end of the page in slot order.
pub(crate) fn append_from<V: Value>(dst: &mut [u8], src: &[u8], range: Range<usize>) {
    for slot in &leaf_slots::<V>(src)[range] {
        push::<V>(dst, slot.key.get(), name(src, slot), &slot.value);
    }
}

/// Appends entries `range` of the concatenation of `a`'s slots and `b`'s slots to `dst`.
pub(crate) fn append_joined<V: Value>(dst: &mut [u8], a: &[u8], b: &[u8], range: Range<usize>) {
    let split = format::slot_count(a);
    append_from::<V>(dst, a, range.start.min(split)..range.end.min(split));
    append_from::<V>(
        dst,
        b,
        range.start.max(split) - split..range.end.max(split) - split,
    );
}

/// Rewrites all names contiguously from the end of the page in slot order, recovering every
/// fragmented byte. `scratch` is a page-sized working buffer.
pub(crate) fn compact<V: Value>(page: &mut [u8], scratch: &mut [u8]) {
    scratch.copy_from_slice(page);
    init(page);
    format::set_link(page, format::link(scratch));
    append_from::<V>(page, scratch, 0..format::slot_count(scratch));
}

/// Picks where to cut a sequence of `len` leaf entries into a left and a right page.
///
/// `entry(j)` gives the key and byte size of entry `j`. Returns the index of the first entry of
/// the right page: the cut nearest the middle by bytes that falls between two different key
/// prefixes (a run is never split across leaves) and leaves both pages within `capacity`
/// bytes. `None` if no such cut exists.
pub(crate) fn choose_cut(
    len: usize,
    capacity: usize,
    entry: impl Fn(usize) -> (u64, usize),
) -> Option<usize> {
    let total: usize = (0..len).map(|j| entry(j).1).sum();
    let mut left = 0;
    let mut best: Option<(usize, usize)> = None;
    for cut in 1..len {
        let (before_key, before_size) = entry(cut - 1);
        left += before_size;
        if left > capacity {
            break;
        }
        let right = total - left;
        if right > capacity || key_prefix(before_key) == key_prefix(entry(cut).0) {
            continue;
        }
        let imbalance = left.abs_diff(right);
        if best.is_none_or(|(_, best_imbalance)| imbalance < best_imbalance) {
            best = Some((cut, imbalance));
        }
    }
    best.map(|(cut, _)| cut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{level, link, run_last, slot_count};
    use alloc::vec;
    use alloc::vec::Vec;
    use zerocopy::byteorder::little_endian::U32;

    const PAGE: usize = 2048;
    type V = U32;

    /// Key for a test entry: prefix `p`, counter `c`.
    fn key(p: u64, c: u64) -> u64 {
        (p << 4) | c
    }

    fn entries(page: &[u8]) -> Vec<(u64, Vec<u8>, u32)> {
        leaf_slots::<V>(page)
            .iter()
            .map(|s| (s.key.get(), name(page, s).to_vec(), s.value.get()))
            .collect()
    }

    fn insert_sorted(page: &mut [u8], model: &mut Vec<(u64, Vec<u8>, u32)>, k: u64, n: &[u8]) {
        let pos = lower_bound::<V>(page, k);
        insert_at::<V>(page, pos, k, n, &U32::new(k as u32));
        let model_pos = model.partition_point(|e| e.0 < k);
        model.insert(model_pos, (k, n.to_vec(), k as u32));
    }

    #[test]
    fn empty_leaf_layout() {
        let mut page = vec![0xAA; PAGE];
        init(&mut page);
        assert_eq!(level(&page), LEAF_LEVEL);
        assert_eq!(slot_count(&page), 0);
        assert_eq!(link(&page), NO_LINK);
        assert_eq!(header(&page).heap_start.get() as usize, PAGE);
        assert_eq!(free_space::<V>(&page), PAGE - HEADER_SIZE);
        assert!(page[HEADER_SIZE..].iter().all(|&b| b == 0));
    }

    #[test]
    fn insert_remove_compact_match_sorted_model() {
        let mut page = vec![0; PAGE];
        let mut scratch = vec![0; PAGE];
        init(&mut page);
        let mut model = Vec::new();
        // Pseudo-random but deterministic key order.
        for i in 0..40u64 {
            let k = key((i * 7919) % 101 + 1, 1);
            if model.iter().any(|e: &(u64, Vec<u8>, u32)| e.0 == k) {
                continue;
            }
            let n = vec![b'a' + (i % 26) as u8; (i % 9 + 1) as usize];
            insert_sorted(&mut page, &mut model, k, &n);
            assert_eq!(entries(&page), model);
        }
        let before = reclaimable::<V>(&page);
        for i in (0..model.len()).rev().step_by(3) {
            remove_at::<V>(&mut page, i);
            model.remove(i);
            assert_eq!(entries(&page), model);
        }
        assert!(reclaimable::<V>(&page) > before);
        let frag = header(&page).frag_bytes.get();
        let reclaim = reclaimable::<V>(&page);
        compact::<V>(&mut page, &mut scratch);
        assert_eq!(entries(&page), model);
        assert_eq!(header(&page).frag_bytes.get(), 0);
        assert_eq!(reclaimable::<V>(&page), reclaim);
        assert_eq!(free_space::<V>(&page), reclaim);
        assert!(frag > 0);
        // Names are packed from the end of the page in slot order.
        let mut end = PAGE;
        for slot in leaf_slots::<V>(&page) {
            end -= slot.name_len as usize;
            assert_eq!(slot.offset.get() as usize, end);
        }
    }

    #[test]
    fn removing_the_lowest_name_shrinks_the_heap() {
        let mut page = vec![0; PAGE];
        init(&mut page);
        insert_at::<V>(&mut page, 0, key(1, 1), b"first", &U32::new(1));
        insert_at::<V>(&mut page, 1, key(2, 1), b"second", &U32::new(2));
        // "second" was written last, so it sits at heap_start.
        remove_at::<V>(&mut page, 1);
        assert_eq!(header(&page).frag_bytes.get(), 0);
        assert_eq!(header(&page).heap_start.get() as usize, PAGE - 5);
        remove_at::<V>(&mut page, 0);
        assert_eq!(header(&page).heap_start.get() as usize, PAGE);
    }

    #[test]
    fn runs_and_counters() {
        let mut page = vec![0; PAGE];
        init(&mut page);
        let mut model = Vec::new();
        insert_sorted(&mut page, &mut model, key(5, 1), b"x");
        insert_sorted(&mut page, &mut model, key(7, 1), b"y");
        insert_sorted(&mut page, &mut model, key(7, 2), b"zz");
        insert_sorted(&mut page, &mut model, key(7, 4), b"w");
        insert_sorted(&mut page, &mut model, key(9, 1), b"v");
        let run7 = run::<V>(&page, 7 << 4);
        assert_eq!(run7, 1..4);
        assert_eq!(free_counter::<V>(&page, run7), Some(3));
        assert_eq!(find::<V>(&page, 7 << 4, b"zz"), Some(2));
        assert_eq!(find::<V>(&page, 7 << 4, b"z"), None);
        assert_eq!(find::<V>(&page, 6 << 4, b"x"), None);
        assert_eq!(run::<V>(&page, 6 << 4), 1..1);
        assert_eq!(lower_bound::<V>(&page, run_last(7 << 4) + 1), 4);
        let old = replace_value::<V>(&mut page, 2, &U32::new(99));
        assert_eq!(old.get(), key(7, 2) as u32);
        assert_eq!(value::<V>(&page, 2).get(), 99);
    }

    #[test]
    fn full_run_has_no_free_counter() {
        let mut page = vec![0; PAGE];
        init(&mut page);
        let mut model = Vec::new();
        for c in MIN_COUNTER..=MAX_COUNTER {
            insert_sorted(&mut page, &mut model, key(3, c), &[c as u8]);
        }
        assert_eq!(free_counter::<V>(&page, run::<V>(&page, 3 << 4)), None);
    }

    #[test]
    fn cut_balances_bytes_and_respects_runs() {
        // Ten equal entries: the middle cut.
        let even = |j: usize| (key(j as u64 + 1, 1), 10);
        assert_eq!(choose_cut(10, 100, even), Some(5));
        // Entries 4..7 share a prefix, so the cut moves to the nearest run edge.
        let runs = |j: usize| {
            let p = if (4..7).contains(&j) { 4 } else { j as u64 };
            (key(p, j as u64 % 15 + 1), 10)
        };
        let cut = choose_cut(10, 100, runs).unwrap();
        assert!(cut == 4 || cut == 7);
        // Entries 3..8 share a prefix and capacity is 70 bytes: cutting after the run would
        // leave 80 bytes on the left, so the cut must go before it.
        let long_run = |j: usize| {
            let p = if (3..8).contains(&j) { 3 } else { j as u64 };
            (key(p, j as u64 % 15 + 1), 10)
        };
        assert_eq!(choose_cut(10, 70, long_run), Some(3));
        assert_eq!(choose_cut(4, 20, even), Some(2));
        // One run covering everything cannot be cut.
        let one_run = |j: usize| (key(1, j as u64 + 1), 10);
        assert_eq!(choose_cut(5, 100, one_run), None);
    }

    #[test]
    fn append_joined_concatenates() {
        let mut a = vec![0; PAGE];
        let mut b = vec![0; PAGE];
        let mut dst = vec![0; PAGE];
        init(&mut a);
        init(&mut b);
        init(&mut dst);
        for i in 0..3u64 {
            push::<V>(&mut a, key(i + 1, 1), b"a", &U32::new(i as u32));
            push::<V>(&mut b, key(i + 10, 1), b"b", &U32::new(i as u32));
        }
        append_joined::<V>(&mut dst, &a, &b, 2..5);
        let keys: Vec<u64> = entries(&dst).iter().map(|e| e.0).collect();
        assert_eq!(keys, vec![key(3, 1), key(10, 1), key(11, 1)]);
    }
}
