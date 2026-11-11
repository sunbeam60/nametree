# Developing nametree

## Prerequisites

- **Rust** (stable toolchain), installed via [rustup](https://rustup.rs/)

## Project structure

| Path                    | Contents                                                       |
| ----------------------- | -------------------------------------------------------------- |
| `nametree/src/`         | The library                                                    |
| `nametree/tests/suite/` | Integration tests (one test crate, shared fixtures in `common.rs`) |
| `docs/`                 | Design document, divergences from it, this guide               |

Library modules, bottom-up:

| Module           | Contents                                                                              |
| ---------------- | ------------------------------------------------------------------------------------- |
| `format`         | Header and slot layouts, key layout, constants, per-page validation                   |
| `store`          | `PageStore` trait, `MemStore`, `FileStore` (feature `std`)                            |
| `hasher`         | `NameHasher` trait and the pinned SipHash-1-3                                         |
| `leaf`           | Slotted leaf page operations on a byte buffer (no I/O)                                |
| `inner`          | Inner page operations on a byte buffer (no I/O)                                       |
| `page_workspace` | Page workspace for one call: buffered writes, append, move-last, truncate             |
| `tree`           | `NameTree`, descent, insert/split (`tree/insert.rs`), remove/merge (`tree/remove.rs`) |
| `iter`           | Readdir iterator and cookies                                                          |
| `check`          | Full-tree fsck                                                                        |
| `error`          | `Error<E>`, `CorruptKind`                                                             |

## Day-to-day workflow

```bash
cargo test                       # unit, integration and doc tests
cargo test --release             # the same without debug-only page checks
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo build -p nametree --no-default-features   # the no_std + alloc core
```

The VS Code tasks in `.vscode/tasks.json` wrap these. `cargo-ci-check` runs fmt and clippy the way CI does.

### Property tests

`tests/suite/model.rs` runs random operation sequences against both the tree and a `BTreeMap`, with a full fsck after every step. Raise the case count for a deeper soak:

```bash
PROPTEST_CASES=1000 cargo test --release --test suite model
```

If proptest finds a failure, it writes the seed to `proptest-regressions/`. Commit that file, so the case keeps being replayed.

### Golden tests

Two kinds of test pin things that must never change silently:

- `hasher::tests::siphash13_golden_vectors`: every stored key depends on the hasher's output.
- `tests/suite/format.rs`: byte images of the design's worked examples (the single leaf byte-for-byte, the two-level tree by fingerprint).

If one of these fails, the on-disk format has changed. Update the expected values only if the change is intended, and record it in `docs/differences.md`.

## Using a local checkout from a host crate

While developing a host and nametree side by side, point the host at the checkout:

```toml
nametree = { path = "../nametree/nametree" }
```

Before release, switch to a git dependency or a published version.

## Releasing

1. Bump `version` in the workspace `Cargo.toml`.
2. Commit, then tag `vX.Y.Z` and push the tag.
3. The `Release` workflow checks that the tag matches `Cargo.toml`, runs the tests, publishes to crates.io and creates a GitHub release.

Publishing needs a `CARGO_REGISTRY_TOKEN` secret in a GitHub environment named `crates.io`.
