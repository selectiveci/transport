#![warn(clippy::perf, clippy::style, clippy::pedantic)]

pub mod error;
pub mod snapshot;

pub use error::Error;
pub use snapshot::{snapshot_tree, TestMapTree};
