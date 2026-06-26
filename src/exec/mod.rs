//! Python execution backends.
//!
//! An [`Executor`] runs user Python code (which calls the generated SDK) and
//! returns its result plus captured output. Backends are selected by
//! `CODEMCP_ISOLATION`.

pub mod host;

#[cfg(feature = "docker")]
pub mod docker;

use std::collections::BTreeMap;

use async_trait::async_trait;

use crate::control::RunOutput;
use crate::error::Error;
use crate::sdk::keyset::KeySet;

#[async_trait]
pub trait Executor: Send + Sync {
    /// Run user code and return its result + captured stdout/stderr.
    async fn run(&self, code: String) -> Result<RunOutput, Error>;

    /// Replace the worker's preloaded SDK with `sdk_py` (no restart).
    async fn reload_sdk(&self, sdk_py: &str) -> Result<(), Error>;

    /// Push the `fn_name -> KeySet` validation map to the worker, used for strict
    /// pre-flight field-access checks. An empty map disables the check. Default
    /// is a no-op so backends that don't support it (or when the feature is off)
    /// simply ignore it.
    async fn set_shapes(&self, _keysets: &BTreeMap<String, KeySet>) -> Result<(), Error> {
        Ok(())
    }

    /// Gracefully stop the backend.
    async fn shutdown(&self);
}
