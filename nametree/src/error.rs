//! Error types returned by every fallible tree operation.

use core::fmt;

/// Everything that can go wrong in a tree operation. `E` is the page store's error type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error<E> {
    /// The page store failed. If this happens while a call is writing pages, the store may hold
    /// a half-finished change: discard the tree and reopen it from a store the host has rolled
    /// back (see the atomicity contract in the crate docs).
    Store(E),
    /// A page read from the store breaks the format.
    Corrupt(CorruptKind),
    /// The store's page size is below [`min_page_size`](crate::min_page_size) for this value
    /// type, or above [`MAX_PAGE_SIZE`](crate::MAX_PAGE_SIZE).
    PageSize,
    /// Names must be at least one byte long.
    NameEmpty,
    /// Names must be at most [`MAX_NAME_LEN`](crate::MAX_NAME_LEN) bytes long.
    NameTooLong,
    /// Too many names share one 59-bit hash prefix: a 16th name, or a group of names too large
    /// to fit in a single page. Only a broken or test hasher hits this in practice.
    CollisionOverflow,
    /// `rename`: the source name is not in the tree.
    NotFound,
    /// `rename`: the target name is already in the tree.
    Exists,
}

/// What [`Error::Corrupt`] found wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CorruptKind {
    /// A page's `level` is outside 1 to 16 (catches zeroed and 0xFF-filled pages).
    Level,
    /// A child's `level` is not its parent's level minus one.
    LevelMismatch,
    /// A page on disk holds no slots.
    EmptyPage,
    /// The slot array runs past the end of the page.
    SlotCount,
    /// A leaf's `heap_start` lies inside the slot array or past the end of the page.
    HeapStart,
    /// A leaf's `frag_bytes` does not match the heap bytes not covered by live names.
    FragBytes,
    /// A leaf slot has `name_len` 0.
    NameLength,
    /// A name lies outside the leaf's heap.
    NameOffset,
    /// Two names in one leaf overlap.
    NameOverlap,
    /// Two slots in one leaf hold the same name.
    DuplicateName,
    /// A child page number or leaf `link` is out of range, or a child points at the root.
    PageIndex,
    /// Keys are not strictly increasing, within a page or along the leaf chain.
    KeyOrder,
    /// A key has bit 63 set or a collision counter of 0.
    KeyCounter,
    /// A key's prefix does not match the hash of its name.
    KeyHash,
    /// A key lies outside the range its parent's separators give it, or a separator's
    /// collision counter is not 1.
    Separator,
    /// Names sharing a hash prefix are split across two leaves.
    RunSplit,
    /// The leaf `link` chain does not visit every leaf once, in key order, ending in 0.
    LeafChain,
    /// A page is not reachable from the root.
    Unreachable,
    /// A page is reachable from the root more than once.
    DuplicatePage,
    /// The caller's value check rejected a stored value.
    Value,
}

impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Store(e) => write!(f, "page store error: {e}"),
            Error::Corrupt(kind) => write!(f, "corrupt name tree: {kind}"),
            Error::PageSize => f.write_str("page size out of range for this value type"),
            Error::NameEmpty => f.write_str("name is empty"),
            Error::NameTooLong => f.write_str("name is longer than 255 bytes"),
            Error::CollisionOverflow => f.write_str("too many names share one hash prefix"),
            Error::NotFound => f.write_str("name not found"),
            Error::Exists => f.write_str("name already exists"),
        }
    }
}

impl fmt::Display for CorruptKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CorruptKind::Level => "page level out of range",
            CorruptKind::LevelMismatch => "child level does not match parent",
            CorruptKind::EmptyPage => "page has no slots",
            CorruptKind::SlotCount => "slot array runs past the page",
            CorruptKind::HeapStart => "heap start out of range",
            CorruptKind::FragBytes => "fragmented byte count is wrong",
            CorruptKind::NameLength => "name length is zero",
            CorruptKind::NameOffset => "name lies outside the heap",
            CorruptKind::NameOverlap => "names overlap",
            CorruptKind::DuplicateName => "name stored twice",
            CorruptKind::PageIndex => "page number out of range",
            CorruptKind::KeyOrder => "keys out of order",
            CorruptKind::KeyCounter => "key counter or top bit invalid",
            CorruptKind::KeyHash => "key does not match name hash",
            CorruptKind::Separator => "key outside separator range",
            CorruptKind::RunSplit => "hash run split across leaves",
            CorruptKind::LeafChain => "leaf chain broken",
            CorruptKind::Unreachable => "page unreachable",
            CorruptKind::DuplicatePage => "page reachable twice",
            CorruptKind::Value => "value rejected by check",
        })
    }
}

impl<E> From<CorruptKind> for Error<E> {
    fn from(kind: CorruptKind) -> Self {
        Error::Corrupt(kind)
    }
}

impl<E: core::error::Error + 'static> core::error::Error for Error<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Error::Store(e) => Some(e),
            _ => None,
        }
    }
}
