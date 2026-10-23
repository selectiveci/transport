#![warn(clippy::perf, clippy::style, clippy::pedantic)]

pub mod build_env;
pub mod conformance;
pub mod error;
pub mod fixtures;
pub mod reference_client;
pub mod session;
pub mod snapshot;
pub mod upload;

pub use build_env::{detect_build_env, BuildEnv};
pub use error::Error;
pub use fixtures::FixtureIds;
pub use session::{BlockingSession, Keepalive, Session};
pub use snapshot::{snapshot_tree, TestMapTree};
pub use upload::{gzip_bytes, upload_fragment, FragmentUpload};
