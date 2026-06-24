//! Building authenticated MCP transports from stored OAuth tokens, and
//! detecting whether a remote server supports OAuth at all.
//!
//! The core insight is that rmcp's `CredentialStore` trait is the single
//! persistence integration point: the SDK auto-saves tokens during login and
//! refresh, and auto-loads them during reconnect. We just need to provide a
//! `FileCredentialStore` and wire it into an `AuthorizationManager`.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rmcp::transport::auth::{AuthClient, AuthError, AuthorizationManager};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::ServiceExt;

use crate::auth::store::FileCredentialStore;
use crate::config::OAuthConfig;
use crate::error::Error;

/// The outcome of attempting an OAuth-based connection.
#[derive(Debug)]
pub enum AuthConnectError {
    /// Server supports OAuth but no valid tokens are stored.
    /// User must run `codemcp auth <name>`.
    NeedsAuth,
    /// Server does not support OAuth (no authorization metadata found).
    NotOAuth,
    /// Tokens exist but are expired and could not be refreshed.
    Expired,
    /// Connection failed for a non-auth reason.
    Failed(String),
}

/// Check whether a remote server supports OAuth by attempting metadata
/// discovery. Returns `true` if discovery succeeds, `false` if the server
/// returns `NoAuthorizationSupport` or any other error.
pub async fn check_oauth_support(url: &str) -> bool {
    match AuthorizationManager::new(url.to_string()).await {
        Ok(manager) => manager.discover_metadata().await.is_ok(),
        Err(_) => false,
    }
}

/// Build a `reqwest::Client` with custom default headers (Authorization, etc.)
/// for use as both the OAuth metadata-discovery client and the MCP transport
/// client.
fn build_reqwest_client(
    url: &str,
    headers: &BTreeMap<String, String>,
) -> Result<reqwest::Client, Error> {
    let mut header_map = HeaderMap::new();
    for (k, v) in headers {
        let name_hdr = HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| Error::Config(format!("invalid header name {k:?}: {e}")))?;
        let val_hdr = HeaderValue::from_str(v)
            .map_err(|e| Error::Config(format!("invalid header value for {k:?}: {e}")))?;
        header_map.insert(name_hdr, val_hdr);
    }
    reqwest::Client::builder()
        .default_headers(header_map)
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|e| Error::Upstream(format!("{url}: http client build failed: {e}")))
}

/// Attempt to connect to a remote server using stored OAuth tokens.
///
/// This is the "reconnect with cached credentials" path. It:
/// 1. Creates an `AuthorizationManager` with a `FileCredentialStore`.
/// 2. Loads stored tokens via `initialize_from_store`.
/// 3. If tokens are valid, builds an `AuthClient` transport and connects.
/// 4. The `AuthClient` auto-refreshes expired tokens transparently.
///
/// Returns the connected service on success, or an `AuthConnectError`
/// indicating why it failed.
pub async fn connect_with_stored_tokens(
    name: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    oauth_config: Option<&OAuthConfig>,
    timeout: Option<u64>,
) -> Result<rmcp::service::RunningService<rmcp::service::RoleClient, ()>, AuthConnectError> {
    let client =
        build_reqwest_client(url, headers).map_err(|e| AuthConnectError::Failed(e.to_string()))?;

    // Build the authorization manager with our file-backed credential store.
    let mut manager = AuthorizationManager::new(url.to_string())
        .await
        .map_err(|e| classify_auth_error(name, &e))?;

    manager
        .with_client(client.clone())
        .map_err(|e| AuthConnectError::Failed(e.to_string()))?;

    manager.set_credential_store(FileCredentialStore::new(name, url));

    // If a pre-registered client ID is configured, set it before loading tokens.
    if let Some(cfg) = oauth_config {
        if let Some(ref id) = cfg.client_id {
            manager
                .configure_client_id(id)
                .map_err(|e| AuthConnectError::Failed(e.to_string()))?;
        }
    }

    // Load stored credentials. This calls discover_metadata internally if
    // tokens are present, so it may return NoAuthorizationSupport.
    let has_credentials = manager
        .initialize_from_store()
        .await
        .map_err(|e| classify_auth_error(name, &e))?;

    if !has_credentials {
        // No stored tokens (or token_response is None).
        // Check if the server supports OAuth at all.
        if !check_oauth_support(url).await {
            return Err(AuthConnectError::NotOAuth);
        }
        return Err(AuthConnectError::NeedsAuth);
    }

    // Build the authenticated transport.
    let auth_client = AuthClient::new(client, manager);
    let mut config = StreamableHttpClientTransportConfig::default();
    config.uri = url.to_string().into();
    let transport = StreamableHttpClientTransport::with_client(auth_client, config);

    // Connect with timeout.
    let secs = timeout.unwrap_or(crate::upstream::DEFAULT_CONNECT_TIMEOUT_SECS);
    let fut = ().serve(transport);
    match tokio::time::timeout(Duration::from_secs(secs), fut).await {
        Ok(Ok(service)) => Ok(service),
        Ok(Err(e)) => {
            // Check if it's an auth-related error (expired/invalid tokens).
            let msg = e.to_string();
            if msg.contains("AuthorizationRequired")
                || msg.contains("TokenExpired")
                || msg.contains("authorization")
                || msg.contains("401")
            {
                Err(AuthConnectError::Expired)
            } else {
                Err(AuthConnectError::Failed(format!("{name}: {msg}")))
            }
        }
        Err(_) => Err(AuthConnectError::Failed(format!(
            "{name}: timed out after {secs}s"
        ))),
    }
}

/// Classify an `AuthError` into an `AuthConnectError`.
fn classify_auth_error(name: &str, e: &AuthError) -> AuthConnectError {
    match e {
        AuthError::NoAuthorizationSupport => AuthConnectError::NotOAuth,
        AuthError::AuthorizationRequired | AuthError::TokenExpired => AuthConnectError::Expired,
        AuthError::RegistrationFailed(msg) => AuthConnectError::Failed(format!(
            "{name}: server does not support dynamic client registration. \
                 Add clientId to your config. ({msg})"
        )),
        _ => AuthConnectError::Failed(format!("{name}: {e}")),
    }
}
