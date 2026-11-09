//! Format tests: golden byte images of the design's worked examples, so the on-disk layout
//! cannot drift unnoticed.

use nametree::{MemStore, PageStore};

use crate::common::{assert_matches, Child, MapHasher, Model, Tree, DIR, FILE};

const PAGE: usize = 4096;

/// Hash whose key prefix (bits 62..4 of the key) is `prefix`: the key layout keeps the top 59
/// bits of the hash, shifted down by one.
const fn hash_for_prefix(prefix: u64) -> u64 {
    prefix << 1
}

const A_TXT: u64 = 0x0D15_0000_0000_0000;
const DOCS: u64 = 0x2D18_0000_0000_0000;
const NOTES: u64 = 0x4E03_0000_0000_0000;

/// The worked example's illustrative keys (0x0D15…, 0x2D18…, 0x4E03…).
static WORKED: MapHasher = MapHasher(&[
    (b"a.txt", hash_for_prefix(A_TXT)),
    (b"docs", hash_for_prefix(DOCS)),
    (b"notes.md", hash_for_prefix(NOTES)),
]);

fn put_u32(page: &mut [u8], at: usize, v: u32) {
    page[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Writes one 22-byte leaf slot by hand: key, name offset, name length, target, type.
fn put_slot(page: &mut [u8], at: usize, key: u64, offset: u32, len: u8, target: u64, kind: u8) {
    page[at..at + 8].copy_from_slice(&key.to_le_bytes());
    put_u32(page, at + 8, offset);
    page[at + 12] = len;
    page[at + 13..at + 21].copy_from_slice(&target.to_le_bytes());
    page[at + 21] = kind;
}

/// The single-leaf worked example: docs, a.txt and notes.md inserted in that order.
#[test]
fn single_leaf_matches_worked_example() {
    let mut tree = Tree::open(MemStore::new(PAGE), &WORKED).expect("open");
    tree.insert(b"docs", Child::new(7, DIR)).expect("insert");
    tree.insert(b"a.txt", Child::new(42, FILE)).expect("insert");
    tree.insert(b"notes.md", Child::new(43, FILE))
        .expect("insert");

    let mut expected = vec![0u8; PAGE];
    // Header: link 0, slot_count 3, heap_start 4079, frag_bytes 0, level 1.
    put_u32(&mut expected, 0, 0);
    put_u32(&mut expected, 4, 3);
    put_u32(&mut expected, 8, 4079);
    put_u32(&mut expected, 12, 0);
    put_u32(&mut expected, 16, 1);
    // Slots in key order, each with counter 1.
    put_slot(&mut expected, 20, A_TXT | 1, 4087, 5, 42, FILE);
    put_slot(&mut expected, 42, DOCS | 1, 4092, 4, 7, DIR);
    put_slot(&mut expected, 64, NOTES | 1, 4079, 8, 43, FILE);
    // Names in insertion order, packed down from the end of the page.
    expected[4079..4087].copy_from_slice(b"notes.md");
    expected[4087..4092].copy_from_slice(b"a.txt");
    expected[4092..4096].copy_from_slice(b"docs");

    assert_eq!(tree.into_inner().as_bytes(), &expected[..]);
}

/// FNV-1a over the store bytes: a compact fingerprint for a golden multi-page image.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn header_u32(bytes: &[u8], page: usize, field: usize) -> u32 {
    let at = page * PAGE + field * 4;
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

/// The two-level worked example: the directory just after its root split.
#[test]
fn two_level_tree_after_root_split() {
    let mut tree = crate::common::mem_tree(PAGE);
    let mut model = Model::new();
    let mut i = 0u64;
    while tree.store_mut().page_count().expect("count") < 3 {
        let name = format!("file-{i:04}.txt").into_bytes();
        let v = Child::new(i, FILE);
        tree.insert(&name, v).expect("insert");
        model.insert(name, v);
        i += 1;
    }
    assert_eq!(model.len(), GOLDEN_NAMES);
    assert_matches(&tree, &model);
    let bytes = tree.into_inner().into_bytes();
    // Header fields: 0 link, 1 slot_count, 2 heap_start, 3 frag_bytes, 4 level.
    assert_eq!(
        header_u32(&bytes, 0, 4),
        2,
        "root is an inner page at level 2"
    );
    assert_eq!(
        header_u32(&bytes, 0, 0),
        1,
        "root's leftmost child is page 1"
    );
    assert_eq!(header_u32(&bytes, 0, 1), 1, "root has one separator");
    assert_eq!(
        header_u32(&bytes, 0, 2),
        0,
        "inner pages write heap_start as 0"
    );
    assert_eq!(
        &bytes[28..32],
        &2u32.to_le_bytes(),
        "the separator points at page 2"
    );
    assert_eq!(header_u32(&bytes, 1, 0), 2, "page 1 links to page 2");
    assert_eq!(header_u32(&bytes, 2, 0), 0, "page 2 is the last leaf");
    assert_eq!(header_u32(&bytes, 1, 4), 1);
    assert_eq!(header_u32(&bytes, 2, 4), 1);
    assert_eq!(fnv1a(&bytes), GOLDEN_FNV, "two-level image drifted");
}

/// Names inserted before the root split, and the fingerprint of the resulting store.
const GOLDEN_NAMES: usize = 117;
const GOLDEN_FNV: u64 = 0x0a43_d951_609e_9a8a;
