//! Crate-wide error type. Converts cleanly into an `rmcp` `ErrorData` so tool
//! handlers can `?` on internal failures and surface structured MCP errors.

use rmcp::model::ErrorData;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("upstream MCP error: {0}")]
    Upstream(String),

    #[error("python worker error: {0}")]
    Worker(String),

    #[error("execution error: {0}")]
    Exec(String),

    #[error("execution timed out after {0:?}")]
    Timeout(std::time::Duration),

    #[error("{0}")]
    Other(String),
}

impl From<Error> for ErrorData {
    fn from(e: Error) -> Self {
        ErrorData::internal_error(e.to_string(), None)
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Other(e.to_string())
    }
}
