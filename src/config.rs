//! Loads the opencode-style `mcp.json` describing upstream MCP servers.
//!
//! Format (subset of opencode's `mcp` object):
//! ```json
//! {
//!   "mcp": {
//!     "github": { "type": "local", "command": ["npx","-y","..."], "environment": {"X":"y"} },
//!     "sentry": { "type": "remote", "url": "https://mcp.sentry.dev/mcp", "headers": {"Authorization":"Bearer {env:TOKEN}"} }
//!   }
//! }
//! ```
//! Values support `{env:VAR}` interpolation. Entries with `"enabled": false` are
//! skipped. Remote servers may carry an `oauth` block for OAuth 2.1 browser auth.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::error::Error;

/// OAuth configuration for a remote MCP server. Mirrors opencode's `OAuth`
/// config schema. All fields optional — when absent, the client uses
/// auto-discovery and dynamic client registration (RFC 7591).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OAuthConfig {
    /// Pre-registered OAuth client ID. If not provided, dynamic client
    /// registration (RFC 7591) will be attempted.
    #[serde(default)]
    pub client_id: Option<String>,
    /// OAuth client secret (if required by the authorization server).
    #[serde(default)]
    pub client_secret: Option<String>,
    /// OAuth scopes to request during authorization.
    #[serde(default)]
    pub scope: Option<String>,
    /// Port for the local OAuth callback server. Shorthand for `redirect_uri`
    /// when only the port needs changing. Default: ephemeral.
    #[serde(default)]
    pub callback_port: Option<u16>,
    /// Full OAuth redirect URI (default: `http://127.0.0.1:<ephemeral>/callback`).
    #[serde(default)]
    pub redirect_uri: Option<String>,
}

/// The `oauth` field on a remote server: either an explicit config object,
/// `true` (auto-detect), or `false` (disabled).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum OAuthSetting {
    Explicit(OAuthConfig),
    Flag(bool),
}

impl OAuthSetting {
    /// Whether OAuth is explicitly disabled (`oauth: false`).
    pub fn is_disabled(&self) -> bool {
        matches!(self, OAuthSetting::Flag(false))
    }

    /// Whether a pre-registered client ID is configured.
    #[allow(dead_code)]
    pub fn client_id(&self) -> Option<&str> {
        match self {
            OAuthSetting::Explicit(c) => c.client_id.as_deref(),
            OAuthSetting::Flag(true) => None,
            OAuthSetting::Flag(false) => None,
        }
    }

    /// The full OAuth config if explicitly set.
    pub fn config(&self) -> Option<&OAuthConfig> {
        match self {
            OAuthSetting::Explicit(c) => Some(c),
            _ => None,
        }
    }
}

/// Top-level config file shape. We only care about the `mcp` map.
#[derive(Debug, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub mcp: BTreeMap<String, ServerSpec>,
}

/// Per-tool default configuration under a server's `tools` map.
///
/// Mirrors the server-level `enabled: Option<bool>` rule: absent or `null`
/// means the default (`true`). Explicit `false` hides the tool by default.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolEnabledCfg {
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl ToolEnabledCfg {
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

/// A single upstream server specification.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerSpec {
    Local {
        command: Vec<String>,
        #[serde(default)]
        environment: BTreeMap<String, String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        enabled: Option<bool>,
        #[serde(default)]
        timeout: Option<u64>,
        /// Per-tool default `enabled` flags. Absent tools default to enabled.
        #[serde(default)]
        tools: Option<BTreeMap<String, ToolEnabledCfg>>,
    },
    Remote {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default)]
        enabled: Option<bool>,
        #[serde(default)]
        timeout: Option<u64>,
        /// OAuth authentication configuration. `None` (absent) means
        /// auto-detect (try OAuth if the server requires auth). `Some(false)`
        /// disables OAuth. `Some(config)` uses the explicit config.
        #[serde(default)]
        oauth: Option<OAuthSetting>,
        /// Per-tool default `enabled` flags. Absent tools default to enabled.
        #[serde(default)]
        tools: Option<BTreeMap<String, ToolEnabledCfg>>,
    },
}

impl ServerSpec {
    pub fn enabled(&self) -> bool {
        match self {
            ServerSpec::Local { enabled, .. } | ServerSpec::Remote { enabled, .. } => {
                enabled.unwrap_or(true)
            }
        }
    }

    /// Explicitly-configured per-tool default `enabled` flags (tools not listed
    /// are absent and default to `true`). Used to seed the runtime's default map.
    pub fn tool_defaults(&self) -> &BTreeMap<String, ToolEnabledCfg> {
        const EMPTY: &BTreeMap<String, ToolEnabledCfg> = &BTreeMap::new();
        match self {
            ServerSpec::Local { tools, .. } | ServerSpec::Remote { tools, .. } => {
                tools.as_ref().unwrap_or(EMPTY)
            }
        }
    }

    /// Whether `tool` is enabled by default according to this spec. Tools not
    /// listed in the `tools` map default to enabled.
    #[allow(dead_code)]
    pub fn tool_default_enabled(&self, tool: &str) -> bool {
        self.tool_defaults()
            .get(tool)
            .map(|c| c.enabled())
            .unwrap_or(true)
    }
}

/// A resolved (env-interpolated) upstream server plus its config `enabled` flag.
#[derive(Debug, Clone)]
pub struct UpstreamConfig {
    pub name: String,
    pub spec: ServerSpec,
    /// Whether this server is `enabled` in the config file (boot state).
    pub enabled: bool,
    /// Explicitly-set per-tool default `enabled` flags (absent => default on).
    pub tool_defaults: BTreeMap<String, bool>,
}

/// Load + parse the config file, interpolate `{env:VAR}`, and return only the
/// enabled servers (the boot-time set to connect at startup).
pub fn load(path: &Path) -> Result<Vec<UpstreamConfig>, Error> {
    Ok(load_all(path)?.into_iter().filter(|c| c.enabled).collect())
}

/// Load + parse every server in the config file (enabled and disabled), with
/// `{env:VAR}` interpolated. Used by the admin runtime so a currently-disabled
/// server can still be connected on demand.
pub fn load_all(path: &Path) -> Result<Vec<UpstreamConfig>, Error> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("cannot read config {}: {e}", path.display())))?;
    let mut file: ConfigFile = serde_json::from_str(&raw)
        .map_err(|e| Error::Config(format!("invalid config {}: {e}", path.display())))?;

    let mut out = Vec::new();
    for (name, mut spec) in std::mem::take(&mut file.mcp) {
        let enabled = spec.enabled();
        let tool_defaults = spec
            .tool_defaults()
            .iter()
            .map(|(tool, cfg)| (tool.clone(), cfg.enabled()))
            .collect();
        interpolate_spec(&mut spec)?;
        out.push(UpstreamConfig {
            name,
            spec,
            enabled,
            tool_defaults,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Persist a server's `enabled` flag back to the config file, preserving all
/// other content verbatim. Used by admin commands with `--make-default`.
pub fn set_enabled(path: &Path, name: &str, enabled: bool) -> Result<(), Error> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("cannot read config {}: {e}", path.display())))?;
    let mut root: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| Error::Config(format!("invalid config {}: {e}", path.display())))?;

    let server = root
        .get_mut("mcp")
        .and_then(|m| m.as_object_mut())
        .and_then(|m| m.get_mut(name))
        .and_then(|s| s.as_object_mut())
        .ok_or_else(|| Error::Config(format!("server {name:?} not found in {}", path.display())))?;
    server.insert("enabled".to_string(), serde_json::Value::Bool(enabled));

    let mut text = serde_json::to_string_pretty(&root)
        .map_err(|e| Error::Config(format!("serialize config failed: {e}")))?;
    text.push('\n');
    std::fs::write(path, text)
        .map_err(|e| Error::Config(format!("cannot write config {}: {e}", path.display())))?;
    Ok(())
}

/// Persist a tool's default `enabled` flag under `mcp[server].tools[tool]`,
/// preserving all other content verbatim. Creates the `tools` object if absent.
/// Used by admin commands with `--make-default` at the tool level.
pub fn set_tool_enabled(path: &Path, server: &str, tool: &str, enabled: bool) -> Result<(), Error> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("cannot read config {}: {e}", path.display())))?;
    let mut root: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| Error::Config(format!("invalid config {}: {e}", path.display())))?;

    let server_obj = root
        .get_mut("mcp")
        .and_then(|m| m.as_object_mut())
        .and_then(|m| m.get_mut(server))
        .and_then(|s| s.as_object_mut())
        .ok_or_else(|| {
            Error::Config(format!("server {server:?} not found in {}", path.display()))
        })?;

    let tools = server_obj
        .entry("tools")
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    let tools_obj = tools
        .as_object_mut()
        .ok_or_else(|| Error::Config(format!("mcp.{server}.tools is not an object")))?;
    tools_obj.insert(tool.to_string(), serde_json::json!({ "enabled": enabled }));

    let mut text = serde_json::to_string_pretty(&root)
        .map_err(|e| Error::Config(format!("serialize config failed: {e}")))?;
    text.push('\n');
    std::fs::write(path, text)
        .map_err(|e| Error::Config(format!("cannot write config {}: {e}", path.display())))?;
    Ok(())
}

fn interpolate_spec(spec: &mut ServerSpec) -> Result<(), Error> {
    match spec {
        ServerSpec::Local {
            command,
            environment,
            cwd,
            ..
        } => {
            for c in command.iter_mut() {
                *c = interpolate(c)?;
            }
            for v in environment.values_mut() {
                *v = interpolate(v)?;
            }
            if let Some(c) = cwd {
                *c = interpolate(c)?;
            }
        }
        ServerSpec::Remote {
            url,
            headers,
            oauth,
            ..
        } => {
            *url = interpolate(url)?;
            for v in headers.values_mut() {
                *v = interpolate(v)?;
            }
            if let Some(OAuthSetting::Explicit(cfg)) = oauth {
                if let Some(ref mut id) = cfg.client_id {
                    *id = interpolate(id)?;
                }
                if let Some(ref mut secret) = cfg.client_secret {
                    *secret = interpolate(secret)?;
                }
                if let Some(ref mut s) = cfg.scope {
                    *s = interpolate(s)?;
                }
                if let Some(ref mut uri) = cfg.redirect_uri {
                    *uri = interpolate(uri)?;
                }
            }
        }
    }
    Ok(())
}

/// Replace `{env:VAR}` occurrences with the value of `$VAR`. Missing vars are an
/// error so misconfiguration fails loudly at startup.
fn interpolate(s: &str) -> Result<String, Error> {
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("{env:") {
        result.push_str(&rest[..start]);
        let after = &rest[start + 5..];
        let end = after
            .find('}')
            .ok_or_else(|| Error::Config(format!("unterminated {{env:...}} in {s:?}")))?;
        let var = &after[..end];
        let val = std::env::var(var)
            .map_err(|_| Error::Config(format!("env var {var} referenced in config is not set")))?;
        result.push_str(&val);
        rest = &after[end + 1..];
    }
    result.push_str(rest);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_env() {
        std::env::set_var("CODEMCP_TEST_TOKEN", "abc123");
        let out = interpolate("Bearer {env:CODEMCP_TEST_TOKEN}").unwrap();
        assert_eq!(out, "Bearer abc123");
    }

    #[test]
    fn missing_env_errors() {
        assert!(interpolate("{env:CODEMCP_DEFINITELY_UNSET_XYZ}").is_err());
    }

    #[test]
    fn no_placeholder_passthrough() {
        assert_eq!(interpolate("plain").unwrap(), "plain");
    }

    #[test]
    fn tool_defaults_absent_means_enabled() {
        let json = r#"{ "mcp": { "s": { "type": "local", "command": ["x"],
            "tools": { "a": { "enabled": false }, "b": { "enabled": true } } } } }"#;
        let f: ConfigFile = serde_json::from_str(json).unwrap();
        let spec = &f.mcp["s"];
        assert!(!spec.tool_default_enabled("a"));
        assert!(spec.tool_default_enabled("b"));
        // Not listed => default enabled.
        assert!(spec.tool_default_enabled("c"));
    }

    #[test]
    fn tool_defaults_none_when_absent() {
        let json = r#"{ "mcp": { "s": { "type": "local", "command": ["x"] } } }"#;
        let f: ConfigFile = serde_json::from_str(json).unwrap();
        let spec = &f.mcp["s"];
        assert!(spec.tool_defaults().is_empty());
        assert!(spec.tool_default_enabled("anything"));
    }

    #[test]
    fn set_tool_enabled_creates_and_preserves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{ "mcp": { "github": { "type": "local", "command": ["npx"],
            "environment": { "K": "v" } } } }
"#,
        )
        .unwrap();
        set_tool_enabled(&path, "github", "create_issue", false).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let root: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // environment preserved, tools created with enabled=false.
        assert_eq!(
            root["mcp"]["github"]["environment"]["K"],
            serde_json::json!("v")
        );
        assert_eq!(
            root["mcp"]["github"]["tools"]["create_issue"]["enabled"],
            serde_json::json!(false)
        );

        // Flip to true, preserves the rest.
        set_tool_enabled(&path, "github", "create_issue", true).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let root: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            root["mcp"]["github"]["tools"]["create_issue"]["enabled"],
            serde_json::json!(true)
        );
        assert_eq!(
            root["mcp"]["github"]["environment"]["K"],
            serde_json::json!("v")
        );
    }

    #[test]
    fn set_tool_enabled_unknown_server_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{ "mcp": {} }
"#,
        )
        .unwrap();
        assert!(set_tool_enabled(&path, "nope", "t", true).is_err());
    }
}
