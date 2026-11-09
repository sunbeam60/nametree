//! Integration tests for nametree, following the testing strategy in
//! `docs/yak-name-tree-design.md`. One test crate, so shared fixtures compile once.

mod common;

mod api;
mod collisions;
mod faults;
mod format;
mod model;
mod page_count;
mod readdir;
mod shape;
