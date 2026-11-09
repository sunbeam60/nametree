//! Collision tests: a hasher that maps many names to a few 59-bit prefixes exercises counter
//! allocation and reuse, `CollisionOverflow`, and split points that must avoid splitting runs.

use nametree::{Error, MemStore};

use crate::common::{assert_matches, sized_name, Bucketed, Child, Model, Tree, FILE};

/// Bits of a cookie holding the collision counter.
const COUNTER_MASK: u64 = 0xF;

fn bucketed_tree(page_size: usize, buckets: u64) -> Tree<MemStore, Bucketed> {
    Tree::open(MemStore::new(page_size), Bucketed { buckets }).expect("open")
}

#[test]
fn counters_fill_reuse_and_overflow() {
    let mut tree = bucketed_tree(4096, 1);
    let mut model = Model::new();
    let name = |i: usize| format!("n{i}").into_bytes();
    for i in 0..15 {
        let v = Child::new(i as u64, FILE);
        assert_eq!(tree.insert(&name(i), v), Ok(None));
        model.insert(name(i), v);
    }
    // Every name shares one prefix: counters are handed out lowest first, 1 to 15.
    let counters: Vec<u64> = crate::common::entries(&tree)
        .iter()
        .map(|e| e.cookie & COUNTER_MASK)
        .collect();
    assert_eq!(counters, (1..=15).collect::<Vec<_>>());
    assert_eq!(
        tree.insert(b"one too many", Child::new(99, FILE)),
        Err(Error::CollisionOverflow)
    );

    // Overwriting a name in a full run needs no new counter.
    let v = Child::new(100, FILE);
    assert_eq!(tree.insert(&name(0), v), Ok(model.insert(name(0), v)));

    // A freed counter is reused by the next new name. n2 was third, so it held counter 3.
    tree.remove(&name(2)).expect("remove");
    model.remove(&name(2));
    let v = Child::new(7, FILE);
    tree.insert(b"fresh", v).expect("insert");
    model.insert(b"fresh".to_vec(), v);
    let fresh = crate::common::entries(&tree)
        .into_iter()
        .find(|e| e.name == b"fresh")
        .expect("listed");
    assert_eq!(fresh.cookie & COUNTER_MASK, 3);
    assert_matches(&tree, &model);
}

#[test]
fn runs_are_never_split_across_leaves() {
    // Few buckets and small pages: almost every split point lands inside a run and has to move.
    let mut tree = bucketed_tree(2048, 60);
    let mut model = Model::new();
    for i in 0..800 {
        let name = sized_name(i, 8 + i % 40);
        let v = Child::new(i as u64, FILE);
        match tree.insert(&name, v) {
            Ok(previous) => assert_eq!(previous, model.insert(name, v)),
            // A bucket that fills up (15 names) refuses more; nothing changes.
            Err(e) => assert_eq!(e, Error::CollisionOverflow),
        }
        if i % 25 == 0 {
            assert_matches(&tree, &model);
        }
    }
    let report = assert_matches(&tree, &model);
    assert!(report.leaves > 10, "enough splits to matter: {report:?}");

    let names: Vec<_> = model.keys().cloned().collect();
    for (k, name) in names.iter().enumerate().filter(|(k, _)| k % 2 == 0) {
        assert!(tree.remove(name).expect("remove").is_some());
        model.remove(name);
        if k % 25 == 0 {
            assert_matches(&tree, &model);
        }
    }
    assert_matches(&tree, &model);
}

#[test]
fn run_larger_than_a_page_overflows_without_writing() {
    // A 2 KiB page holds seven maximum-length entries. With one bucket, the eighth name would
    // need the run split across two leaves, which the format never allows.
    let mut tree = bucketed_tree(2048, 1);
    let mut model = Model::new();
    let mut i = 0;
    let error = loop {
        let name = sized_name(i, 255);
        let v = Child::new(i as u64, FILE);
        match tree.insert(&name, v) {
            Ok(None) => {
                model.insert(name, v);
                i += 1;
            }
            other => break other,
        }
    };
    assert_eq!(error, Err(Error::CollisionOverflow));
    assert_eq!(model.len(), 7);

    let before = tree.store_mut().as_bytes().to_vec();
    assert_eq!(
        tree.insert(&sized_name(1000, 255), Child::new(0, FILE)),
        Err(Error::CollisionOverflow)
    );
    assert_eq!(tree.store_mut().as_bytes(), &before[..]);
    assert_matches(&tree, &model);
}
