#![warn(clippy::perf, clippy::style, clippy::pedantic)]

pub mod build_env;
pub mod conformance;
pub mod error;
mod legacy_fifo;
pub mod reference_client;
pub mod session;
pub mod snapshot;
pub mod stdio;
pub mod upload;

pub use build_env::{build_env_json, detect_build_env, detect_build_env_at, BuildEnv};
pub use error::Error;
pub use legacy_fifo::run as run_fifo;
pub use session::{BlockingSession, Session};
pub use snapshot::{snapshot_tree, TestMapTree};
pub use upload::{fragment_endpoint, gzip_bytes, http_origin, upload_fragment, FragmentUpload};
