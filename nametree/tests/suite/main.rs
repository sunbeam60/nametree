//! Integration tests for nametree, following the testing strategy in
//! `docs/design.md`. One test crate, so shared fixtures compile once.

mod common;

mod api;
mod collisions;
mod corruption;
mod faults;
mod format;
mod model;
mod page_count;
mod readdir;
mod shape;
