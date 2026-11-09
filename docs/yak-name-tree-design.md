# Yak Directory Name Tree - Design and Implementation Guide

Bjorn Toft Madsen · started 5 Nov 2026 · updated 9 Nov 2026

## Overview and goals

A standalone Rust library that stores one directory's names as a B+ tree of fixed-size pages inside a generic page store. Yak uses it with one Yak stream per directory, but the library knows nothing about Yak.

Goals:

- **Minimal page churn** - most inserts, deletes and renames rewrite exactly one page, because the host store (Yak) is copy-on-write.
- **Compact** - variable-length names packed into slotted pages, fixed-size slots, no per-name padding.
- **Fast lookup and listing** - O(log n) point lookup by name key, ordered leaf walk for readdir with an exactly-once 63-bit cookie.
- **Pluggable storage** - any store that can read, write and truncate whole pages (in-memory, file, Yak stream).
- **Linux-compatible names** - case-sensitive, byte-exact, no Unicode normalisation, so Linux trees copy in without collisions.

Design principles:

- The library owns the page format and tree algorithms. The host owns atomicity, copy-on-write, stream identity and the hasher seed.
- Pages are overwritten in place. The host's COW turns each page write into one Yak block copy, so the tree does no COW of its own.
- Every structural choice is judged by pages written per operation, not bytes moved.

## Storage model and keys

One directory is one page store - a sequence of pages of size B, with page N at byte offset N × B.

**Stream rules**

- B comes from the store, not the format. The library accepts any B of at least 2 KiB (room for four maximum-size entries), up to the u32 offset limit; small pages are useful in tests to force deep trees. **For atomicity, B should equal the host's atomic write unit** - for Yak, its block size, which is at least 4 KiB (smaller risks torn writes).
- Yak data streams start exactly on a block boundary, so one page = one Yak block.
- Page 0 is always the root. A zero-length stream is a valid empty directory: page 0 exists only while the directory holds at least one name, so an empty directory costs no blocks.
- The stream is always dense: its length is exactly pages in use × B. There is no free list - freed pages are filled by moving the last page (see Structure changes).
- All integers are little-endian, accessed through zerocopy's unaligned types (no alignment requirements anywhere).

**Keys**

Each name's **key** is a u64 built from a caller-supplied `NameHasher` (keyed SipHash-1-3 with a random per-file seed) over the raw name bytes:

| Bits | Contents |
| --- | --- |
| 63 | Always 0, so a key fits a positive signed 64-bit offset |
| 62 to 4 | Top 59 bits of the name hash (the prefix) |
| 3 to 0 | Collision counter, 1 to 15 |

- Keys are unique within a directory, and sort order is the key alone.
- Names sharing a 59-bit prefix form a **run** and get distinct counters (the lowest unused, almost always 1). A 16th name in one run fails with `CollisionOverflow` - about 10⁻¹⁸⁴ for a million-entry directory, so in practice only a broken hasher hits it.
- **Collision rule** - a run is never split across two leaves, so a lookup never crosses a page edge.
- Key 0 never occurs (counters start at 1), so 0 is free to mean "start of directory" as a readdir cookie.

**Names**

- Case-sensitive, byte-exact, no Unicode normalisation (matches Linux: any bytes except `/` and NUL).
- The library rejects empty names and names over 255 bytes. Rejecting `/` and NUL is the caller's path-level rule.
- Exporting to case-insensitive filesystems may collide (`Readme` vs `README`) - the exporter's problem to report, as with git.

## Page header

Every page starts with the same 20-byte header of five u32 fields.

| Offset | Size | Field | Meaning |
| --- | --- | --- | --- |
| 0 | 4 | link | Leaf: next leaf page, 0 = none. Inner: leftmost child page |
| 4 | 4 | slot_count | Number of slots in the page |
| 8 | 4 | heap_start | Leaf only: lowest name offset (B when empty) |
| 12 | 4 | frag_bytes | Leaf only: bytes lost to deleted names |
| 16 | 4 | level | 1 leaf, 2 to 16 inner; anything else invalid |

Notes:

- No magic, version, kind, flags or checksum. The host's format version covers the format, the host checksums blocks, and `level` doubles as page type.
- `level 0` being invalid means an all-zero page is rejected rather than read as an empty leaf.
- `link = 0` is safe as "none" because page 0 is always the root, so it is never a next leaf.
- On inner pages `heap_start` and `frag_bytes` are written as 0 and ignored.

## Leaf page

A leaf is a slotted page: header at offset 0, a sorted array of fixed-size slots growing up from offset 20, name bytes packed down from B, free space in between.

```
0        20                      86                   4079        B
+--------+-------+-------+-------+--------------------+--------+-----+------+
| Header | slot0 | slot1 | slot2 |     free space     |notes.md|a.txt| docs |
+--------+-------+-------+-------+--------------------+--------+-----+------+
             |       |       |                          ^         ^     ^
             |       |       +--------------------------+         |     |
             |       +--------------------------------------------|-----+
             +----------------------------------------------------+
        slots grow ->                                   <- names grow
```

Each slot holds the key, the name's offset and length, and the value. A lookup binary searches the slots and reads name bytes only to confirm a match.

**Leaf slot (22 bytes for Yak)** - kept sorted by key:

| Offset | Size | Field | Meaning |
| --- | --- | --- | --- |
| 0 | 8 | key | 59-bit hash prefix + 4-bit collision counter |
| 8 | 4 | offset | Name position within the page |
| 12 | 1 | name_len | 1 to 255 |
| 13 | 8 | target | Value: stream ID of the child (Yak's V) |
| 21 | 1 | type | Value: 0 invalid, 1 file, 2 dir, 3 symlink (Yak's V) |

**Name heap** - raw name bytes only, in no particular order. Each name's position and length come from its slot.

In the generic library, byte 12 is `name_len` and bytes 13 onward hold an opaque fixed-size value `V`, so a leaf slot is `13 + size_of::<V>()` bytes. `target` and `type` are Yak's choice of V.

Keeping the value and name length in the slot costs no extra space (the same bytes moved out of the entry) and keeps the common checks in the slot array: a run member with a different `name_len` is rejected without reading its name, a type filter (for example, directories only) reads names only for matches, value updates never touch the heap, and delete and compaction never read it.

**Derived quantities**

- Name size = `name_len`, read from the slot.
- Slot size S = `13 + size_of::<V>()` (22 for Yak).
- Free space = `heap_start - (20 + S × slot_count)`.
- Reclaimable space = free space + `frag_bytes`.
- A leaf is never empty on disk: removing the last name truncates the stream to zero length. (A leaf being built in memory starts at `slot_count = 0`, `heap_start = B`, `frag_bytes = 0`.)

**Worked example** - B = 4096, names inserted in the order docs, a.txt, notes.md (keys illustrative, each with counter 1):

| Region | Offset | Contents |
| --- | --- | --- |
| Header | 0 | link 0, slot_count 3, heap_start 4079, frag_bytes 0, level 1 |
| Slot 0 | 20 | key 0x0D15…1, offset 4087, len 5, target 42, file → a.txt |
| Slot 1 | 42 | key 0x2D18…1, offset 4092, len 4, target 7, dir → docs |
| Slot 2 | 64 | key 0x4E03…1, offset 4079, len 8, target 43, file → notes.md |
| Free | 86 to 4078 | 3,993 bytes |
| Name | 4079 | notes.md |
| Name | 4087 | a.txt |
| Name | 4092 | docs |

Slots are in key order; names are in insertion order from the end backwards. The two never need to line up.

## Inner page

An inner page is the header plus a sorted array of (separator, child) slots from offset 20 - no entries and no heap.

**Inner slot (12 bytes)**

| Offset | Size | Field | Meaning |
| --- | --- | --- | --- |
| 0 | 8 | key | Separator: the first key in `child` |
| 8 | 4 | child | Page number of the child |

**Routing rules**

- Keys below the first separator go to the header's `link` (leftmost child).
- Otherwise take the largest separator ≤ the key; its `child` covers keys up to the next separator.
- Children = `slot_count + 1`.
- Keys are unique, so separators are plain keys with no tie-breaking.
- A child's `level` must equal the parent's `level - 1`.

**Worked example** - the directory above after its root split (about 140 names):

| Page | level | link | Slots | Covers |
| --- | --- | --- | --- | --- |
| 0 (root) | 2 | 1 | 1: (0x4009…1, child 2) | everything |
| 1 | 1 | 2 | ~70 | 0x0000… to 0x3FF2… |
| 2 | 1 | 0 | ~71 | 0x4009… to 0x7FFF… |

Looking up docs (0x2D18…) finds no separator ≤ its run's highest possible key, so it follows `link` to page 1. notes.md (0x4E03…) routes via 0x4009…1 to page 2.

## Capacity

At B = 4 KiB a directory of 36,000 names is two levels deep and 12 million names is three. Figures assume 16-byte names (38 bytes per leaf name including its slot).

| Page size B | Names per leaf | Children per inner | Height 2 | Height 3 |
| --- | --- | --- | --- | --- |
| 4 KiB | 107 | 339 | 36 K | 12 M |
| 8 KiB | 215 | 681 | 146 K | 100 M |
| 16 KiB | 430 | 1,363 | 586 K | 800 M |
| 64 KiB | 1,724 | 5,459 | 9.4 M | 51 B |

Formulas: names per leaf = (B − 20) / (13 + size_of V + name_len); children per inner = (B − 20) / 12. Height 1 is a single leaf. Bigger B means a shallower tree but every insert rewrites a bigger page; 4 to 16 KiB is the sweet spot for directories.

## Library API

The library is generic over three things: the page store, the hasher and the fixed-size value stored per name.

**Storage trait**

```rust
pub trait PageStore {
    type Error;

    fn page_size(&self) -> usize;
    fn page_count(&self) -> Result<u32, Self::Error>;
    fn read_page(&self, index: u32, buf: &mut [u8]) -> Result<(), Self::Error>;
    /// index <= page_count; writing at page_count appends one page
    fn write_page(&mut self, index: u32, buf: &[u8]) -> Result<(), Self::Error>;
    fn truncate(&mut self, count: u32) -> Result<(), Self::Error>;
}
```

Shipped implementations: `MemStore` (a `Vec<u8>`) and `FileStore` (positioned reads and writes plus `set_len`). Yak implements `PageStore` for its stream handle inside the Yak crate.

**Hasher**

```rust
pub trait NameHasher {
    fn hash(&self, name: &[u8]) -> u64;
}
```

The caller owns the seed. Yak generates a random 128-bit seed when it creates a file, stores it in the L4 header, and builds the hasher from it on every open (a directory stream has no metadata of its own). Pin the hasher implementation and keep golden test vectors: every stored key depends on its output never changing.

**Tree**

```rust
pub struct NameTree<S: PageStore, H: NameHasher, V> { /* store, hasher, working buffers */ }

impl<S, H, V> NameTree<S, H, V>
where
    V: FromBytes + IntoBytes + KnownLayout + Immutable + Unaligned,
{
    /// A zero-length store is a valid empty directory.
    pub fn open(store: S, hasher: H) -> Result<Self, Error<S::Error>>;
    pub fn get(&self, name: &[u8]) -> Result<Option<V>, Error<S::Error>>;
    pub fn insert(&mut self, name: &[u8], value: V) -> Result<Option<V>, Error<S::Error>>;
    pub fn remove(&mut self, name: &[u8]) -> Result<Option<V>, Error<S::Error>>;
    pub fn rename(&mut self, from: &[u8], to: &[u8]) -> Result<(), Error<S::Error>>;
    pub fn iter_from(&self, cookie: u64) -> Iter<'_, S, H, V>;
    pub fn into_inner(self) -> S;
}
```

- `open` accepts a zero-length store as an empty directory; otherwise it checks page 0's level. There is no `create` - creating a directory is just creating an empty stream.
- `insert` returns the previous value if the name existed.
- A cross-directory rename is the caller's job: `remove` on one tree, `insert` on another, inside one host transaction.

**Errors**

```rust
pub enum Error<E> {
    Store(E),
    Corrupt(CorruptKind), // bad level, unsorted slots, offset out of page, ...
    NameEmpty,
    NameTooLong,
    CollisionOverflow,    // a 16th name shares a 59-bit hash prefix
    NotFound,             // rename source missing
    Exists,               // rename target present
}
```

**On-disk structs** (zerocopy, all unaligned little-endian)

```rust
use zerocopy::byteorder::little_endian::{U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
struct Header {
    link: U32,
    slot_count: U32,
    heap_start: U32,
    frag_bytes: U32,
    level: U32,
}

/// key = (top 59 bits of hash) << 4 | counter (1..=15); bit 63 is always 0
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
struct LeafSlot<V> {
    key: U64,
    offset: U32,  // name position in the page
    name_len: u8, // 1..=255
    value: V,     // Yak: YakEntry (9 bytes)
}

#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
struct InnerSlot { key: U64, child: U32 }

// Yak's value type
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
pub struct YakEntry { target: U64, kind: u8 }

const fn prefix(hash: u64) -> u64 { (hash >> 5) << 4 }   // bits 62..4
const fn run_first(hash: u64) -> u64 { prefix(hash) | 1 }
const fn run_last(hash: u64) -> u64 { prefix(hash) | 15 }
```

A leaf's heap holds only name bytes. The slot array is read as `<[LeafSlot<V>]>::ref_from_bytes(&page[20..20 + size_of::<LeafSlot<V>>() * n])`.

## Operations

Every operation descends from page 0, keeping the path (page numbers and slot indices) on a small stack for any structure change that follows.

**Lookup**

1. Hash the name. Its run's keys lie between `run_first` (prefix + 1) and `run_last` (prefix + 15).
2. At each inner page, binary search for the largest separator ≤ `run_last`; none → take `link`. Check the child's level. (Routing by the run's highest possible key always lands on the one leaf holding the run, even if its counter-1 entry was deleted.)
3. At the leaf, binary search for the first key ≥ `run_first`, then check each run member (almost always one): skip it if `name_len` differs, otherwise compare the name bytes.

**Insert**

1. On an empty tree (zero-length stream), append page 0 as a leaf holding just this entry and stop. Otherwise look up the run; if the name exists, overwrite the value in its slot and return the old value.
2. Otherwise take the lowest counter unused in the run (none free → `CollisionOverflow`); key = prefix + counter.
3. Needed = `name_len` + slot size. If free space ≥ needed: write the name at `heap_start - name_len`, insert the slot at its sorted position (shifting later slots up by one slot), update `heap_start` and `slot_count`.
4. Else if reclaimable ≥ needed: compact the page, then step 3.
5. Else split the leaf (see Structure changes), then insert into the correct half.

**Delete**

1. Look up; not found → `Ok(None)`.
2. Remove the slot (shift later slots down) and add its `name_len` to `frag_bytes`. If the name was at `heap_start`, advance `heap_start` instead. If this removes the root leaf's last slot, `truncate(0)` instead of writing the page.
3. If used bytes fall below about 25% of B and the page isn't the root, merge.

**Rename (same directory)**

Remove then insert, buffered as one operation so a page touched twice is written once. Usually a single page. Fails with `NotFound` / `Exists` before any write.

**Readdir**

- From page 0, follow `link` down to the leftmost leaf; walk leaves via `link` until 0. Order is key order. A type filter (for example, directories only) checks each slot's value and reads name bytes only for matches.
- `Iter` yields `(name, value, cookie)`, where the cookie is the entry's key.
- `iter_from(cookie)` resumes at the first key greater than the cookie; `iter_from(0)` starts at the beginning.
- **Exactly once** - keys are unique, so a listing resumed from any yielded cookie returns every entry that existed throughout exactly once, whatever changed in between. The cookie is a key, not a position, so splits, merges and page moves don't affect it. Entries added or removed meanwhile may or may not appear, as POSIX allows.
- Cookies are positive, non-zero 63-bit values, valid as a `getdents` `d_off`, a `telldir` position or an NFS cookie. A deleted entry's counter may be reused by a new name; that's an added entry, which POSIX allows to appear or not. Document the cookie as opaque and using all 63 bits, so callers don't steal bits from it.

## Structure changes

New pages are always appended to the end of the stream; freed pages are always filled by moving the last page, so the stream stays dense.

**Page compaction** (leaf, same page)

Rewrite all names contiguously from B downward in slot order, update each slot's offset, set `heap_start` to the new lowest offset and `frag_bytes` to 0. Cheap under COW - the page is being rewritten anyway - and it never parses the heap, since every name's position and length are in its slot.

**Leaf split**

1. Pick the split point at the middle by bytes; move it left or right to the nearest boundary between different 59-bit prefixes (collision rule).
2. Append a new page R; move the upper slots and their names into it (compacted).
3. R.link = L.link; L.link = R. Compact L.
4. Insert (R's first key, R) into the parent; if the parent is full, split it.

Pages written: L, R, parent.

**Inner split**

1. Append a new page R. Move the slots above the middle into R.
2. The middle slot's key is promoted to the grandparent; its child becomes R.link.
3. Insert (promoted key, R) into the grandparent, recursing upward.

**Root split** (page 0 full)

1. Append a new page C and copy page 0 into it.
2. Split C as a leaf or inner page, producing C and its new sibling S.
3. Rewrite page 0 as an inner page at C.level + 1 with link = C and one slot (S's first key, S).

The root never moves, so nothing records its location.

**Merge** (page below about 25% used, not the root)

1. Fold the page into its left sibling if they fit together; otherwise borrow slots from it to rebalance. No left sibling (leftmost child) → use the right sibling instead.
2. Remove the separator from the parent; for leaves fix the survivor's `link`.
3. The emptied page is freed (stream compaction below). If the parent now underflows, merge upward.

**Root collapse**

When page 0 is an inner page with `slot_count = 0`, copy its only child (page `link`) into page 0 and free the child. The tree loses a level.

**Stream compaction on free** (freeing page P in a stream of N pages)

1. If P = N − 1, truncate to N − 1 pages and stop.
2. Find page N − 1's parent and, if it's a leaf, its previous leaf, by searching from the root for N − 1's first key.
3. Copy page N − 1 into P. Repoint the parent's child entry (or `link`) and the previous leaf's `link` from N − 1 to P.
4. Truncate to N − 1 pages. No freed-page marker is needed: any stale pointer to the old last page is now beyond `page_count` and fails the bounds check.

## Cost model and atomicity

The common case - an insert, delete or same-page rename - writes exactly one page.

| Operation | Pages written | How often |
| --- | --- | --- |
| Insert, delete, same-page rename, page compaction | 1 | Almost every operation |
| Leaf split | 3 (L, R, parent) | About 1 in 50 inserts at 4 KiB |
| Root split | 3 (page 0, C, S) | Once per new tree level |
| Merge with stream compaction | 3 to 4 | Rare (lazy 25% threshold) |
| Cascading split or merge | +2 per extra level | Very rare |

The directory's version is the host stream's version, so the tree tracks none. Page counts are the tree's view: each page write also costs Yak its own block-map update and commit.

**Write buffering** - within one public call, each page the library modifies is held in a working buffer and written once when the call finishes (compaction and rename touch some pages twice). There is no read cache: the store is expected to provide one (Yak keeps blocks in a write-through memory cache).

**Atomicity contract** - one call can write several pages and the library does not journal. The caller must make each call atomic:

- **Yak** - wrap each call (and each cross-directory rename pair) in a Yak transaction.
- **MemStore** - atomic by construction if a call that errors is treated as poisoning the tree.
- **FileStore** - not crash-safe across multi-page calls; documented as for tests and non-durable use only.

After a store error mid-call, the in-memory tree handle must be discarded and the tree reopened from the store (which the host has rolled back).

## Integrity checks

Corruption is returned as `Error::Corrupt`, never a panic. These checks come free with the format.

**Always on** (cheap, per page read)

- `level` is between 1 and 16 inclusive (catches zeroed pages, 0xFF-filled pages and most garbage). A root leaf has at least one slot.
- A child's `level` equals its parent's `level - 1`; a leaf has `level 1`.
- `20 + S × slot_count ≤ heap_start ≤ B` on leaves.
- Every child page number and `link` is < `page_count`.
- Each slot's `offset + name_len ≤ B` and `offset ≥ heap_start`; `name_len` is 1 to 255.

**Debug builds and fsck**

- Keys strictly increasing within each page.
- Inner separators bracket their children's first and last keys.
- Names do not overlap; `frag_bytes` equals the heap bytes not covered by live names.
- The leaf `link` chain visits every leaf exactly once, in key order, ending in 0.
- The stream holds no unreachable pages.
- Each key's prefix equals the top 59 bits of `hasher.hash(name)`, its counter is 1 to 15, and no run spans two leaves.
- Yak's value: `type` is not 0.

## Implementation plan

Build bottom-up, so each layer is tested before the next depends on it. Dependencies: `zerocopy` only; `no_std` + `alloc` for the core, `std` feature for `FileStore`.

**Module layout**

| Module | Contents |
| --- | --- |
| `store` | `PageStore` trait, `MemStore`, `FileStore` (feature `std`) |
| `format` | `Header`, `LeafSlot`, `InnerSlot`, constants, level values, page validation |
| `leaf` | Slotted-page ops on a `&mut [u8]`: search, insert, remove, compact, split, merge |
| `inner` | Separator search, insert, remove, split, merge on a `&mut [u8]` |
| `pager` | Thin layer over a `PageStore`: working buffers for pages modified in the current call (each written once), append, move-last, truncate. No read cache - the store provides one |
| `tree` | `NameTree`: descent with path stack, the public API, split and merge propagation, root split and collapse |
| `iter` | Leaf-chain iterator and `iter_from(cookie)` |
| `check` | Full-tree fsck used by tests and debug builds |
| `error` | `Error<E>`, `CorruptKind` |

**Build order**

1. `format` and `store` with `MemStore`, plus unit tests that round-trip headers and slots.
2. `leaf` as pure functions on a page buffer - no I/O. Test insert, remove, compact and split against a sorted `Vec` model.
3. `inner` the same way.
4. `pager` with working page buffers and the move-last-page primitive (no read cache).
5. `tree`: open (including the zero-length empty case), get, insert, remove.
6. Leaf split and root split; then inner split and cascades.
7. Merge, root collapse and stream compaction.
8. `iter` and cookies.
9. `check` (fsck), wired into every test.
10. `FileStore`, then Yak's `PageStore` impl and integration in the Yak crate.

**Implementation notes**

- Keep the path stack as a fixed array sized for height 8 (far beyond any real directory).
- Insert slots with `copy_within` on the slot region; never re-serialise the whole page.
- Pick split points by bytes, not slot count, so pages of long names split evenly.
- Compare names only within a run; every other comparison uses the slot key.

## Testing strategy

The core test is a model check: random operation sequences run against both `NameTree<MemStore>` and a `BTreeMap<Vec<u8>, V>`, with fsck after every step.

- **Property tests** (proptest) - random insert, remove, rename and iterate sequences; assert identical results to the model and a clean fsck. Run at B = 4 KiB and 64 KiB, starting from a zero-length store and including runs that remove every name.
- **Collision tests** - a test hasher that maps many names to the same few 59-bit prefixes, to exercise the collision rule, counter allocation and reuse, `CollisionOverflow` at the 16th name, and split-point adjustment.
- **Shape tests** - small B (2 KiB) filled with 255-byte names, to force deep trees, cascading splits, merges and root collapse quickly.
- **Fault injection** - a `FaultStore` that fails the Nth write; verify the call returns `Error::Store` and that reopening the pre-call snapshot is consistent. This documents exactly which calls need host atomicity.
- **Page-count tests** - a `CountingStore` asserting the cost model: one page per common-case insert and delete, three per split.
- **Format tests** - golden byte images of the worked examples (single leaf, two-level tree) so the on-disk layout can't drift unnoticed.
- **Hasher tests** - golden vectors for the pinned SipHash-1-3 implementation, so an upgrade can't silently change keys.
- **Readdir tests** - iteration resumes correctly from any cookie while inserts and deletes happen between pages.

## Decision log

| Decision | Choice | Why |
| --- | --- | --- |
| Directory structure | One B+ tree per directory stream | Matches Yak's directory-is-a-stream model; version comes free |
| Rejected: fst | Not used | Immutable once built; directories change constantly |
| Rejected: size-class streams | Not used | Several structures dirtied per change; readdir spans every stream |
| Rejected: global tree (btrfs style) | Not used | Yak already has a stream per directory, and Yak transactions already span streams |
| Tree COW | None; overwrite in place | Yak's COW copies the block; no path copying needed |
| Read cache | None in the library | Yak keeps blocks in a write-through memory cache |
| Page size | One Yak block, B ≥ 4 KiB in Yak; the library accepts 2 KiB and up | Avoids torn writes; one page = one Yak block copy |
| Key | 63-bit key: 59-bit hash prefix + 4-bit collision counter | Unique keys: fixed-size slots, and the key doubles as an exactly-once readdir cookie that fits a positive off_t |
| Hasher | Keyed SipHash-1-3, random per-file seed | Crafted collisions infeasible without the seed. The 128-bit seed is generated randomly when the Yak file is created, stored in the L4 header and never changed |
| Collision overflow | Insert fails with `CollisionOverflow` | Needs 16 names sharing a 59-bit prefix: about 10⁻¹⁸⁴ for a million-entry directory; only buggy or test hashers hit it |
| Key in slot | Full key in the slot, none in the entry | Smaller than splitting it; search rarely touches the heap |
| Leaf slot contents | Key, offset, `name_len` and value in the slot; the heap holds only names | Same space as before; early rejection on collisions, type filtering, value updates, delete and compaction all work from the slot array alone |
| Inner entries | None; child in the slot | 12-byte (key, child) slots, 339 children at 4 KiB |
| Inner tie names | Never; collision rule | Keys are unique, so separators are plain keys |
| Header | 20 bytes, five u32 | Supports pages above 32 KiB; all fields aligned |
| Magic, version, kind, flags | Dropped | `level` covers type; host format version covers versioning |
| Checksum | Dropped | Yak checksums blocks |
| Freed-page marker | Dropped | A stale pointer to a truncated page fails the `page_count` bounds check |
| frag_bytes | Kept | Fixed cost to maintain; avoids a scan |
| Leaf sibling links | Next-leaf only | Safe without tree COW; split and merge touch no extra page |
| Free space | Dense stream, move last page; an empty directory is a zero-length stream | No free list, no leaked pages; empty directories cost no blocks |
| Names | Case-sensitive, byte-exact, no normalisation | Linux trees copy in without collisions |
| Storage | Generic `PageStore` trait | Library is independent of Yak |
| Endianness and access | Little-endian, zerocopy unaligned types | No alignment rules; portable format |

**Resolved questions**

- Cross-file copies: directory streams will never be copied byte-for-byte between Yak files, so a per-file seed is fine.
- Merge threshold: 25% of B for now; revisit only if profiling says otherwise.
- Small pages: the library accepts B below 4 KiB (useful in tests); production use should set B to the host's atomic write unit (Yak's block size).
- Readdir cookies: each key carries a 4-bit collision counter, so keys are unique and the key itself is an exactly-once cookie. This replaced a (hash, name) key, under which two names sharing a hash could be skipped or repeated on resume.

**Out of scope for now**

- Packing small directories (and small streams generally) so a tiny directory doesn't cost a full block.
- Bulk-load path for imports and redistribute-before-split; both are possible later without format changes.
