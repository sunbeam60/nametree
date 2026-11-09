//! Fault injection: a store that fails the Nth read or write. Calls must return
//! `Error::Store`, a failed read must leave the store untouched, and the pre-call snapshot must
//! reopen cleanly. This documents exactly which calls need host atomicity: only a failure while
//! writing can leave a half-finished change.

use nametree::{Error, MemStore};

use crate::common::{assert_matches, sized_name, Child, Fault, FaultStore, Model, Tree, FILE};

const PAGE: usize = 2048;

/// A two-level tree and its model, as raw store bytes.
fn base() -> (Vec<u8>, Model) {
    let mut tree = crate::common::mem_tree(PAGE);
    let mut model = Model::new();
    for i in 0..60 {
        let v = Child::new(i as u64, FILE);
        tree.insert(&sized_name(i, 200), v).expect("insert");
        model.insert(sized_name(i, 200), v);
    }
    assert!(assert_matches(&tree, &model).height >= 2);
    (tree.into_inner().into_bytes(), model)
}

fn open(bytes: &[u8]) -> Tree<FaultStore> {
    Tree::open(
        FaultStore::new(MemStore::from_bytes(PAGE, bytes.to_vec())),
        crate::common::sip(),
    )
    .expect("open")
}

/// An operation that writes several pages: inserting names until a leaf splits.
fn grow(tree: &mut Tree<FaultStore>) -> Result<(), Error<Fault>> {
    for i in 1000..1010 {
        tree.insert(&sized_name(i, 200), Child::new(i as u64, FILE))?;
    }
    Ok(())
}

#[test]
fn write_faults_surface_as_store_errors() {
    let (bytes, model) = base();
    let mut n = 0;
    loop {
        let mut tree = open(&bytes);
        tree.store_mut().fail_write_after(n);
        match grow(&mut tree) {
            Ok(()) => break,
            Err(e) => assert_eq!(e, Error::Store(Fault::Injected)),
        }
        // The host rolls back to the snapshot taken before the call; it reopens cleanly.
        assert_matches(&open(&bytes), &model);
        n += 1;
    }
    assert!(
        n > 10,
        "the operation wrote several pages, each failed once: {n}"
    );
}

#[test]
fn read_faults_write_nothing_and_the_tree_carries_on() {
    let (bytes, mut model) = base();
    let name = sized_name(5000, 200);
    let v = Child::new(5000, FILE);
    let mut n = 0;
    loop {
        let mut tree = open(&bytes);
        tree.store_mut().fail_read_after(n);
        match tree.insert(&name, v) {
            Ok(_) => break,
            Err(e) => assert_eq!(e, Error::Store(Fault::Injected)),
        }
        // Writes are buffered until the call succeeds, so nothing reached the store.
        assert_eq!(tree.store_mut().inner.as_bytes(), &bytes[..]);
        // The tree keeps no state between calls, so it works again once the store does.
        tree.store_mut().heal();
        assert_eq!(tree.get(&name), Ok(None));
        n += 1;
    }
    assert!(n >= 2, "the insert read at least the root and a leaf: {n}");
    model.insert(name, v);
    let mut tree = open(&bytes);
    tree.insert(&sized_name(5000, 200), v).expect("insert");
    assert_matches(&tree, &model);
}
