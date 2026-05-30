//! `npxc` library crate.
//!
//! All module declarations live here so that both the library crate (used by
//! integration tests via `use npxc::...`) and the binary crate (`src/main.rs`,
//! which links against this library) share the same module tree.
//!
//! # Stability
//!
//! `npxc` is distributed as a command-line tool. This library exists to support
//! the binary and its integration tests; the public API is **not** covered by
//! semantic-versioning guarantees and may change between releases.

pub mod cli;
pub mod config;
pub mod dockerfile;
pub mod error;
pub mod paths;
pub mod rpc;
pub mod runtime;
