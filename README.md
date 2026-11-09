# nametree

[![CI](https://github.com/sunbeam60/nametree/actions/workflows/ci.yml/badge.svg)](https://github.com/sunbeam60/nametree/actions/workflows/ci.yml)

A Rust library that stores one directory's names as a B+ tree of fixed-size pages inside a pluggable page store.

The crate owns the page format and the tree algorithms. The host, whatever supplies the pages, owns atomicity, copy-on-write, store identity and the hasher seed.

Used by [Yak](https://github.com/sunbeam60/yak).

* **Minimal page churn**: most inserts, deletes and renames rewrite exactly one page, which matters when the host store is copy-on-write.
* **Compact**: variable-length names are packed into slotted pages with fixed-size slots and no per-name padding.
* **Fast lookup and listing**: O(log n) lookup by name, plus an ordered leaf walk for readdir with an exactly-once 63-bit cookie.
* **Pluggable storage**: anything that can read, write and truncate whole pages. An in-memory store and a file store ship with the crate.
* **Linux-compatible names**: 1 to 255 arbitrary bytes, case-sensitive and byte-exact, with no Unicode normalisation.
* **`no_std` + `alloc`** core. The only dependency is `zerocopy`.

```rust
use nametree::{MemStore, NameTree, SipHasher13};
use nametree::zerocopy::byteorder::little_endian::U64;

// The seed must be secret and must never change for the life of the stored tree.
let hasher = SipHasher13::new([7; 16]);
let mut tree: NameTree<_, _, U64> = NameTree::open(MemStore::new(4096), hasher)?;

tree.insert(b"notes.md", U64::new(43))?;
tree.insert(b"docs", U64::new(7))?;
assert_eq!(tree.get(b"notes.md")?, Some(U64::new(43)));

tree.rename(b"notes.md", b"README.md")?;
for entry in tree.iter() {
    let entry = entry?;
    println!("{:?} -> {} (cookie {})", entry.name, entry.value.get(), entry.cookie);
}
```

## How it works

* Each name's key is a `u64`: the top 59 bits of a keyed SipHash-1-3 of the name, plus a 4-bit collision counter. Keys are unique, so the key doubles as the readdir cookie.
* Leaves are slotted pages. Sorted fixed-size slots grow up from the header and name bytes pack down from the end of the page. Inner pages hold `(separator, child)` slots.
* Page 0 is always the root. The store stays dense: new pages are appended, and a freed page is filled by moving the last page into it. An empty directory is a zero-length store.
* Within one call, every modified page is buffered and written once at the end. A call that fails before then writes nothing.

The full design, including the on-disk format, is in [docs/design.md](docs/design.md). Divergences from it are tracked in [docs/differences.md](docs/differences.md).

## Atomicity

One call can write several pages (a split writes three), and the tree does not journal. The host must make each call atomic, for example by wrapping each call in a transaction. `MemStore` is atomic if a call that errors is treated as poisoning the tree. `FileStore` is not crash-safe and is meant for tests and non-durable use.

## Using it from another crate

```toml
[dependencies]
nametree = { git = "https://github.com/sunbeam60/nametree" }
# or, side by side on disk while developing:
# nametree = { path = "../nametree/nametree" }
```

Once a version is published to crates.io, use `nametree = "0.1"`.

## License

Licensed under either of

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
* MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
