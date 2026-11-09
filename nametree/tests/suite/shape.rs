//! Shape tests: 2 KiB pages filled with 255-byte names force deep trees, cascading splits,
//! inner merges and root collapse quickly.

use crate::common::{assert_matches, mem_tree, sized_name, Child, Model, FILE};

/// Names inserted: enough for three levels at seven names per leaf.
const NAMES: usize = 2500;

/// Run a full check every this many operations (a check reads the whole tree).
const CHECK_EVERY: usize = 97;

/// A prime coprime with [`NAMES`], used to delete in a scrambled order.
const SCRAMBLE: usize = 7919;

#[test]
fn deep_tree_grows_and_collapses_to_nothing() {
    let mut tree = mem_tree(2048);
    let mut model = Model::new();
    let mut heights = Vec::new();

    for i in 0..NAMES {
        let v = Child::new(i as u64, FILE);
        tree.insert(&sized_name(i, 255), v).expect("insert");
        model.insert(sized_name(i, 255), v);
        if i % CHECK_EVERY == 0 {
            heights.push(assert_matches(&tree, &model).height);
        }
    }
    let full = assert_matches(&tree, &model);
    assert!(full.height >= 3, "expected a deep tree: {full:?}");
    assert!(
        heights.windows(2).all(|w| w[0] <= w[1]),
        "trees only grow while inserting"
    );

    let mut shrinking = Vec::new();
    for k in 0..NAMES {
        let name = sized_name(k * SCRAMBLE % NAMES, 255);
        assert!(tree.remove(&name).expect("remove").is_some());
        model.remove(&name);
        if k % CHECK_EVERY == 0 {
            shrinking.push(assert_matches(&tree, &model).height);
        }
    }
    assert!(
        shrinking.windows(2).all(|w| w[0] >= w[1]),
        "trees only shrink while removing"
    );
    assert!(
        shrinking.contains(&2),
        "root collapsed through level 2: {shrinking:?}"
    );
    assert_matches(&tree, &model);
    assert!(tree.into_inner().as_bytes().is_empty());
}

#[test]
fn churn_keeps_the_store_dense() {
    // Alternate bursts of inserts and removes so merges, borrows and page moves interleave.
    let mut tree = mem_tree(2048);
    let mut model = Model::new();
    let mut next = 0;
    for round in 0..20 {
        for _ in 0..150 {
            let v = Child::new(next as u64, FILE);
            tree.insert(&sized_name(next, 60 + next % 190), v)
                .expect("insert");
            model.insert(sized_name(next, 60 + next % 190), v);
            next += 1;
        }
        let names: Vec<_> = model.keys().cloned().collect();
        for name in names.iter().skip(round % 3).step_by(2) {
            tree.remove(name).expect("remove");
            model.remove(name);
        }
        // The check fails on any page not reachable from the root, so passing it means the
        // store holds no freed pages.
        assert_matches(&tree, &model);
    }
}
