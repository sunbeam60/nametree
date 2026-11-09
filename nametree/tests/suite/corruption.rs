//! Corrupt pages must produce `Error::Corrupt`, never a panic, a hang or a successful call that
//! writes an unreadable page. The named tests reproduce bugs found in review; the fuzz test
//! mutates valid trees at random and runs every API call against them.

use std::panic::{self, AssertUnwindSafe};

use nametree::{CorruptKind, Error, MemStore, NameHasher};

use crate::common::{mem_tree, sip, sized_name, Bucketed, Child, Tree, FILE};

const PAGE: usize = 2048;

/// Byte offsets of header fields within a page.
const LINK: usize = 0;
const SLOT_COUNT: usize = 4;
const HEAP_START: usize = 8;
const FRAG_BYTES: usize = 12;
const LEVEL: usize = 16;
const HEADER: usize = 20;
/// Slot sizes for the `Child` value type, and the offset of fields within them.
const LEAF_SLOT: usize = 22;
const INNER_SLOT: usize = 12;
const SLOT_OFFSET: usize = 8;
const SLOT_NAME_LEN: usize = 12;
const SLOT_CHILD: usize = 8;

fn get32(bytes: &[u8], page: usize, at: usize) -> u32 {
    let at = page * PAGE + at;
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn put32(bytes: &mut [u8], page: usize, at: usize, v: u32) {
    let at = page * PAGE + at;
    bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn pages(bytes: &[u8]) -> usize {
    bytes.len() / PAGE
}

/// Store bytes of a tree holding `count` names of `len` bytes.
fn tree_bytes(count: usize, len: usize) -> Vec<u8> {
    let mut tree = mem_tree(PAGE);
    for i in 0..count {
        tree.insert(&sized_name(i, len), Child::new(i as u64, FILE))
            .expect("insert");
    }
    tree.check().expect("valid tree");
    tree.into_inner().into_bytes()
}

fn open(bytes: &[u8]) -> Result<Tree, Error<nametree::MemStoreError>> {
    Tree::open(MemStore::from_bytes(PAGE, bytes.to_vec()), sip())
}

/// Names stored in leaf page `page`.
fn leaf_names(bytes: &[u8], page: usize) -> Vec<Vec<u8>> {
    (0..get32(bytes, page, SLOT_COUNT) as usize)
        .map(|i| {
            let slot = HEADER + i * LEAF_SLOT;
            let offset = get32(bytes, page, slot + SLOT_OFFSET) as usize;
            let len = bytes[page * PAGE + slot + SLOT_NAME_LEN] as usize;
            bytes[page * PAGE + offset..page * PAGE + offset + len].to_vec()
        })
        .collect()
}

/// An overstated `frag_bytes` once made a full leaf look underfull, so a merge folded two full
/// leaves into one page and ran off its end; on insert, compaction "recovered" bytes that did
/// not exist and the new slot overwrote the new name.
#[test]
fn overstated_frag_bytes_is_corrupt() {
    let bytes = tree_bytes(40, 255);
    let leaf = (1..pages(&bytes))
        .filter(|&p| get32(&bytes, p, LEVEL) == 1)
        .max_by_key(|&p| get32(&bytes, p, SLOT_COUNT))
        .expect("a leaf");
    let heap = PAGE as u32 - get32(&bytes, leaf, HEAP_START);
    let mut bad = bytes.clone();
    put32(&mut bad, leaf, FRAG_BYTES, heap - 255);
    let mut tree = open(&bad).expect("root is intact");
    let name = &leaf_names(&bytes, leaf)[0];
    assert_eq!(
        tree.remove(name),
        Err(Error::Corrupt(CorruptKind::FragBytes))
    );
    assert_eq!(tree.store_mut().as_bytes(), &bad[..], "nothing written");

    // A single root leaf with the same damage is caught when the tree is opened.
    let mut root = tree_bytes(16, 100);
    let heap = PAGE as u32 - get32(&root, 0, HEAP_START);
    put32(&mut root, 0, FRAG_BYTES, heap);
    assert!(matches!(
        open(&root),
        Err(Error::Corrupt(CorruptKind::FragBytes))
    ));
}

/// A merge partner comes from the parent's slots, not the descent path. A child pointer aimed
/// at a page of the wrong level once made the merge read an inner page as a leaf.
#[test]
fn merge_sibling_at_wrong_level_is_corrupt() {
    let bytes = tree_bytes(1500, 255);
    assert_eq!(get32(&bytes, 0, LEVEL), 3);
    let level2: Vec<usize> = (1..pages(&bytes))
        .filter(|&p| get32(&bytes, p, LEVEL) == 2)
        .collect();
    let (parent, other) = (level2[0], level2[1]);
    let mut bad = bytes.clone();
    // The parent's second child now points at another level-2 page.
    put32(&mut bad, parent, HEADER + SLOT_CHILD, other as u32);
    let leftmost = get32(&bytes, parent, LINK) as usize;
    let mut tree = open(&bad).expect("root is intact");
    let result = leaf_names(&bytes, leftmost)
        .iter()
        .map(|name| tree.remove(name))
        .find(Result::is_err);
    assert_eq!(
        result,
        Some(Err(Error::Corrupt(CorruptKind::LevelMismatch)))
    );
}

/// SipHash, except that names starting with "twin" collide with `anchor`.
struct Twin(Vec<u8>);

impl NameHasher for Twin {
    fn hash(&self, name: &[u8]) -> u64 {
        if name.starts_with(b"twin") {
            sip().hash(&self.0)
        } else {
            sip().hash(name)
        }
    }
}

/// Lookups route by a run's highest key, which is only safe if separators carry counter 1.
/// The check once accepted a separator with counter 2 (the literal "first key in the child"
/// after counter 1 was deleted), and a valid insert then put a counter-1 key below it.
#[test]
fn check_rejects_separator_with_counter_above_1() {
    let bytes = tree_bytes(60, 100);
    assert_eq!(get32(&bytes, 0, LEVEL), 2);
    let separator_at = HEADER;
    let right = get32(&bytes, 0, HEADER + SLOT_CHILD) as usize;
    let anchor = leaf_names(&bytes, right)[0].clone();
    type TwinTree = nametree::NameTree<MemStore, Twin, Child>;
    let mut tree =
        TwinTree::open(MemStore::from_bytes(PAGE, bytes), Twin(anchor.clone())).expect("open");
    // A twin joins the anchor's run with counter 2; removing the anchor leaves the right leaf
    // starting at counter 2 while its separator still says counter 1.
    tree.insert(b"twin-a", Child::new(1, FILE)).expect("insert");
    tree.remove(&anchor).expect("remove");
    tree.check().expect("still valid");

    let mut bytes = tree.into_inner().into_bytes();
    let field = separator_at..separator_at + 8;
    let separator = u64::from_le_bytes(bytes[field.clone()].try_into().expect("eight bytes"));
    bytes[field].copy_from_slice(&(separator + 1).to_le_bytes());
    let tree = TwinTree::open(MemStore::from_bytes(PAGE, bytes), Twin(anchor)).expect("open");
    assert_eq!(tree.check(), Err(Error::Corrupt(CorruptKind::Separator)));
}

/// The value check can read from the tree it is checking.
#[test]
fn check_with_can_read_the_tree() {
    let mut tree = mem_tree(PAGE);
    tree.insert(b"a", Child::new(1, FILE)).expect("insert");
    let report = tree
        .check_with(|_| tree.get(b"a").is_ok_and(|v| v.is_some()))
        .expect("check");
    assert_eq!(report.names, 1);
}

/// Deterministic xorshift, so fuzz failures replay.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// A value likely to sit on a boundary the code must handle.
fn edgy(rng: &mut Rng, old: u32, pages: usize) -> u32 {
    let pages = pages as u32;
    match rng.below(10) {
        0 => 0,
        1 => 1,
        2 => old.wrapping_add(1),
        3 => old.wrapping_sub(1),
        4 => PAGE as u32,
        5 => pages,
        6 => rng.below(pages as usize) as u32,
        7 => u32::MAX,
        8 => old.wrapping_add(rng.below(64) as u32).wrapping_sub(32),
        _ => rng.next() as u32,
    }
}

/// Damages one field of one page and describes what it did.
fn mutate(rng: &mut Rng, bytes: &mut [u8]) -> String {
    let total = pages(bytes);
    let page = rng.below(total);
    let count = get32(bytes, page, SLOT_COUNT) as usize;
    let leaf = get32(bytes, page, LEVEL) == 1;
    let slot = rng.below(count.min(80));
    match rng.below(6) {
        0 | 1 => {
            let field = rng.below(5) * 4;
            let new = edgy(rng, get32(bytes, page, field), total);
            put32(bytes, page, field, new);
            format!("page {page} header byte {field} = {new}")
        }
        2 if leaf => {
            let at = HEADER + slot * LEAF_SLOT + SLOT_OFFSET;
            let new = edgy(rng, get32(bytes, page, at), total);
            put32(bytes, page, at, new);
            format!("page {page} slot {slot} name offset = {new}")
        }
        2 => {
            let at = HEADER + slot * INNER_SLOT + SLOT_CHILD;
            let new = edgy(rng, get32(bytes, page, at), total);
            put32(bytes, page, at, new);
            format!("page {page} slot {slot} child = {new}")
        }
        3 => {
            let size = if leaf { LEAF_SLOT } else { INNER_SLOT };
            let at = page * PAGE + HEADER + slot * size + rng.below(size);
            bytes[at] ^= 1 << rng.below(8);
            format!("page {page} slot {slot} byte flipped")
        }
        _ => {
            let at = page * PAGE + rng.below(PAGE);
            bytes[at] = rng.next() as u8;
            format!("page {page} byte {} randomised", at % PAGE)
        }
    }
}

/// Runs every API call against a possibly corrupt tree, ignoring results.
fn exercise<H: NameHasher>(bytes: &[u8], hasher: H, names: &[Vec<u8>], rng: &mut Rng) {
    let Ok(mut tree) = Tree::open(MemStore::from_bytes(PAGE, bytes.to_vec()), hasher) else {
        return;
    };
    let _ = tree.check();
    let _ = tree.iter().take(5000).count();
    // Removing many names makes merges, and so reads of sibling pages, likely.
    let start = rng.below(names.len());
    for k in 0..40 {
        let name = &names[(start + k) % names.len()];
        let _ = match rng.below(6) {
            0 => tree.get(name).map(drop),
            1..=3 => tree.remove(name).map(drop),
            4 => tree
                .insert(
                    &sized_name(rng.below(100_000), 1 + rng.below(255)),
                    Child::new(1, FILE),
                )
                .map(drop),
            _ => tree.rename(name, &sized_name(rng.below(100_000), 50)),
        };
    }
    let _ = tree.iter_from(rng.next() >> 1).take(5000).count();
    let _ = tree.check();
}

/// Iterations per run; raise with `NAMETREE_FUZZ_ITERATIONS` for a soak.
fn iterations() -> usize {
    std::env::var("NAMETREE_FUZZ_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

#[test]
fn corrupted_trees_never_panic() {
    let names: Vec<Vec<u8>> = (0..1300).map(|i| sized_name(i, 255)).collect();
    let deep = tree_bytes(names.len(), 255);
    let mut collided = Tree::open(MemStore::new(PAGE), Bucketed { buckets: 40 }).expect("open");
    for name in names.iter().take(500) {
        let _ = collided.insert(name, Child::new(1, FILE));
    }
    let collided = collided.into_inner().into_bytes();

    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for iteration in 0..iterations() {
        let base = if iteration % 2 == 0 { &deep } else { &collided };
        let mut bad = base.clone();
        let damage: Vec<String> = (0..1 + rng.below(3))
            .map(|_| mutate(&mut rng, &mut bad))
            .collect();
        let mut op_rng = Rng(rng.next() | 1);
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
            if iteration % 2 == 0 {
                exercise(&bad, sip(), &names, &mut op_rng);
            } else {
                exercise(&bad, Bucketed { buckets: 40 }, &names, &mut op_rng);
            }
        }));
        assert!(
            outcome.is_ok(),
            "iteration {iteration} panicked after {damage:?}"
        );
    }
}
