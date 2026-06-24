//! Connect to a single upstream MCP server (stdio or streamable-http).
//!
//! Remote (HTTP) servers may require OAuth 2.1 authentication. The connection
//! logic auto-detects this: if a server has no `Authorization` header and OAuth
//! is not explicitly disabled, the client attempts OAuth discovery. Servers that
//! support OAuth but have no stored tokens are reported as `NeedsAuth` so the
//! user can run `codemcp auth <name>`.

use std::collections::BTreeMap;
use std::time::Duration;

use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{
    streamable_http_client::StreamableHttpClientTransportConfig, IntoTransport,
    StreamableHttpClientTransport, TokioChildProcess,
};
use rmcp::ServiceExt;
use tokio::process::Command;

use crate::auth::{self, AuthConnectError, AuthStatus};
use crate::config::{OAuthSetting, ServerSpec};
use crate::error::Error;

/// A live connection to one upstream server. The unit handler `()` is a valid
/// client that just doesn't react to server-initiated requests.
pub type UpstreamService = RunningService<RoleClient, ()>;

/// Default time to wait for an upstream to spawn and complete the MCP
/// handshake when the config does not specify a `timeout`.
pub(crate) const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 30;

/// The outcome of a connection attempt: the service (if connected), the auth
/// status, and an optional error message.
pub struct ConnectResult {
    /// The connected service, if the connection succeeded.
    pub service: Option<UpstreamService>,
    /// Auth status for this server.
    pub auth_status: AuthStatus,
    /// Error message if the connection failed for a non-auth reason.
    pub error: Option<String>,
}

impl ConnectResult {
    fn connected(service: UpstreamService, auth_status: AuthStatus) -> Self {
        Self {
            service: Some(service),
            auth_status,
            error: None,
        }
    }

    fn failed(msg: impl Into<String>, auth_status: AuthStatus) -> Self {
        Self {
            service: None,
            auth_status,
            error: Some(msg.into()),
        }
    }
}

/// Connect to the upstream described by `spec`.
pub(crate) async fn connect(name: &str, spec: &ServerSpec) -> ConnectResult {
    match spec {
        ServerSpec::Local {
            command,
            environment,
            cwd,
            timeout,
            ..
        } => connect_stdio(name, command, environment, cwd.as_deref(), *timeout).await,
        ServerSpec::Remote {
            url,
            headers,
            timeout,
            oauth,
            ..
        } => connect_http(name, url, headers, oauth.as_ref(), *timeout).await,
    }
}

/// Drive the MCP handshake to completion, failing if it does not finish within
/// the configured (or default) timeout.
async fn serve_with_timeout<T, E, A>(
    name: &str,
    transport: T,
    timeout: Option<u64>,
) -> Result<UpstreamService, Error>
where
    T: IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let secs = timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS);
    let fut = ().serve(transport);
    match tokio::time::timeout(Duration::from_secs(secs), fut).await {
        Ok(Ok(service)) => Ok(service),
        Ok(Err(e)) => Err(Error::Upstream(format!("{name}: initialize failed: {e}"))),
        Err(_) => Err(Error::Upstream(format!(
            "{name}: timed out after {secs}s waiting for the server to initialize"
        ))),
    }
}

async fn connect_stdio(
    name: &str,
    command: &[String],
    environment: &BTreeMap<String, String>,
    cwd: Option<&str>,
    timeout: Option<u64>,
) -> ConnectResult {
    let (program, args) = match command.split_first() {
        Some(pa) => pa,
        None => {
            return ConnectResult::failed(
                format!("upstream {name}: empty command"),
                AuthStatus::NotApplicable,
            )
        }
    };

    let mut cmd = Command::new(program);
    cmd.args(args);
    for (k, v) in environment {
        cmd.env(k, v);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let transport = match TokioChildProcess::new(cmd) {
        Ok(t) => t,
        Err(e) => {
            return ConnectResult::failed(
                format!("{name}: spawn failed: {e}"),
                AuthStatus::NotApplicable,
            )
        }
    };

    match serve_with_timeout(name, transport, timeout).await {
        Ok(service) => ConnectResult::connected(service, AuthStatus::NotApplicable),
        Err(e) => ConnectResult::failed(e.to_string(), AuthStatus::NotApplicable),
    }
}

/// Connect to a remote (HTTP) MCP server.
///
/// Auth logic (mirrors opencode's auto-detect):
/// 1. If OAuth is explicitly disabled (`oauth: false`) or an `Authorization`
///    header is present → plain connect with headers (no OAuth).
/// 2. If stored OAuth tokens exist → connect via `AuthClient` (auto-refresh).
/// 3. If no stored tokens → try plain connect first (fast path for open
///    servers). If that fails, check OAuth support:
///    - Supports OAuth → `NeedsAuth` (user must run `codemcp auth <name>`).
///    - Doesn't support OAuth → report the original failure.
async fn connect_http(
    name: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    oauth: Option<&OAuthSetting>,
    timeout: Option<u64>,
) -> ConnectResult {
    let oauth_disabled = oauth.is_some_and(|o| o.is_disabled());
    let has_auth_header =
        headers.contains_key("Authorization") || headers.contains_key("authorization");

    // Path 1: OAuth disabled or bearer header → plain connect.
    if oauth_disabled || has_auth_header {
        return plain_http_connect(name, url, headers, timeout).await;
    }

    // Path 2: Check for stored OAuth tokens.
    let oauth_config = oauth.and_then(|o| o.config());
    let has_stored_tokens = auth::store::load_entry(name, url)
        .map(|t| t.is_some())
        .unwrap_or(false);

    if has_stored_tokens {
        // Try connecting with stored tokens (auto-refresh if expired).
        match auth::connect_with_stored_tokens(name, url, headers, oauth_config, timeout).await {
            Ok(service) => return ConnectResult::connected(service, AuthStatus::Authenticated),
            Err(AuthConnectError::Expired) => {
                // Tokens expired and couldn't be refreshed → needs re-auth.
                return ConnectResult::failed(
                    format!("{name}: OAuth tokens expired. Run: codemcp auth {name}"),
                    AuthStatus::NeedsAuth,
                );
            }
            Err(AuthConnectError::NeedsAuth) => {
                return ConnectResult::failed(
                    format!("Authentication required. Run: codemcp auth {name}"),
                    AuthStatus::NeedsAuth,
                );
            }
            Err(AuthConnectError::NotOAuth) => {
                // Server doesn't support OAuth after all → fall through to
                // plain connect (might work without auth).
            }
            Err(AuthConnectError::Failed(msg)) => {
                return ConnectResult::failed(msg, AuthStatus::NotApplicable);
            }
        }
    }

    // Path 3: No stored tokens. Try plain connect first (fast path).
    match plain_http_connect(name, url, headers, timeout).await {
        ok @ ConnectResult {
            service: Some(_), ..
        } => ok,
        // Plain connect failed. Check if the server supports OAuth.
        ConnectResult {
            service: None,
            error,
            ..
        } => {
            if auth::check_oauth_support(url).await {
                // Server supports OAuth but we have no tokens.
                ConnectResult::failed(
                    format!("Authentication required. Run: codemcp auth {name}"),
                    AuthStatus::NeedsAuth,
                )
            } else {
                // Server doesn't support OAuth. Report the original error.
                let msg = error.unwrap_or_else(|| format!("{name}: connection failed"));
                ConnectResult::failed(msg, AuthStatus::NotOAuth)
            }
        }
    }
}

/// Plain HTTP connect (no OAuth), with optional custom headers.
async fn plain_http_connect(
    name: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    timeout: Option<u64>,
) -> ConnectResult {
    let transport = if headers.is_empty() {
        StreamableHttpClientTransport::from_uri(url.to_string())
    } else {
        let mut header_map = reqwest::header::HeaderMap::new();
        for (k, v) in headers {
            let name_hdr = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| Error::Config(format!("{name}: invalid header name {k:?}: {e}")));
            let val_hdr = reqwest::header::HeaderValue::from_str(v)
                .map_err(|e| Error::Config(format!("{name}: invalid header value for {k:?}: {e}")));
            match (name_hdr, val_hdr) {
                (Ok(n), Ok(vh)) => {
                    header_map.insert(n, vh);
                }
                (Err(e), _) | (_, Err(e)) => {
                    return ConnectResult::failed(e.to_string(), AuthStatus::NotApplicable)
                }
            }
        }
        let client = match reqwest::Client::builder()
            .default_headers(header_map)
            .pool_max_idle_per_host(0)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                return ConnectResult::failed(
                    format!("{name}: http client build failed: {e}"),
                    AuthStatus::NotApplicable,
                )
            }
        };
        let mut config = StreamableHttpClientTransportConfig::default();
        config.uri = url.to_string().into();
        StreamableHttpClientTransport::with_client(client, config)
    };

    match serve_with_timeout(name, transport, timeout).await {
        Ok(service) => ConnectResult::connected(service, AuthStatus::NotApplicable),
        Err(e) => ConnectResult::failed(e.to_string(), AuthStatus::NotApplicable),
    }
}
