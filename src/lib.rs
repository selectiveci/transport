#![warn(clippy::perf, clippy::style, clippy::pedantic)]

pub mod build_env;
pub mod conformance;
pub mod error;
pub mod fixtures;
mod legacy_fifo;
pub mod reference_client;
pub mod session;
pub mod snapshot;
pub mod stdio;
pub mod upload;

pub use build_env::{detect_build_env, detect_build_env_at, BuildEnv};
pub use error::Error;
pub use fixtures::FixtureIds;
pub use legacy_fifo::run as run_fifo;
pub use session::{BlockingSession, Keepalive, Session};
pub use snapshot::{snapshot_tree, TestMapTree};
pub use upload::{fragment_endpoint, gzip_bytes, http_origin, upload_fragment, FragmentUpload};
