//! Page-count tests: a counting store checks the cost model from the design. The common case
//! writes exactly one page; splits write three.

use nametree::{MemStore, PageStore};

use crate::common::{assert_matches, sip, sized_name, Child, CountingStore, Model, Tree, FILE};

type Counted = Tree<CountingStore<MemStore>>;

fn counted(page_size: usize) -> Counted {
    Tree::open(CountingStore::new(MemStore::new(page_size)), sip()).expect("open")
}

fn pages(tree: &mut Counted) -> u32 {
    tree.store_mut().inner.page_count().expect("page count")
}

/// Runs `op` and returns (pages written, pages truncated, page count before, page count after).
fn cost(tree: &mut Counted, op: impl FnOnce(&mut Counted)) -> (u32, u32, u32, u32) {
    let before = pages(tree);
    tree.store_mut().reset();
    op(tree);
    let store = tree.store_mut();
    let (writes, truncates) = (store.writes, store.truncates);
    (writes, truncates, before, pages(tree))
}

#[test]
fn common_case_writes_one_page() {
    let mut tree = counted(4096);
    for i in 0..30 {
        let name = sized_name(i, 16);
        let (writes, truncates, ..) = cost(&mut tree, |t| {
            t.insert(&name, Child::new(i as u64, FILE)).expect("insert");
        });
        assert_eq!((writes, truncates), (1, 0), "insert {i}");
    }
    let (writes, ..) = cost(&mut tree, |t| {
        t.insert(&sized_name(3, 16), Child::new(99, FILE))
            .expect("overwrite");
    });
    assert_eq!(writes, 1, "overwriting a value");
    let (writes, ..) = cost(&mut tree, |t| {
        t.rename(&sized_name(4, 16), b"renamed").expect("rename");
    });
    assert_eq!(writes, 1, "rename within one page");
    let (writes, ..) = cost(&mut tree, |t| {
        t.remove(&sized_name(5, 16)).expect("remove");
    });
    assert_eq!(writes, 1, "remove");
    let (writes, ..) = cost(&mut tree, |t| {
        t.get(&sized_name(6, 16)).expect("get");
        t.rename(b"missing", b"other").expect_err("not found");
    });
    assert_eq!(writes, 0, "reads and failed calls write nothing");
}

#[test]
fn splits_write_three_pages() {
    let mut tree = counted(4096);
    let mut model = Model::new();
    let (mut root_splits, mut leaf_splits) = (0, 0);
    for i in 0..3000 {
        let name = sized_name(i, 16);
        let v = Child::new(i as u64, FILE);
        let (writes, truncates, before, after) = cost(&mut tree, |t| {
            t.insert(&name, v).expect("insert");
        });
        model.insert(name, v);
        assert_eq!(truncates, 0);
        match (before, after) {
            // The first name creates page 0.
            (0, 1) => assert_eq!(writes, 1),
            // Root split: page 0 becomes an inner page over two new pages.
            (1, 3) => {
                root_splits += 1;
                assert_eq!(writes, 3, "root split at insert {i}");
            }
            (b, a) if a == b + 1 => {
                leaf_splits += 1;
                assert_eq!(writes, 3, "leaf split at insert {i}");
            }
            (b, a) if a == b => assert_eq!(writes, 1, "insert {i}"),
            _ => panic!("unexpected growth {before} -> {after} at insert {i}"),
        }
    }
    assert_eq!(root_splits, 1);
    assert!(leaf_splits > 20);
    assert_matches(&tree, &model);
}

#[test]
fn removes_write_one_page_until_a_merge() {
    let mut tree = counted(4096);
    for i in 0..2000 {
        tree.insert(&sized_name(i, 16), Child::new(i as u64, FILE))
            .expect("insert");
    }
    let mut merges = 0;
    for i in 0..2000 {
        let (writes, truncates, before, after) = cost(&mut tree, |t| {
            t.remove(&sized_name(i, 16)).expect("remove");
        });
        match (before, after) {
            // The last name: the store is truncated to nothing.
            (1, 0) => assert_eq!((writes, truncates), (0, 1), "remove {i}"),
            // No page freed: one page, or three for a borrow between siblings (both siblings
            // and the parent's separator).
            (b, a) if a == b => assert!(writes == 1 || writes == 3, "remove {i}: {writes}"),
            // One page freed by folding siblings: the survivor and the parent, plus moving the
            // last page into the hole (the moved page, its parent and its previous leaf).
            (b, a) if a + 1 == b => {
                merges += 1;
                assert!(
                    (2..=5).contains(&writes),
                    "merge at remove {i}: {writes} writes"
                );
                assert_eq!(truncates, 1);
            }
            // A merge that cascades or collapses the root frees more than one page.
            (b, a) => assert!(a < b && truncates == 1, "remove {i}: {b} -> {a} pages"),
        }
    }
    assert!(merges > 10);
}
