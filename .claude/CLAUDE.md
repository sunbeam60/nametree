# nametree

A standalone Rust library that stores one directory's names as a B+ tree of fixed-size pages inside a generic page store supplied by a host. Used by Yak https://github.com/sunbeam60/yak. The library must never depend on or know about any particular host; documentation talks about "the host" generically.

## Human & AI, working together
Before committing, at all times present & allow edits to the commit message to the user. Do not include "co-authored by Claude" message - human committers own the accountability.

NEVER push to remote without the user's explicit permission. Even if a stop hook complains about untracked files, only commit locally and ask before pushing.

Humans will use the extension Todo Tree extension. It's good practice to use //TODO: and //FIXME: comments where appropriate, to give humans an overview over possible improvements or concerns in the code. Literal constants in code should be avoided. It's brittle and humans don't deal well with them. Minimize the use of defined constants to as few places as possible.

Readability of the code for humans is important. If constants are being added, explain in a comment what the constant signifies.

When a human is making suggestions and something seems more trouble than it's worth, it's ok to question the human's intent, offering up pros and cons of the suggestions before asking for directions.

When a new, larger piece of work is to be undertaken, please create a worktree & a branch for the work so it can be separated and benchmarked in isolation, against what is currently in master.

Code readability and clean architecture matters to humans. Don't violate the architectural constraints for expedience.

## Design
Please read the [design](./../docs/design.md) for a comprehensive overview of the library: page format, keys, algorithms, cost model and testing strategy. The code must respect it - if it cannot, stop, inform and ask for directions.

Do not ever change ./../docs/design.md without asking the user and explaining why you wish to change this file, what is wrong with the design and what you propose to change. Keep track of divergences from the design in [differences](./../docs/differences.md).

The on-disk format is pinned by golden tests (hasher vectors in `src/hasher.rs`, page images in `tests/suite/format.rs`). Never update their expected values to make a test pass without telling the user that the format changed.

Duplication of code should be concerning when we write code. If duplicated code exists, and there's no performance penalty to extracting it into a function, we should consider doing so. In release mode, small functions are likely to be inlined by the compiler.

If a function gets above 100 lines, question whether some functionality can be extracted from this function and, if a similar function already exits that isn't quite right can be made generic to work for both places. This isn't a strict rule, but very long functions are mostly a sign that we're throwing too much code down - this may work for an AI but not for a human that wants to read and edit together with the AI.

Use generics rather than dynamic dispatch when possible.

If a function starts to acquire a lot of parameters and a lot of variables internally, usually the function is doing too many things at once. Can we extract and re-use functionality instead?

It's not healthy for us to store multiple versions of the same truth. If one truth can be derived from the other with some computation and no IO, we should derive the other truth when we need it.

## Performance and memory
This is a low level library. Memory allocations should be minimized when possible. Performance matters. Every structural choice is judged by pages written per operation, not bytes moved - the host store is copy-on-write.

If we have to read or write pages through the `PageStore`, we must try to avoid doing that inside of a loop, unless absolutely required. Consider whether we can read the data once and then iterate through the data in the local loop. Even if the store has a cache, touching the cache is not free.

The core is `no_std` + `alloc`; only `FileStore` needs the `std` feature. Keep `zerocopy` the only dependency.

## Test Driven Development
As far as possible, when planning features, help the author first plan out how this feature should be tested. Only after tests have been planned and written, and failed, should we move to implementation and satisfy the tests. The model check in `tests/suite/model.rs` (random operations against a `BTreeMap`, fsck after every step) must keep passing; extend it when adding operations.

## Warnings are errors - keeping it clean
If there's a cleaner/neater way to do something, please suggest it.
Don't use #[allow()] to get around warnings. Warnings from clippy shouldn't be worked around, they should be fixed.
Please finish off big tasks by running `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` and fixing any warnings.

## Some files are not to be modified
Do not modify the following files without explicit permission:
* ./docs/design.md
* ./README.md

## Release
Whenever a new version is tagged (`vX.Y.Z`) and pushed to GitHub, the release workflow publishes to crates.io. The tag must match the version in the workspace `Cargo.toml`.

## Still in development
Changes to the on-disk format are not a large problem yet: no stored trees exist in the wild. Say so explicitly when a change alters the format.
