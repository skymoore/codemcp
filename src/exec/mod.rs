//! Python execution backends.
//!
//! An [`Executor`] runs user Python code (which calls the generated SDK) and
//! returns its result plus captured output. Backends are selected by
//! `CODEMCP_ISOLATION`.

pub mod host;

use async_trait::async_trait;

use crate::control::RunOutput;
use crate::error::Error;

#[async_trait]
pub trait Executor: Send + Sync {
    /// Run user code and return its result + captured stdout/stderr.
    async fn run(&self, code: String) -> Result<RunOutput, Error>;

    /// Gracefully stop the backend.
    async fn shutdown(&self);
}
