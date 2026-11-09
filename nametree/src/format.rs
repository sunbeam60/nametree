//! The on-disk page format: header, slot layouts, key layout and per-page validation.
//!
//! All integers are little-endian and accessed through zerocopy's unaligned types, so pages have
//! no alignment requirements anywhere.

use core::mem::size_of;

use zerocopy::byteorder::little_endian::{U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::error::CorruptKind;

/// A fixed-size value stored with each name. Any plain-old-data type with no alignment
/// requirement qualifies, such as a `#[repr(C)]` struct of zerocopy's little-endian integers.
pub trait Value: FromBytes + IntoBytes + KnownLayout + Immutable + Unaligned {}

impl<T: FromBytes + IntoBytes + KnownLayout + Immutable + Unaligned> Value for T {}

/// The first 20 bytes of every page.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
pub(crate) struct Header {
    /// Leaf: next leaf page, [`NO_LINK`] for none. Inner: leftmost child page.
    pub link: U32,
    /// Number of slots in the page.
    pub slot_count: U32,
    /// Leaf only: lowest name offset (the page size when empty). Written as 0 on inner pages.
    pub heap_start: U32,
    /// Leaf only: heap bytes lost to deleted names. Written as 0 on inner pages.
    pub frag_bytes: U32,
    /// [`LEAF_LEVEL`] for leaves, 2 to [`MAX_LEVEL`] for inner pages; anything else is invalid.
    pub level: U32,
}

/// One name in a leaf. Slots are kept sorted by key.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
pub(crate) struct LeafSlot<V> {
    /// Top 59 bits of the name hash, then a 4-bit collision counter (1 to 15). Bit 63 is 0.
    pub key: U64,
    /// Position of the name bytes within the page.
    pub offset: U32,
    /// 1 to 255.
    pub name_len: u8,
    /// The caller's value.
    pub value: V,
}

/// One separator in an inner page. Slots are kept sorted by key.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
pub(crate) struct InnerSlot {
    /// No key in `child`'s subtree is below this, and every key in the previous child is.
    pub key: U64,
    /// Page number of the child.
    pub child: U32,
}

/// Size of the page header in bytes.
pub(crate) const HEADER_SIZE: usize = size_of::<Header>();

/// Size of an inner slot in bytes.
pub(crate) const INNER_SLOT_SIZE: usize = size_of::<InnerSlot>();

/// Bytes in a leaf slot before the value: key, name offset and name length.
const LEAF_SLOT_BASE: usize = size_of::<LeafSlot<()>>();

/// Page 0 is always the root, so it never moves and nothing records its location.
pub(crate) const ROOT: u32 = 0;

/// A leaf `link` meaning "no next leaf". Safe because the root is never anyone's next leaf.
pub(crate) const NO_LINK: u32 = 0;

/// The `level` of a leaf page. An inner page's level is one more than its children's.
pub(crate) const LEAF_LEVEL: u32 = 1;

/// The highest valid `level`, and so the deepest tree the format can describe.
pub(crate) const MAX_LEVEL: u32 = 16;

/// Longest name the tree stores, in bytes (`name_len` is a `u8`).
pub const MAX_NAME_LEN: usize = u8::MAX as usize;

/// Smallest page size accepted for any value type. Small pages are useful in tests to force deep
/// trees; real hosts should use their atomic write unit (Yak: its block size, at least 4 KiB).
pub const MIN_PAGE_SIZE: usize = 2048;

/// Largest page size: name offsets and `heap_start` are `u32`, and `heap_start` can equal the
/// page size.
pub const MAX_PAGE_SIZE: usize = u32::MAX as usize;

/// Every page must hold at least this many maximum-size leaf entries, so that splitting a full
/// leaf always leaves room for the entry being inserted.
const MIN_ENTRIES_PER_PAGE: usize = 4;

/// A non-root page whose used bytes fall below `page_size / UNDERFLOW_DIVISOR` (25%) is merged
/// with or rebalanced against a sibling.
pub(crate) const UNDERFLOW_DIVISOR: usize = 4;

/// Bits of collision counter at the bottom of each key.
const COUNTER_BITS: u32 = 4;

/// Mask selecting a key's collision counter.
const COUNTER_MASK: u64 = (1 << COUNTER_BITS) - 1;

/// Lowest collision counter. Counters start at 1 so that key 0 never occurs and can mean
/// "start of directory" as a readdir cookie.
pub(crate) const MIN_COUNTER: u64 = 1;

/// Highest collision counter: a run holds at most 15 names.
pub(crate) const MAX_COUNTER: u64 = COUNTER_MASK;

/// Bit 63 of every key is 0, so a key fits a positive signed 64-bit offset.
const KEY_TOP_BIT: u64 = 1 << 63;

/// Size of a leaf slot for value type `V`: 13 bytes plus the value (22 for Yak).
pub(crate) const fn leaf_slot_size<V>() -> usize {
    size_of::<LeafSlot<V>>()
}

/// Smallest page size a tree storing values of type `V` accepts: [`MIN_PAGE_SIZE`], or more if
/// `V` is so large that four maximum-size entries would not fit.
pub const fn min_page_size<V>() -> usize {
    let four_entries =
        HEADER_SIZE + MIN_ENTRIES_PER_PAGE * (LEAF_SLOT_BASE + size_of::<V>() + MAX_NAME_LEN);
    if four_entries > MIN_PAGE_SIZE {
        four_entries
    } else {
        MIN_PAGE_SIZE
    }
}

/// The key prefix of a name hash: its top 59 bits, moved down to bits 62 to 4.
pub(crate) const fn hash_prefix(hash: u64) -> u64 {
    (hash >> (COUNTER_BITS + 1)) << COUNTER_BITS
}

/// The prefix part of a key (bits 62 to 4).
pub(crate) const fn key_prefix(key: u64) -> u64 {
    key & !COUNTER_MASK
}

/// The collision counter part of a key (bits 3 to 0).
pub(crate) const fn key_counter(key: u64) -> u64 {
    key & COUNTER_MASK
}

/// Lowest possible key in a run.
pub(crate) const fn run_first(prefix: u64) -> u64 {
    prefix | MIN_COUNTER
}

/// Highest possible key in a run. Routing by it always reaches the one leaf holding the run.
pub(crate) const fn run_last(prefix: u64) -> u64 {
    prefix | MAX_COUNTER
}

/// Views the start of `bytes` as a `T`. Page buffers are always at least [`MIN_PAGE_SIZE`]
/// long, so this only fails on a bug in this crate.
fn view<T: FromBytes + KnownLayout + Immutable + Unaligned>(bytes: &[u8]) -> &T {
    match T::ref_from_prefix(bytes) {
        Ok((value, _)) => value,
        Err(_) => unreachable!("page buffer shorter than the structure read from it"),
    }
}

/// Mutable form of [`view`].
fn view_mut<T: FromBytes + IntoBytes + KnownLayout + Unaligned>(bytes: &mut [u8]) -> &mut T {
    match T::mut_from_prefix(bytes) {
        Ok((value, _)) => value,
        Err(_) => unreachable!("page buffer shorter than the structure written to it"),
    }
}

pub(crate) fn header(page: &[u8]) -> &Header {
    view(page)
}

pub(crate) fn header_mut(page: &mut [u8]) -> &mut Header {
    view_mut(page)
}

pub(crate) fn level(page: &[u8]) -> u32 {
    header(page).level.get()
}

pub(crate) fn slot_count(page: &[u8]) -> usize {
    header(page).slot_count.get() as usize
}

pub(crate) fn link(page: &[u8]) -> u32 {
    header(page).link.get()
}

pub(crate) fn set_link(page: &mut [u8], link: u32) {
    header_mut(page).link.set(link);
}

/// Writes a fresh header and zeroes the rest of the page, so free space never carries stale
/// bytes from whatever the buffer held before.
pub(crate) fn init_page(page: &mut [u8], level: u32, link: u32, heap_start: u32) {
    page.fill(0);
    let header = header_mut(page);
    header.link.set(link);
    header.heap_start.set(heap_start);
    header.level.set(level);
}

/// The slot array of a leaf, sized by its `slot_count`.
pub(crate) fn leaf_slots<V: Value>(page: &[u8]) -> &[LeafSlot<V>] {
    let count = slot_count(page);
    match <[LeafSlot<V>]>::ref_from_prefix_with_elems(&page[HEADER_SIZE..], count) {
        Ok((slots, _)) => slots,
        Err(_) => unreachable!("leaf slot count was validated against the page size"),
    }
}

/// The slot array of an inner page, sized by its `slot_count`.
pub(crate) fn inner_slots(page: &[u8]) -> &[InnerSlot] {
    let count = slot_count(page);
    match <[InnerSlot]>::ref_from_prefix_with_elems(&page[HEADER_SIZE..], count) {
        Ok((slots, _)) => slots,
        Err(_) => unreachable!("inner slot count was validated against the page size"),
    }
}

/// One slot-sized window of a page, for writing a single slot.
pub(crate) fn slot_mut<T: FromBytes + IntoBytes + KnownLayout + Unaligned>(
    page: &mut [u8],
    offset: usize,
) -> &mut T {
    view_mut(&mut page[offset..])
}

/// Copies a stored value out of a page. `V` is plain bytes, so this is a byte copy.
pub(crate) fn copy_value<V: Value>(value: &V) -> V {
    let mut out = V::new_zeroed();
    out.as_mut_bytes().copy_from_slice(value.as_bytes());
    out
}

/// The always-on checks run on every page read from the store. `page_count` is the store's
/// current page count.
pub(crate) fn validate_page<V: Value>(page: &[u8], page_count: u32) -> Result<(), CorruptKind> {
    let header = header(page);
    let level = header.level.get();
    if !(LEAF_LEVEL..=MAX_LEVEL).contains(&level) {
        return Err(CorruptKind::Level);
    }
    let count = header.slot_count.get() as usize;
    if count == 0 {
        return Err(CorruptKind::EmptyPage);
    }
    let link = header.link.get();
    if level == LEAF_LEVEL {
        if link >= page_count {
            return Err(CorruptKind::PageIndex);
        }
        validate_leaf::<V>(page, count)
    } else {
        check_child(link, page_count)?;
        validate_inner(page, count, page_count)
    }
}

/// An inner page's child must exist and must not be the root.
fn check_child(child: u32, page_count: u32) -> Result<(), CorruptKind> {
    if child == ROOT || child >= page_count {
        Err(CorruptKind::PageIndex)
    } else {
        Ok(())
    }
}

/// Bytes taken by the header plus `count` slots of `slot_size`, if that fits a `usize`.
fn slots_end(count: usize, slot_size: usize) -> Option<usize> {
    count.checked_mul(slot_size)?.checked_add(HEADER_SIZE)
}

fn validate_leaf<V: Value>(page: &[u8], count: usize) -> Result<(), CorruptKind> {
    let page_size = page.len();
    let header = header(page);
    let heap_start = header.heap_start.get() as usize;
    let end = slots_end(count, leaf_slot_size::<V>()).ok_or(CorruptKind::SlotCount)?;
    if end > page_size {
        return Err(CorruptKind::SlotCount);
    }
    if heap_start < end || heap_start > page_size {
        return Err(CorruptKind::HeapStart);
    }
    let mut live = 0;
    for slot in leaf_slots::<V>(page) {
        let offset = slot.offset.get() as usize;
        let len = slot.name_len as usize;
        if len == 0 {
            return Err(CorruptKind::NameLength);
        }
        // Checked, because on 32-bit targets an offset near u32::MAX would wrap.
        if offset < heap_start || offset.checked_add(len).is_none_or(|e| e > page_size) {
            return Err(CorruptKind::NameOffset);
        }
        live += len;
    }
    // frag_bytes must account exactly for the heap bytes not covered by names. Free space,
    // reclaimable space and used bytes are all derived from it, so an overstated value would
    // make compaction or a merge try to fit more into a page than it holds.
    if (page_size - heap_start).checked_sub(live) != Some(header.frag_bytes.get() as usize) {
        return Err(CorruptKind::FragBytes);
    }
    #[cfg(debug_assertions)]
    {
        let slots = leaf_slots::<V>(page);
        if slots.iter().any(|s| !is_valid_key(s.key.get())) {
            return Err(CorruptKind::KeyCounter);
        }
        if slots.windows(2).any(|w| w[0].key.get() >= w[1].key.get()) {
            return Err(CorruptKind::KeyOrder);
        }
    }
    Ok(())
}

fn validate_inner(page: &[u8], count: usize, page_count: u32) -> Result<(), CorruptKind> {
    let end = slots_end(count, INNER_SLOT_SIZE).ok_or(CorruptKind::SlotCount)?;
    if end > page.len() {
        return Err(CorruptKind::SlotCount);
    }
    for slot in inner_slots(page) {
        check_child(slot.child.get(), page_count)?;
    }
    #[cfg(debug_assertions)]
    if inner_slots(page)
        .windows(2)
        .any(|w| w[0].key.get() >= w[1].key.get())
    {
        return Err(CorruptKind::KeyOrder);
    }
    Ok(())
}

/// A stored key has bit 63 clear and a non-zero collision counter.
pub(crate) const fn is_valid_key(key: u64) -> bool {
    key & KEY_TOP_BIT == 0 && key_counter(key) >= MIN_COUNTER
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Yak's value: a child stream ID and an entry type.
    #[repr(C)]
    #[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
    struct YakLike {
        target: U64,
        kind: u8,
    }

    #[test]
    fn sizes_match_the_design() {
        assert_eq!(HEADER_SIZE, 20);
        assert_eq!(INNER_SLOT_SIZE, 12);
        assert_eq!(LEAF_SLOT_BASE, 13);
        assert_eq!(leaf_slot_size::<YakLike>(), 22);
        assert_eq!(min_page_size::<YakLike>(), MIN_PAGE_SIZE);
        // A value so large that four maximum entries need more than 2 KiB.
        assert_eq!(
            min_page_size::<[u8; 300]>(),
            HEADER_SIZE + 4 * (13 + 300 + 255)
        );
    }

    #[test]
    fn header_round_trips_little_endian() {
        let mut page = [0u8; 64];
        init_page(&mut page, 2, 7, 0);
        header_mut(&mut page).slot_count.set(0x0102_0304);
        assert_eq!(&page[..4], &[7, 0, 0, 0]);
        assert_eq!(&page[4..8], &[4, 3, 2, 1]);
        assert_eq!(&page[16..20], &[2, 0, 0, 0]);
        assert_eq!(level(&page), 2);
        assert_eq!(link(&page), 7);
        assert_eq!(slot_count(&page), 0x0102_0304);
    }

    #[test]
    fn key_layout() {
        let hash = u64::MAX;
        let prefix = hash_prefix(hash);
        assert_eq!(prefix, 0x7FFF_FFFF_FFFF_FFF0);
        assert_eq!(run_first(prefix), 0x7FFF_FFFF_FFFF_FFF1);
        assert_eq!(run_last(prefix), 0x7FFF_FFFF_FFFF_FFFF);
        assert_eq!(key_prefix(run_last(prefix)), prefix);
        assert_eq!(key_counter(prefix | 5), 5);
        assert!(is_valid_key(run_first(0)));
        assert!(!is_valid_key(0));
        assert!(!is_valid_key(KEY_TOP_BIT | 1));
    }

    #[test]
    fn validation_rejects_zeroed_and_filled_pages() {
        let zeroed = [0u8; MIN_PAGE_SIZE];
        assert_eq!(
            validate_page::<YakLike>(&zeroed, 1),
            Err(CorruptKind::Level)
        );
        let filled = [0xFFu8; MIN_PAGE_SIZE];
        assert_eq!(
            validate_page::<YakLike>(&filled, 1),
            Err(CorruptKind::Level)
        );
    }
}
