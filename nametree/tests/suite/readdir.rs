//! Readdir tests: iteration resumes correctly from any cookie, including while names are
//! inserted and removed between pages of the listing.

use std::collections::BTreeMap;

use crate::common::{assert_matches, entries, mem_tree, sized_name, Child, Model, DIR, FILE};

const PAGE: usize = 2048;

#[test]
fn empty_tree_lists_nothing() {
    let tree = mem_tree(PAGE);
    assert_eq!(tree.iter().count(), 0);
    assert_eq!(tree.iter_from(12345).count(), 0);
}

#[test]
fn resume_from_every_cookie() {
    let mut tree = mem_tree(PAGE);
    for i in 0..400 {
        tree.insert(&sized_name(i, 30), Child::new(i as u64, FILE))
            .expect("insert");
    }
    let all = entries(&tree);
    assert_eq!(all.len(), 400);
    assert!(all.iter().all(|e| e.cookie > 0 && e.cookie < 1 << 63));
    for (i, entry) in all.iter().enumerate() {
        let rest: Vec<_> = tree
            .iter_from(entry.cookie)
            .collect::<Result<_, _>>()
            .expect("iterate");
        assert_eq!(&rest[..], &all[i + 1..]);
    }
    // Cookies between keys resume at the next key, and nothing lies past the last key.
    let between: Vec<_> = tree
        .iter_from(all[10].cookie + 1)
        .take(1)
        .collect::<Result<_, _>>()
        .expect("iterate");
    assert_eq!(between[0], all[11]);
    assert_eq!(tree.iter_from(u64::MAX).count(), 0);
}

#[test]
fn resume_is_exactly_once_while_names_change() {
    let mut tree = mem_tree(PAGE);
    let mut model = Model::new();
    // Names 0..600 exist throughout; names from 10_000 come and go during the listing.
    for i in 0..600 {
        let v = Child::new(i as u64, FILE);
        tree.insert(&sized_name(i, 40), v).expect("insert");
        model.insert(sized_name(i, 40), v);
    }
    let mut seen: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    let mut cookie = 0;
    let mut churn = 10_000;
    loop {
        let chunk: Vec<_> = tree
            .iter_from(cookie)
            .take(37)
            .collect::<Result<_, _>>()
            .expect("iterate");
        let Some(last) = chunk.last() else { break };
        cookie = last.cookie;
        for entry in &chunk {
            *seen.entry(entry.name.clone()).or_default() += 1;
        }
        // Between chunks: add some names and remove some of the ones added earlier, enough to
        // split and merge leaves and move pages around.
        for _ in 0..25 {
            let v = Child::new(churn as u64, DIR);
            tree.insert(&sized_name(churn, 90), v).expect("insert");
            model.insert(sized_name(churn, 90), v);
            churn += 1;
        }
        for old in (10_000..churn).step_by(3) {
            if model.remove(&sized_name(old, 90)).is_some() {
                tree.remove(&sized_name(old, 90)).expect("remove");
            }
        }
    }
    for i in 0..600 {
        assert_eq!(
            seen.get(&sized_name(i, 40)),
            Some(&1),
            "name {i} seen exactly once"
        );
    }
    assert!(seen.values().all(|&n| n == 1), "no name is listed twice");
    assert_matches(&tree, &model);
}

#[test]
fn filter_reads_only_matching_values() {
    let mut tree = mem_tree(PAGE);
    let mut dirs = Vec::new();
    for i in 0..300 {
        let kind = if i % 4 == 0 { DIR } else { FILE };
        tree.insert(&sized_name(i, 20), Child::new(i as u64, kind))
            .expect("insert");
        if kind == DIR {
            dirs.push(sized_name(i, 20));
        }
    }
    let mut listed: Vec<Vec<u8>> = tree
        .iter_filtered(0, |v| v.kind == DIR)
        .map(|e| e.expect("entry").name)
        .collect();
    listed.sort();
    dirs.sort();
    assert_eq!(listed, dirs);

    // Cookies from a filtered listing resume the filtered listing.
    let all_dirs: Vec<_> = tree
        .iter_filtered(0, |v| v.kind == DIR)
        .collect::<Result<_, _>>()
        .expect("iterate");
    let rest: Vec<_> = tree
        .iter_filtered(all_dirs[20].cookie, |v| v.kind == DIR)
        .collect::<Result<_, _>>()
        .expect("iterate");
    assert_eq!(&rest[..], &all_dirs[21..]);
}
