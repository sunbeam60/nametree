# Differences from yak-name-tree-design.md

This file tracks where the implementation diverges from, or fills gaps in, the design document. The design document itself is not changed without agreement.

## Format and algorithms

### Separators are the run's first possible key

The design says a separator is "the first key in `child`". The implementation uses `run_first(prefix)` (prefix | counter 1) of the right page's first key. When the first key's counter is 1, which is almost always, these are identical.

Why: a deleted counter can be reused. If a run's counter-1 name is deleted, the right page's first key becomes `prefix | 2`. A later name in the same run then gets counter 1 and lands below a separator of `prefix | 2`. Lookups still work, because they route by `run_last`, but "separator ≤ every key in its child" would no longer hold, and fsck could not check it. With `prefix | 1` the invariant holds exactly.

`check()` rejects any separator whose counter is not 1. A tree written to the design's literal rule would fail that check, so the design's inner-slot table ("Separator: the first key in `child`") should eventually say "the lowest possible key of the run that starts `child`". That needs agreement before the design document changes.

### CollisionOverflow also covers a run too large for one page

The collision rule forbids splitting a run across leaves. A run of up to 15 long names can need more than one page (for example 15 × 277 bytes > 4 KiB). An insert that would need such a split fails with `CollisionOverflow`, the same as a 16th name in a run, and writes nothing. Only a broken or test hasher can hit this.

### Path stack sized for 16 levels

The design suggests a fixed path stack sized for height 8. The implementation sizes it for `MAX_LEVEL` (16), so any tree the format can describe can be walked without a separate "too deep" error. The cost is about 200 bytes of stack.

### Merge with stream compaction can write up to 5 pages

The cost table says 3 to 4 pages. A fold writes the surviving sibling and the parent. Moving the last page into the freed hole then writes the moved page, its parent and, for a leaf, its previous leaf. That is 5 when none of them coincide, and fewer when they do. The page-count test asserts 2 to 5.

### Page initialisation zeroes the page

Building or compacting a page zeroes it first, so free space never carries stale bytes from another page that happened to share the buffer. This keeps page images deterministic, which the golden format tests rely on. Removing a name does not zero its bytes; they stay as fragmentation until the next compaction.

### Per-read checks

As in the design, key order and key counters are checked on every page read in debug builds only. Release builds rely on `check()` for these.

Two checks run on every read beyond the design's always-on list, because without them a corrupt page could cause a panic or a bad write instead of `Error::Corrupt`. Both were found by an adversarial review:

* **`frag_bytes` must be exact.** It must equal the heap bytes not covered by live names; the design lists this under fsck only. Free space, reclaimable space and used bytes are all derived from it. An overstated value made a full leaf look underfull, so a merge folded two full leaves into one page and ran off its end. On insert, compaction then "recovered" bytes that did not exist. The sum is taken in the slot loop the read already runs, so it costs nothing extra.
* **Pages reached off the descent path have their levels checked.** These are a merge's sibling and the pages walked to find the previous leaf. A child pointer aimed at a page of the wrong level made a merge read an inner page as a leaf.

Name offsets are also added with overflow checks, so a corrupt offset near `u32::MAX` cannot wrap on 32-bit targets.

## API

### Additions

* `Error::PageSize`: the store's page size is below `min_page_size::<V>()` or above `MAX_PAGE_SIZE`. The design lists no error for this.
* `iter()` (same as `iter_from(0)`) and `iter_filtered(cookie, filter)`, the design's type filter. Name bytes are read only for entries that pass the filter.
* `is_empty()`: cheap, because an empty tree is a zero-length store. Yak needs it before removing a directory.
* `check()` and `check_with(value_ok)`: the full fsck. `check_with` takes a value predicate, for rules such as Yak's "type is not 0", since the library knows nothing about the value. The predicate may read from the tree.
* `store_mut()`, `hasher()`, `page_size()`, `min_page_size::<V>()`, and `impl NameHasher for &H`.
* `MemStore::from_bytes` / `as_bytes` / `into_bytes`, and `FileStore::new` / `file` / `into_file`.
* `nametree::zerocopy` re-export, so hosts use the same zerocopy version for their value types.

### Behaviour the design leaves open

* `Iter` yields `Result<Entry<V>, Error<E>>` with an owned `Vec<u8>` name. The iterator holds one leaf in memory and ends after an error.
* `rename(a, a)` succeeds without writing if `a` exists. `NotFound` is checked before `Exists`.
* `NameTree` is not `Sync`, because it reuses page buffers through a `RefCell`. Use one tree per thread.
* The tree keeps no state between calls other than reusable buffers. It reads the store's page count at the start of every call, so a tree handle keeps working after a failed read. After a failed *write*, discard it as the design says.

## Module layout

* `hasher` holds `NameHasher` and the pinned SipHash-1-3. It is not in the design's module table.
* `tree` is split into `tree.rs` (API, descent, path), `tree/insert.rs` (splits) and `tree/remove.rs` (merges, stream compaction and root collapse).

## Not done here

* Build step 10's Yak `PageStore` impl and its integration belong in the Yak crate.
