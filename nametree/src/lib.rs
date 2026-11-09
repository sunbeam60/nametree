//! A directory's names as a B+ tree of fixed-size pages inside a generic page store.
//!
//! The crate owns the page format and the tree algorithms. The host, whatever supplies the
//! pages, owns atomicity, copy-on-write, store identity and the hasher seed.
//!
//! * **Minimal page churn**: most inserts, deletes and renames rewrite exactly one page.
//! * **Compact**: variable-length names packed into slotted pages, no per-name padding.
//! * **Fast**: O(log n) lookup by name, and an ordered leaf walk for readdir with an
//!   exactly-once 63-bit cookie.
//! * **Pluggable storage**: anything that can read, write and truncate whole pages
//!   ([`PageStore`]); [`MemStore`] and [`FileStore`] ship with the crate.
//! * **Linux-compatible names**: 1 to 255 arbitrary bytes, case-sensitive, byte-exact.
//!
//! The full design, including the on-disk format, is in `docs/design.md` in the repository.
//!
//! # Example
//!
//! ```
//! use nametree::{MemStore, NameTree, SipHasher13};
//! use nametree::zerocopy::byteorder::little_endian::U64;
//!
//! // The seed must be secret and stable for the life of the stored tree.
//! let hasher = SipHasher13::new([7; 16]);
//! let mut tree: NameTree<_, _, U64> = NameTree::open(MemStore::new(4096), hasher)?;
//!
//! tree.insert(b"notes.md", U64::new(43))?;
//! tree.insert(b"docs", U64::new(7))?;
//! assert_eq!(tree.get(b"notes.md")?, Some(U64::new(43)));
//!
//! tree.rename(b"notes.md", b"README.md")?;
//! let mut names: Vec<Vec<u8>> = tree.iter().map(|e| e.map(|e| e.name)).collect::<Result<_, _>>()?;
//! names.sort();
//! assert_eq!(names, [b"README.md".to_vec(), b"docs".to_vec()]);
//! # Ok::<(), nametree::Error<nametree::MemStoreError>>(())
//! ```
//!
//! # Atomicity
//!
//! One call can write several pages (a split writes three), and the tree does not journal.
//! Within a call every modified page is buffered and written once at the end, so a call that
//! fails before then writes nothing. A store error *while* writing can leave a half-finished
//! change, so the host must make each call atomic:
//!
//! * **A transactional host**: wrap each call (and each cross-directory rename pair) in a
//!   transaction.
//! * **[`MemStore`]**: atomic if a call that errors is treated as poisoning the tree.
//! * **[`FileStore`]**: not crash-safe; for tests and non-durable use only.
//!
//! After a store error, discard the tree and reopen it from the store the host rolled back.

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

mod check;
mod error;
mod format;
mod hasher;
mod inner;
mod iter;
mod leaf;
mod pager;
mod store;
mod tree;

pub use check::CheckReport;
pub use error::{CorruptKind, Error};
pub use format::{min_page_size, Value, MAX_NAME_LEN, MAX_PAGE_SIZE, MIN_PAGE_SIZE};
pub use hasher::{NameHasher, SipHasher13};
pub use iter::{Entry, Iter};
#[cfg(feature = "std")]
pub use store::FileStore;
pub use store::{MemStore, MemStoreError, PageStore};
pub use tree::NameTree;

/// The zerocopy version this crate's [`Value`] bound is written against, re-exported so hosts
/// can derive it on their value types without a version mismatch.
pub use zerocopy;
