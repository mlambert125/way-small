//! Unit tests, kept out of the implementation tree.
//!
//! Mirrors the module layout under `src/` one-for-one, so a test file's path
//! says exactly what it tests. Everything it exercises that isn't already
//! `pub` had to become `pub(crate)` to make this possible — see the sibling
//! source files for what that widened.

mod backend;
mod compositor;
