//! Python execution backends.
//!
//! An [`Executor`] runs user Python code (which calls the generated SDK) and
//! returns its result plus captured output. Backends are selected by
//! `CODEMCP_ISOLATION`.

pub mod host;

#[cfg(feature = "docker")]
pub mod docker;

use async_trait::async_trait;

use crate::control::{RunOptions, RunOutput};
use crate::error::Error;

#[async_trait]
pub trait Executor: Send + Sync {
    /// Run user code and return its result + captured stdout/stderr.
    async fn run(&self, code: String, opts: RunOptions) -> Result<RunOutput, Error>;

    /// Replace the worker's preloaded SDK with `sdk_py` (no restart).
    async fn reload_sdk(&self, sdk_py: &str) -> Result<(), Error>;

    /// Gracefully stop the backend.
    async fn shutdown(&self);
}
