//! Model check: random operation sequences run against both the tree and a `BTreeMap`, with a
//! full fsck after every step.

use nametree::Error;
use proptest::collection::vec;
use proptest::prelude::*;

use crate::common::{assert_matches, mem_tree, sized_name, Child, Model, DIR, FILE};

#[derive(Debug, Clone)]
enum Op {
    Insert(Vec<u8>, u64, bool),
    Remove(Vec<u8>),
    Rename(Vec<u8>, Vec<u8>),
    Get(Vec<u8>),
    /// Resume iteration from the cookie of the Nth entry (mod the entry count).
    Resume(usize),
}

/// Names in the shared pool: drawn often, so operations keep hitting existing names.
const POOL: usize = 400;

fn name() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        4 => (0..POOL).prop_map(|i| sized_name(i, 1 + (i * 37) % 255)),
        1 => vec(any::<u8>(), 1..=255),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (name(), any::<u64>(), any::<bool>()).prop_map(|(n, t, d)| Op::Insert(n, t, d)),
        3 => name().prop_map(Op::Remove),
        2 => (name(), name()).prop_map(|(a, b)| Op::Rename(a, b)),
        1 => name().prop_map(Op::Get),
        1 => any::<usize>().prop_map(Op::Resume),
    ]
}

fn run(page_size: usize, ops: Vec<Op>) -> Result<(), TestCaseError> {
    let mut tree = mem_tree(page_size);
    let mut model = Model::new();
    for op in ops {
        match op {
            Op::Insert(name, target, dir) => {
                let value = Child::new(target, if dir { DIR } else { FILE });
                prop_assert_eq!(tree.insert(&name, value), Ok(model.insert(name, value)));
            }
            Op::Remove(name) => {
                prop_assert_eq!(tree.remove(&name), Ok(model.remove(&name)));
            }
            Op::Rename(from, to) => {
                let expected = if !model.contains_key(&from) {
                    Err(Error::NotFound)
                } else if from == to {
                    Ok(())
                } else if model.contains_key(&to) {
                    Err(Error::Exists)
                } else {
                    let value = model.remove(&from).expect("present");
                    model.insert(to.clone(), value);
                    Ok(())
                };
                prop_assert_eq!(tree.rename(&from, &to), expected);
            }
            Op::Get(name) => {
                prop_assert_eq!(tree.get(&name), Ok(model.get(&name).copied()));
            }
            Op::Resume(n) => {
                let all = crate::common::entries(&tree);
                if !all.is_empty() {
                    let at = n % all.len();
                    let rest: Vec<_> = tree
                        .iter_from(all[at].cookie)
                        .collect::<Result<_, _>>()
                        .expect("iterate");
                    prop_assert_eq!(&rest[..], &all[at + 1..]);
                }
            }
        }
        assert_matches(&tree, &model);
    }

    // Removing every name returns the store to zero length.
    for name in model.keys() {
        prop_assert!(tree.remove(name).expect("remove").is_some());
    }
    assert_matches(&tree, &Model::new());
    prop_assert!(tree.into_inner().as_bytes().is_empty());
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    #[test]
    fn model_2k_pages(ops in vec(op(), 1..300)) {
        run(2048, ops)?;
    }

    #[test]
    fn model_4k_pages(ops in vec(op(), 1..300)) {
        run(4096, ops)?;
    }

    #[test]
    fn model_64k_pages(ops in vec(op(), 1..600)) {
        run(65536, ops)?;
    }
}
