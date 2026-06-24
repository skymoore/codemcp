//! File-backed OAuth credential storage.
//!
//! Tokens are persisted to `~/.config/codemcp/mcp-auth.json` (mode 0600), keyed
//! by server name. This mirrors opencode's `mcp-auth.json`. The file is shared
//! across all upstreams, so writes are serialized with an advisory file lock.
//!
//! Implements rmcp's `CredentialStore` trait so the SDK automatically persists
//! tokens during login (`exchange_code_for_token`) and refresh (`refresh_token`),
//! and loads them during reconnect (`initialize_from_store`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use oauth2::TokenResponse;
use rmcp::transport::auth::{AuthError, CredentialStore, StoredCredentials};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::AuthStatus;
use crate::env::config_base;

/// The on-disk path for the shared OAuth token file.
pub fn auth_file_path() -> PathBuf {
    config_base().join("codemcp").join("mcp-auth.json")
}

/// One server's stored auth data: the rmcp credentials plus the server URL
/// (to validate that cached tokens are for the current server, not a stale one).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEntry {
    /// The remote server URL these credentials were obtained for. Used to
    /// invalidate cached tokens when the server URL changes in config.
    pub server_url: String,
    /// The rmcp credential payload (client_id, token_response, scopes, etc.).
    /// Serialized as an opaque JSON value because `StoredCredentials` contains
    /// `OAuthTokenResponse` (from the `oauth2` crate) which is serde-compatible
    /// but not directly constructible outside the SDK.
    pub credentials: Value,
}

/// The full file shape: server name → stored entry.
type AuthFile = BTreeMap<String, StoredEntry>;

/// Read the entire auth file. Returns an empty map if the file doesn't exist.
fn read_file(path: &Path) -> Result<AuthFile, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            if raw.trim().is_empty() {
                return Ok(BTreeMap::new());
            }
            serde_json::from_str(&raw).map_err(|e| format!("invalid mcp-auth.json: {e}"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Write the auth file atomically (temp + rename) with mode 0600.
fn write_file(path: &Path, data: &AuthFile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create dir failed: {e}"))?;
    }
    let raw = serde_json::to_string_pretty(data).map_err(|e| format!("serialize failed: {e}"))?;

    // Atomic write: write to a temp file in the same directory, then rename.
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("mcp-auth.json");
    let tmp = path
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!("{file_name}.tmp"));
    std::fs::write(&tmp, &raw).map_err(|e| format!("write tmp failed: {e}"))?;
    set_perms(&tmp);
    std::fs::rename(&tmp, path).map_err(|e| format!("rename failed: {e}"))
}

#[cfg(unix)]
fn set_perms(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_perms(_path: &Path) {}

/// Lock the auth file exclusively for the duration of `f`. Uses an advisory
/// lock on a sibling `.lock` file so concurrent gateway instances don't
/// corrupt the shared file.
fn with_lock<R>(path: &Path, f: impl FnOnce() -> Result<R, String>) -> Result<R, String> {
    let lock_path = path.with_extension("json.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("cannot open lock {}: {e}", lock_path.display()))?;

    #[cfg(unix)]
    {
        use fs4::fs_std::FileExt;
        file.lock_exclusive()
            .map_err(|e| format!("lock failed: {e}"))?;
        let result = f();
        let _ = file.unlock();
        result
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        f()
    }
}

/// Load the stored entry for a specific server. Returns `None` if the server
/// has no stored credentials or if the server URL doesn't match (stale tokens
/// from a different server).
pub fn load_entry(name: &str, server_url: &str) -> Result<Option<StoredCredentials>, String> {
    let path = auth_file_path();
    with_lock(&path, || {
        let data = read_file(&path)?;
        let entry = match data.get(name) {
            Some(e) => e,
            None => return Ok(None),
        };
        // Invalidate cached tokens if the server URL changed.
        if entry.server_url != server_url {
            return Ok(None);
        }
        let creds: StoredCredentials = serde_json::from_value(entry.credentials.clone())
            .map_err(|e| format!("cannot deserialize credentials for {name}: {e}"))?;
        Ok(Some(creds))
    })
}

/// Remove stored credentials for a server. Returns true if credentials existed.
pub fn remove_tokens(name: &str) -> Result<bool, String> {
    let path = auth_file_path();
    with_lock(&path, || {
        let mut data = read_file(&path)?;
        let existed = data.remove(name).is_some();
        if existed || path.exists() {
            write_file(&path, &data)?;
        }
        Ok(existed)
    })
}

/// Remove stored credentials for a server (alias matching the module-level API).
#[allow(dead_code)]
pub fn remove(name: &str) -> Result<bool, String> {
    remove_tokens(name)
}

/// Load all stored entries (for `codemcp auth list`).
#[allow(dead_code)]
pub fn load_all() -> Result<BTreeMap<String, StoredEntry>, String> {
    let path = auth_file_path();
    with_lock(&path, || read_file(&path))
}

/// Check the auth status for a server by inspecting stored credentials.
#[allow(dead_code)]
pub fn check_status(name: &str, server_url: &str) -> AuthStatus {
    match load_entry(name, server_url) {
        Ok(Some(creds)) => {
            if let Some(token_response) = &creds.token_response {
                // Check if the access token is expired.
                // `expires_in` is relative to `token_received_at`.
                if let Some(received_at) = creds.token_received_at {
                    if let Some(expires_in) = token_response.expires_in() {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        let expires_in_secs: u64 = expires_in.as_secs();
                        if now > received_at + expires_in_secs {
                            return AuthStatus::Expired;
                        }
                    }
                }
                AuthStatus::Authenticated
            } else {
                AuthStatus::NeedsAuth
            }
        }
        Ok(None) => AuthStatus::NeedsAuth,
        Err(_) => AuthStatus::NeedsAuth,
    }
}

/// A file-backed `CredentialStore` for a single upstream server.
///
/// One instance per upstream. The `name` and `server_url` identify which entry
/// in the shared `mcp-auth.json` to read/write. The rmcp SDK calls `load()`
/// during `initialize_from_store` (reconnect) and `save()` during
/// `exchange_code_for_token` (login) and `refresh_token` (refresh).
#[derive(Clone)]
pub struct FileCredentialStore {
    name: String,
    server_url: String,
}

impl FileCredentialStore {
    pub fn new(name: impl Into<String>, server_url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            server_url: server_url.into(),
        }
    }
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        load_entry(&self.name, &self.server_url)
            .map_err(|e| AuthError::InternalError(format!("credential load failed: {e}")))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let path = auth_file_path();
        let name = self.name.clone();
        let server_url = self.server_url.clone();
        with_lock(&path, || {
            let mut data = read_file(&path)?;
            let creds_value = serde_json::to_value(&credentials)
                .map_err(|e| format!("serialize credentials failed: {e}"))?;
            data.insert(
                name,
                StoredEntry {
                    server_url,
                    credentials: creds_value,
                },
            );
            write_file(&path, &data)
        })
        .map_err(|e| AuthError::InternalError(format!("credential save failed: {e}")))
    }

    async fn clear(&self) -> Result<(), AuthError> {
        let _ = remove_tokens(&self.name)
            .map_err(|e| AuthError::InternalError(format!("credential clear failed: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_auth_file(test_name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("codemcp-test-{}-{}", std::process::id(), test_name));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp-auth.json");
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn round_trip_credentials() {
        let path = tmp_auth_file("round_trip");

        let mut data: AuthFile = BTreeMap::new();
        let creds = serde_json::json!({
            "client_id": "test-client",
            "token_response": null,
            "granted_scopes": [],
            "token_received_at": null
        });
        data.insert(
            "test-server".to_string(),
            StoredEntry {
                server_url: "https://example.com/mcp".to_string(),
                credentials: creds,
            },
        );
        write_file(&path, &data).unwrap();

        let loaded = read_file(&path).unwrap();
        assert!(loaded.contains_key("test-server"));
        assert_eq!(loaded["test-server"].server_url, "https://example.com/mcp");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(perms & 0o077, 0o000, "file should be 0600");
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_returns_empty() {
        let path = std::env::temp_dir().join("nonexistent-codemcp-auth-test.json");
        let _ = std::fs::remove_file(&path);
        let data = read_file(&path).unwrap();
        assert!(data.is_empty());
    }

    #[test]
    fn url_mismatch_invalidates() {
        let path = tmp_auth_file("url_mismatch");

        let mut data: AuthFile = BTreeMap::new();
        data.insert(
            "server".to_string(),
            StoredEntry {
                server_url: "https://old.example.com/mcp".to_string(),
                credentials: serde_json::json!({
                    "client_id": "test",
                    "token_response": null,
                    "granted_scopes": [],
                    "token_received_at": null
                }),
            },
        );
        write_file(&path, &data).unwrap();

        let entry = data.get("server").unwrap();
        assert_ne!(entry.server_url, "https://new.example.com/mcp");

        let _ = std::fs::remove_file(&path);
    }
}
