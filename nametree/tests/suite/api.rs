//! Public API behaviour: argument checks, return values, corruption reporting and FileStore.

use std::fs::OpenOptions;

use nametree::{min_page_size, CorruptKind, Error, FileStore, MemStore, NameTree, PageStore};
use zerocopy::byteorder::little_endian::U64;

use crate::common::{assert_matches, mem_tree, sip, sized_name, Child, Model, Tree, DIR, FILE};

#[test]
fn names_must_be_1_to_255_bytes() {
    let mut tree = mem_tree(4096);
    let long = vec![b'x'; 256];
    let v = Child::new(1, FILE);
    assert_eq!(tree.insert(b"", v), Err(Error::NameEmpty));
    assert_eq!(tree.insert(&long, v), Err(Error::NameTooLong));
    assert_eq!(tree.get(b""), Err(Error::NameEmpty));
    assert_eq!(tree.remove(&long), Err(Error::NameTooLong));
    assert_eq!(tree.rename(b"a", b""), Err(Error::NameEmpty));
    assert_eq!(tree.insert(&long[..255], v), Ok(None));
    assert_eq!(tree.get(&long[..255]), Ok(Some(v)));
}

#[test]
fn names_are_byte_exact() {
    let mut tree = mem_tree(4096);
    let mut model = Model::new();
    // Case-sensitive, no normalisation, any bytes: the caller rejects '/' and NUL, not us.
    let names: [&[u8]; 6] = [
        b"Readme",
        b"README",
        b"readme",
        b"a/b",
        b"\0",
        "caf\u{e9}".as_bytes(),
    ];
    for (i, name) in names.iter().enumerate() {
        let v = Child::new(i as u64, FILE);
        assert_eq!(tree.insert(name, v), Ok(None));
        model.insert(name.to_vec(), v);
    }
    // The decomposed form of "café" is a different name.
    assert_eq!(tree.get("cafe\u{301}".as_bytes()), Ok(None));
    assert_matches(&tree, &model);
}

#[test]
fn page_size_limits() {
    assert!(matches!(
        Tree::open(MemStore::new(1024), sip()),
        Err(Error::PageSize)
    ));
    assert!(Tree::open(MemStore::new(2048), sip()).is_ok());
    // A big value raises the minimum: four maximum-size entries must fit.
    type Big = [u8; 400];
    assert!(min_page_size::<Big>() > 2048);
    assert!(matches!(
        NameTree::<_, _, Big>::open(MemStore::new(2048), sip()),
        Err(Error::PageSize)
    ));
    assert!(NameTree::<_, _, Big>::open(MemStore::new(min_page_size::<Big>()), sip()).is_ok());
}

#[test]
fn insert_and_remove_return_previous_values() {
    let mut tree: NameTree<_, _, U64> = NameTree::open(MemStore::new(4096), sip()).expect("open");
    assert_eq!(tree.insert(b"a", U64::new(1)), Ok(None));
    assert_eq!(tree.insert(b"a", U64::new(2)), Ok(Some(U64::new(1))));
    assert_eq!(tree.get(b"a"), Ok(Some(U64::new(2))));
    assert_eq!(tree.remove(b"a"), Ok(Some(U64::new(2))));
    assert_eq!(tree.remove(b"a"), Ok(None));
    assert_eq!(tree.get(b"a"), Ok(None));
}

#[test]
fn rename_rules() {
    let mut tree = mem_tree(4096);
    let mut model = Model::new();
    for (name, kind) in [(&b"a"[..], FILE), (b"b", DIR)] {
        tree.insert(name, Child::new(1, kind)).expect("insert");
        model.insert(name.to_vec(), Child::new(1, kind));
    }
    assert_eq!(tree.rename(b"missing", b"c"), Err(Error::NotFound));
    assert_eq!(tree.rename(b"missing", b"a"), Err(Error::NotFound));
    assert_eq!(tree.rename(b"a", b"b"), Err(Error::Exists));
    assert_eq!(tree.rename(b"a", b"a"), Ok(()));
    assert_eq!(tree.rename(b"b", b"c"), Ok(()));
    let v = model.remove(&b"b"[..]).expect("present");
    model.insert(b"c".to_vec(), v);
    assert_matches(&tree, &model);
}

#[test]
fn an_empty_directory_is_a_zero_length_store() {
    let mut tree = mem_tree(4096);
    assert_eq!(tree.is_empty(), Ok(true));
    tree.insert(b"only", Child::new(1, FILE)).expect("insert");
    assert_eq!(tree.is_empty(), Ok(false));
    assert_eq!(tree.store_mut().as_bytes().len(), 4096);
    tree.remove(b"only").expect("remove");
    assert_eq!(tree.is_empty(), Ok(true));
    assert!(tree.store_mut().as_bytes().is_empty());
}

#[test]
fn corrupt_pages_are_errors_not_panics() {
    let zeroed = MemStore::from_bytes(4096, vec![0; 4096]);
    assert!(matches!(
        Tree::open(zeroed, sip()),
        Err(Error::Corrupt(CorruptKind::Level))
    ));
    let filled = MemStore::from_bytes(4096, vec![0xFF; 4096]);
    assert!(matches!(
        Tree::open(filled, sip()),
        Err(Error::Corrupt(CorruptKind::Level))
    ));

    // A two-level tree whose root points past the end of the store.
    let mut tree = mem_tree(2048);
    for i in 0..40 {
        tree.insert(&sized_name(i, 100), Child::new(i as u64, FILE))
            .expect("insert");
    }
    let mut bytes = tree.into_inner().into_bytes();
    bytes[0..4].copy_from_slice(&999u32.to_le_bytes());
    let tree = Tree::open(MemStore::from_bytes(2048, bytes.clone()), sip());
    assert!(matches!(tree, Err(Error::Corrupt(CorruptKind::PageIndex))));

    // A leaf whose name runs off the end of the page: found on first read, not by a panic.
    bytes[0..4].copy_from_slice(&1u32.to_le_bytes());
    let leaf = 2048;
    let first_slot_offset = leaf + 20 + 8;
    bytes[first_slot_offset..first_slot_offset + 4].copy_from_slice(&2040u32.to_le_bytes());
    let tree = Tree::open(MemStore::from_bytes(2048, bytes), sip()).expect("root is fine");
    assert_eq!(tree.check(), Err(Error::Corrupt(CorruptKind::NameOffset)));
    assert!(tree.iter().any(|e| e.is_err()));
}

#[test]
fn check_with_rejects_bad_values() {
    let mut tree = mem_tree(4096);
    tree.insert(b"bad", Child::new(1, 0)).expect("insert");
    assert_eq!(
        tree.check_with(|v| v.kind != 0),
        Err(Error::Corrupt(CorruptKind::Value))
    );
}

#[test]
fn file_store_round_trip() {
    let path = std::env::temp_dir().join(format!("nametree-test-{}.dir", std::process::id()));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .expect("create temp file");
    let mut tree = Tree::open(FileStore::new(file, 2048), sip()).expect("open");
    let mut model = Model::new();
    for i in 0..500 {
        let v = Child::new(i as u64, FILE);
        tree.insert(&sized_name(i, 50), v).expect("insert");
        model.insert(sized_name(i, 50), v);
    }
    for i in (0..500).step_by(3) {
        tree.remove(&sized_name(i, 50)).expect("remove");
        model.remove(&sized_name(i, 50));
    }
    let file = tree.into_inner().into_file();
    let reopened = Tree::open(FileStore::new(file, 2048), sip()).expect("reopen");
    assert_matches(&reopened, &model);
    let len = reopened.into_inner().page_count().expect("page count");
    assert!(len > 1);
    std::fs::remove_file(&path).expect("remove temp file");
}
