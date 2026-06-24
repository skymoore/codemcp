//! OAuth 2.1 authentication for remote MCP servers.
//!
//! Mirrors opencode's `opencode mcp auth` feature: detects when a remote MCP
//! server requires OAuth, informs the user, and runs an interactive browser
//! authorization flow via the admin interface.
//!
//! Built on rmcp's `auth` cargo feature (`OAuthState`, `AuthorizationManager`,
//! `AuthClient`). Token persistence is handled through rmcp's `CredentialStore`
//! trait — the SDK automatically saves tokens during login (`exchange_code_for_token`)
//! and refresh (`refresh_token`), and loads them during reconnect
//! (`initialize_from_store`).

pub mod callback;
pub mod login;
pub mod oauth_client;
pub mod store;

pub use login::{AuthStartResult, LoginHandle};
pub use oauth_client::{check_oauth_support, connect_with_stored_tokens, AuthConnectError};

use serde::{Deserialize, Serialize};

/// Per-server authentication status, shown by `codemcp list` and `codemcp auth list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthStatus {
    /// Server is connected and authenticated with valid OAuth tokens.
    Authenticated,
    /// Tokens exist but have expired and could not be refreshed.
    Expired,
    /// Server supports OAuth but no tokens are stored. User must run
    /// `codemcp auth <name>`.
    NeedsAuth,
    /// Server requires a pre-registered client ID (dynamic registration
    /// unsupported). User must provide `clientId` in config.
    NeedsClientRegistration,
    /// Server does not support OAuth (no authorization metadata found).
    NotOAuth,
    /// OAuth explicitly disabled in config; using headers/bearer instead.
    Disabled,
    /// Not applicable (local server, or connected without OAuth).
    NotApplicable,
}

impl AuthStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthStatus::Authenticated => "authenticated",
            AuthStatus::Expired => "expired",
            AuthStatus::NeedsAuth => "needs_auth",
            AuthStatus::NeedsClientRegistration => "needs_client_registration",
            AuthStatus::NotOAuth => "not_oauth",
            AuthStatus::Disabled => "disabled",
            AuthStatus::NotApplicable => "n/a",
        }
    }
}

impl std::fmt::Display for AuthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
