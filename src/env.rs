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

/// Default fixed port for the Streamable HTTP transport. Used both as the
/// `CODEMCP_HTTP_BIND` default and the `codemcp start --port` default so the two
/// stay in sync.
pub const DEFAULT_HTTP_PORT: u16 = 3388;

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
    /// URL path the Streamable HTTP MCP endpoint is mounted at.
    pub http_path: String,
    /// When true, the HTTP transport replies with plain `application/json` for
    /// request/response calls instead of SSE framing (stateless mode only).
    pub http_json_response: bool,

    pub python: Option<PathBuf>,

    // --- DOCKER isolation ----------------------------------------------------
    /// Image the Python worker runs in. Any stock python image works.
    pub docker_image: String,
    /// User-defined bridge network the worker container attaches to. The control
    /// channel binds to *this network's* gateway IP only, so it is never exposed
    /// on the host's LAN interfaces.
    pub docker_network: String,
    /// Hard memory limit in bytes (`0` = unlimited).
    pub docker_memory: i64,
    /// CPU limit in whole/fractional cores (`0` = unlimited). Mapped to NanoCPUs.
    pub docker_cpus: f64,
    /// Max number of processes in the container (`0` = unlimited).
    pub docker_pids_limit: i64,
    /// Mount the container root filesystem read-only.
    pub docker_readonly: bool,

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

    /// Whether to check for updates on startup (default `"true"`).
    pub check_update: bool,

    /// Learn and surface return shapes. When enabled, the first successful call
    /// to each tool teaches the gateway the (size-bounded) shape of its return
    /// value, which is then appended to that tool's entry in the
    /// `execute_python` description so the model stops guessing field names.
    /// Off by default — steady-state behavior is byte-identical when unset.
    pub learn_shapes: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            config: default_config_path(),
            isolation: Isolation::HostSystem,
            transport: ServerTransport::Stdio,
            http_bind: SocketAddr::from(([127, 0, 0, 1], DEFAULT_HTTP_PORT)),
            http_path: "/mcp".to_string(),
            http_json_response: false,
            python: None,
            docker_image: "python:3.14-slim".to_string(),
            docker_network: "codemcp-net".to_string(),
            docker_memory: 0,
            docker_cpus: 0.0,
            docker_pids_limit: 0,
            docker_readonly: false,
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
            check_update: true,
            learn_shapes: false,
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

/// XDG-style config base: `$XDG_CONFIG_HOME`, else `~/.config`.
///
/// We deliberately do not use `dirs::config_dir()`: on macOS it returns
/// `~/Library/Application Support`, but codemcp (and opencode) follow the XDG
/// convention of `~/.config` on every platform.
pub fn config_base() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        let p = PathBuf::from(xdg);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    dirs::home_dir()
        .map(|h| h.join(".config"))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn default_config_path() -> PathBuf {
    config_base().join("codemcp").join("mcp.json")
}

impl Settings {
    /// The codemcp config path used by `setup`: honors `CODEMCP_CONFIG`, else the
    /// XDG default. Standalone so `setup` need not build full `Settings`.
    pub fn config_path_for_setup() -> PathBuf {
        std::env::var_os("CODEMCP_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(default_config_path)
    }
}

/// Directory holding the worker's `bootstrap.py` + `sdk.py` for DOCKER mode.
///
/// Unlike the host executor (which uses `$TMPDIR`), this lives under the user's
/// cache dir in `$HOME`, because macOS Docker Desktop shares `$HOME` by default
/// but NOT `/var/folders/...`. Returning a per-pid subdir keeps instances apart.
#[cfg(feature = "docker")]
pub fn docker_workdir() -> PathBuf {
    let base = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("codemcp")
        .join("work")
        .join(std::process::id().to_string())
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
