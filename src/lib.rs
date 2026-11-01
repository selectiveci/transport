#![warn(clippy::perf, clippy::style, clippy::pedantic)]

pub mod build_env;
pub mod error;
pub mod snapshot;

pub use build_env::{detect_build_env, BuildEnv};
pub use error::Error;
pub use snapshot::{snapshot_tree, TestMapTree};
