//! Steady-state reads allocate no page buffers: once a tree has served a call, lookups,
//! listings and checks reuse the page workspace's buffers.
//!
//! A separate test binary, because it installs a counting global allocator for the whole
//! process. It holds one test only, so no other test can allocate while it counts.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use nametree::{MemStore, NameTree, SipHasher13};

/// A page size no other allocation in these calls happens to share, so counting allocations of
/// exactly this many bytes counts page buffers and nothing else.
const PAGE: usize = 3000;

/// How many times each read is repeated: more than the workspace has spare buffers, so a call
/// that takes a buffer without giving it back runs the spares dry and shows up as allocations.
const ROUNDS: usize = 20;

/// Names in the test tree: enough for several leaves under one inner root.
const NAMES: usize = 400;

/// Counts allocations of exactly [`PAGE`] bytes.
struct CountingAlloc;

static PAGE_ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() == PAGE {
            PAGE_ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if layout.size() == PAGE {
            PAGE_ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn name(i: usize) -> Vec<u8> {
    format!("file-{i:05}").into_bytes()
}

#[test]
fn steady_state_reads_allocate_no_page_buffers() {
    let mut tree: NameTree<MemStore, SipHasher13, [u8; 8]> =
        NameTree::open(MemStore::new(PAGE), SipHasher13::new(*b"nametree-tests!!"))
            .expect("open empty tree");
    for i in 0..NAMES {
        tree.insert(&name(i), (i as u64).to_le_bytes())
            .expect("insert");
    }
    assert!(
        tree.check().expect("check").height >= 2,
        "want an inner root"
    );

    // Warm up: one of each read, so the workspace holds the buffers they need.
    tree.get(&name(0)).expect("get");
    assert_eq!(tree.iter().count(), NAMES);

    let before = PAGE_ALLOCS.load(Ordering::Relaxed);
    for round in 0..ROUNDS {
        assert!(tree.get(&name(round)).expect("get").is_some());
        assert_eq!(tree.iter().count(), NAMES);
        tree.check().expect("check");
    }
    let allocated = PAGE_ALLOCS.load(Ordering::Relaxed) - before;
    assert_eq!(allocated, 0, "page buffers allocated by steady-state reads");
}
