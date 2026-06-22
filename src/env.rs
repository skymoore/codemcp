//! All `CODEMCP_*` environment variables, parsed into a typed [`Settings`] struct
//! via `figment`.
//!
//! Everything that may need changing is controlled here. Reading happens once at
//! startup; downstream code takes `&Settings`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use figment::providers::Env;
use figment::Figment;
use serde::Deserialize;

use crate::error::Error;

/// Python execution isolation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Isolation {
    #[serde(alias = "HOST")]
    HostSystem,
    Docker,
    Monty,
}

/// MCP server-side transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerTransport {
    Stdio,
    Http,
}

/// Fully-resolved runtime settings, deserialized from `CODEMCP_*` env vars.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Path to the opencode-style `mcp.json`.
    pub config: PathBuf,
    pub isolation: Isolation,
    pub transport: ServerTransport,
    pub http_bind: SocketAddr,

    pub python: Option<PathBuf>,
    pub docker_image: String,
    /// Whitespace-split via custom deserializer.
    #[serde(deserialize_with = "de_args")]
    pub docker_extra_args: Vec<String>,

    pub control_bind: String,
    pub control_host_for_worker: Option<String>,
    /// Auto-generated per run if unset.
    pub control_token: Option<String>,

    pub ws_auto_install: bool,
    pub ws_version: Option<String>,
    #[serde(deserialize_with = "de_args")]
    pub ws_pip_args: Vec<String>,

    pub exec_timeout_ms: u64,
    pub max_output_bytes: usize,
    pub monty_mem_limit: usize,

    pub enable_llm_summaries: bool,
    pub summary_model: Option<String>,
    pub summary_api_base: Option<String>,
    pub summary_api_key: Option<String>,
    pub summary_cache: PathBuf,

    pub log: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            config: default_config_path(),
            isolation: Isolation::HostSystem,
            transport: ServerTransport::Stdio,
            http_bind: "127.0.0.1:3388".parse().expect("valid default addr"),
            python: None,
            docker_image: "python:3.14-slim".to_string(),
            docker_extra_args: Vec::new(),
            control_bind: "127.0.0.1:0".to_string(),
            control_host_for_worker: None,
            control_token: None,
            ws_auto_install: true,
            ws_version: None,
            ws_pip_args: Vec::new(),
            exec_timeout_ms: 30_000,
            max_output_bytes: 1_048_576,
            monty_mem_limit: 268_435_456,
            enable_llm_summaries: false,
            summary_model: None,
            summary_api_base: None,
            summary_api_key: None,
            summary_cache: default_summary_cache(),
            log: "info".to_string(),
        }
    }
}

/// Deserialize a whitespace-separated string into a `Vec<String>`.
fn de_args<'de, D>(de: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(de)?;
    Ok(s.split_whitespace().map(String::from).collect())
}

fn default_config_path() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("codemcp").join("mcp.json")
}

fn default_summary_cache() -> PathBuf {
    let base = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("codemcp").join("summaries.json")
}

/// Generate a random hex token for the control channel.
fn random_token() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Settings {
    /// Load settings from `CODEMCP_*` environment variables.
    pub fn from_env() -> Result<Self, Error> {
        let mut settings: Settings = Figment::new()
            .merge(Env::prefixed("CODEMCP_"))
            .extract()
            .map_err(|e| Error::Config(e.to_string()))?;

        if settings.control_token.is_none() {
            settings.control_token = Some(random_token());
        }
        Ok(settings)
    }

    pub fn exec_timeout(&self) -> Duration {
        Duration::from_millis(self.exec_timeout_ms)
    }

    pub fn control_token(&self) -> &str {
        self.control_token
            .as_deref()
            .expect("control_token set in from_env")
    }
}
