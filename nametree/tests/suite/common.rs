//! Shared test fixtures: a Yak-like value type, test hashers, instrumented stores and the
//! model comparison every test runs after each step.

use std::cell::Cell;
use std::collections::BTreeMap;

use nametree::{
    CheckReport, MemStore, MemStoreError, NameHasher, NameTree, PageStore, SipHasher13,
};
use zerocopy::byteorder::little_endian::U64;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// Entry type values, as Yak uses them.
pub const FILE: u8 = 1;
pub const DIR: u8 = 2;

/// Yak's value: the child's stream ID and its entry type (9 bytes, so slots are 22 bytes).
#[repr(C)]
#[derive(
    FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned, Clone, Copy, Debug, PartialEq, Eq,
)]
pub struct Child {
    pub target: U64,
    pub kind: u8,
}

impl Child {
    pub fn new(target: u64, kind: u8) -> Self {
        Self {
            target: U64::new(target),
            kind,
        }
    }
}

/// What the tree should hold.
pub type Model = BTreeMap<Vec<u8>, Child>;

pub type Tree<S = MemStore, H = SipHasher13> = NameTree<S, H, Child>;

/// The seeded hasher used by most tests.
pub fn sip() -> SipHasher13 {
    SipHasher13::new(*b"nametree-tests!!")
}

/// An empty in-memory tree.
pub fn mem_tree(page_size: usize) -> Tree {
    Tree::open(MemStore::new(page_size), sip()).expect("open empty tree")
}

/// Sends every name to one of `buckets` hash prefixes, spread across the key space, to force
/// collisions.
#[derive(Clone, Copy)]
pub struct Bucketed {
    pub buckets: u64,
}

impl NameHasher for Bucketed {
    fn hash(&self, name: &[u8]) -> u64 {
        let bucket = sip().hash(name) % self.buckets;
        // Low 5 bits are dropped by the key layout; clear them so buckets stay distinct.
        (bucket * (u64::MAX / self.buckets)) & !0x1F
    }
}

/// Returns fixed hashes for a known set of names, for golden layouts.
pub struct MapHasher(pub &'static [(&'static [u8], u64)]);

impl NameHasher for MapHasher {
    fn hash(&self, name: &[u8]) -> u64 {
        self.0
            .iter()
            .find(|(n, _)| *n == name)
            .map(|&(_, h)| h)
            .expect("name not in the MapHasher table")
    }
}

/// A name of exactly `len` bytes, unique per `i`.
pub fn sized_name(i: usize, len: usize) -> Vec<u8> {
    let mut name = format!("{i}-").into_bytes();
    let fill = b'a' + (i % 26) as u8;
    name.resize(len.max(name.len()), fill);
    name
}

/// Counts store traffic, to check the cost model.
pub struct CountingStore<S> {
    pub inner: S,
    pub reads: Cell<u32>,
    pub writes: u32,
    pub truncates: u32,
}

impl<S> CountingStore<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            reads: Cell::new(0),
            writes: 0,
            truncates: 0,
        }
    }

    pub fn reset(&mut self) {
        self.reads.set(0);
        self.writes = 0;
        self.truncates = 0;
    }
}

impl<S: PageStore> PageStore for CountingStore<S> {
    type Error = S::Error;

    fn page_size(&self) -> usize {
        self.inner.page_size()
    }

    fn page_count(&self) -> Result<u32, S::Error> {
        self.inner.page_count()
    }

    fn read_page(&self, index: u32, buf: &mut [u8]) -> Result<(), S::Error> {
        self.reads.set(self.reads.get() + 1);
        self.inner.read_page(index, buf)
    }

    fn write_page(&mut self, index: u32, buf: &[u8]) -> Result<(), S::Error> {
        self.writes += 1;
        self.inner.write_page(index, buf)
    }

    fn truncate(&mut self, count: u32) -> Result<(), S::Error> {
        self.truncates += 1;
        self.inner.truncate(count)
    }
}

/// Error from [`FaultStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    Injected,
    Mem(MemStoreError),
}

/// Fails a chosen read, or a chosen write or truncate, on request.
pub struct FaultStore {
    pub inner: MemStore,
    fail_read: Option<u32>,
    fail_write: Option<u32>,
    reads: Cell<u32>,
    writes: u32,
}

impl FaultStore {
    pub fn new(inner: MemStore) -> Self {
        Self {
            inner,
            fail_read: None,
            fail_write: None,
            reads: Cell::new(0),
            writes: 0,
        }
    }

    /// Fails the `n`th read from now (counting from 0).
    pub fn fail_read_after(&mut self, n: u32) {
        self.fail_read = Some(self.reads.get() + n);
    }

    /// Fails the `n`th write or truncate from now (counting from 0).
    pub fn fail_write_after(&mut self, n: u32) {
        self.fail_write = Some(self.writes + n);
    }

    pub fn heal(&mut self) {
        self.fail_read = None;
        self.fail_write = None;
    }

    fn mutation(&mut self) -> Result<(), Fault> {
        let n = self.writes;
        self.writes += 1;
        if self.fail_write == Some(n) {
            Err(Fault::Injected)
        } else {
            Ok(())
        }
    }
}

impl PageStore for FaultStore {
    type Error = Fault;

    fn page_size(&self) -> usize {
        self.inner.page_size()
    }

    fn page_count(&self) -> Result<u32, Fault> {
        self.inner.page_count().map_err(Fault::Mem)
    }

    fn read_page(&self, index: u32, buf: &mut [u8]) -> Result<(), Fault> {
        let n = self.reads.get();
        self.reads.set(n + 1);
        if self.fail_read == Some(n) {
            return Err(Fault::Injected);
        }
        self.inner.read_page(index, buf).map_err(Fault::Mem)
    }

    fn write_page(&mut self, index: u32, buf: &[u8]) -> Result<(), Fault> {
        self.mutation()?;
        self.inner.write_page(index, buf).map_err(Fault::Mem)
    }

    fn truncate(&mut self, count: u32) -> Result<(), Fault> {
        self.mutation()?;
        self.inner.truncate(count).map_err(Fault::Mem)
    }
}

/// Every name in the tree, in cookie order, with cookies.
pub fn entries<S: PageStore, H: NameHasher>(tree: &Tree<S, H>) -> Vec<nametree::Entry<Child>>
where
    S::Error: std::fmt::Debug,
{
    tree.iter().collect::<Result<_, _>>().expect("iterate tree")
}

/// Checks the tree is consistent (full fsck, entry types valid) and holds exactly `model`.
pub fn assert_matches<S: PageStore, H: NameHasher>(tree: &Tree<S, H>, model: &Model) -> CheckReport
where
    S::Error: std::fmt::Debug,
{
    let report = tree
        .check_with(|v| v.kind == FILE || v.kind == DIR)
        .expect("tree passes check");
    let listed = entries(tree);
    assert!(
        listed.windows(2).all(|w| w[0].cookie < w[1].cookie),
        "cookies rise strictly"
    );
    let found: Model = listed.into_iter().map(|e| (e.name, e.value)).collect();
    assert_eq!(&found, model);
    assert_eq!(report.names, model.len() as u64);
    assert_eq!(tree.is_empty().expect("is_empty"), model.is_empty());
    report
}
