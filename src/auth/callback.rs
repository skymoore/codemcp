//! Localhost OAuth callback server.
//!
//! Spins up a temporary axum HTTP server on `127.0.0.1` (ephemeral port by
//! default, or a configured port) that receives the OAuth `?code=&state=`
//! redirect. The `state` parameter is validated against the expected CSRF token
//! before the authorization code is accepted.
//!
//! Mirrors opencode's `oauth-callback.ts`: success/error HTML pages, state
//! validation, timeout, and clean shutdown when idle.

use std::sync::Arc;

use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex};

const HTML_SUCCESS: &str = r#"<!DOCTYPE html>
<html>
<head>
  <title>codemcp - Authorization Successful</title>
  <style>
    body { font-family: system-ui, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background: #1a1a2e; color: #eee; }
    .container { text-align: center; padding: 2rem; }
    h1 { color: #4ade80; margin-bottom: 1rem; }
    p { color: #aaa; }
  </style>
</head>
<body>
  <div class="container">
    <h1>Authorization Successful</h1>
    <p>You can close this window and return to your terminal.</p>
  </div>
  <script>setTimeout(() => window.close(), 2000);</script>
</body>
</html>"#;

fn html_error(error: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
  <title>codemcp - Authorization Failed</title>
  <style>
    body {{ font-family: system-ui, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background: #1a1a2e; color: #eee; }}
    .container {{ text-align: center; padding: 2rem; }}
    h1 {{ color: #f87171; margin-bottom: 1rem; }}
    p {{ color: #aaa; }}
    .error {{ color: #fca5a5; font-family: monospace; margin-top: 1rem; padding: 1rem; background: rgba(248,113,113,0.1); border-radius: 0.5rem; }}
  </style>
</head>
<body>
  <div class="container">
    <h1>Authorization Failed</h1>
    <p>An error occurred during authorization.</p>
    <div class="error">{}</div>
  </div>
</body>
</html>"#,
        html_escape(error)
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Query parameters received on the callback URL.
#[derive(serde::Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Shared state for the callback server: the expected CSRF token and a channel
/// to deliver the authorization code (or error) back to the login flow.
#[derive(Clone)]
struct CallbackState {
    expected_state: String,
    tx: Arc<Mutex<Option<oneshot::Sender<CallbackResult>>>>,
}

/// The result of the OAuth callback: either a code (success) or an error message.
pub enum CallbackResult {
    Code(String),
    Error(String),
}

/// A running callback server. Drop the guard to stop it.
pub struct CallbackServer {
    pub redirect_uri: String,
    #[allow(dead_code)]
    pub port: u16,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl CallbackServer {
    /// Stop the callback server (sends shutdown signal). The actual TCP listener
    /// closes when the axum task observes the signal.
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Start a callback server on `127.0.0.1`. If `port` is `Some`, bind to that
/// port; otherwise bind to an ephemeral port. The `expected_state` is the CSRF
/// token that the OAuth provider embedded in the authorization URL — the
/// callback validates it matches the `state` query parameter.
///
/// Returns the running server plus a receiver that resolves with the
/// authorization code (or an error) when the callback is hit.
pub async fn start(
    port: Option<u16>,
    path: &str,
    expected_state: String,
) -> Result<(CallbackServer, oneshot::Receiver<CallbackResult>), String> {
    let bind_addr = match port {
        Some(p) => format!("127.0.0.1:{p}"),
        None => "127.0.0.1:0".to_string(),
    };

    let listener = TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| format!("cannot bind callback server on {bind_addr}: {e}"))?;
    let actual_port = listener
        .local_addr()
        .map_err(|e| format!("cannot get bound port: {e}"))?
        .port();

    let (tx, rx) = oneshot::channel::<CallbackResult>();
    let state = CallbackState {
        expected_state,
        tx: Arc::new(Mutex::new(Some(tx))),
    };

    let redirect_uri = format!("http://127.0.0.1:{actual_port}{path}");

    let app = Router::new()
        .route(path, get(handle_callback))
        .with_state(state);

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let serve_fut = axum::serve(listener, app);
    tokio::spawn(async move {
        let _ = serve_fut
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    let server = CallbackServer {
        redirect_uri,
        port: actual_port,
        shutdown_tx: Some(shutdown_tx),
    };
    Ok((server, rx))
}

async fn handle_callback(
    Query(params): Query<CallbackParams>,
    axum::extract::State(state): axum::extract::State<CallbackState>,
) -> (StatusCode, Html<String>) {
    // Check for OAuth error response.
    if let Some(error) = params.error {
        let msg = params.error_description.unwrap_or(error);
        deliver_result(&state, CallbackResult::Error(msg.clone())).await;
        return (StatusCode::OK, Html(html_error(&msg)));
    }

    // Validate state parameter (CSRF protection).
    let state_param = match params.state {
        Some(s) if s == state.expected_state => s,
        Some(_) => {
            let msg = "Invalid or expired state parameter - potential CSRF attack";
            deliver_result(&state, CallbackResult::Error(msg.to_string())).await;
            return (StatusCode::BAD_REQUEST, Html(html_error(msg)));
        }
        None => {
            let msg = "Missing required state parameter - potential CSRF attack";
            deliver_result(&state, CallbackResult::Error(msg.to_string())).await;
            return (StatusCode::BAD_REQUEST, Html(html_error(msg)));
        }
    };
    let _ = state_param; // validated, not needed further

    // Extract the authorization code.
    let code = match params.code {
        Some(c) if !c.is_empty() => c,
        _ => {
            let msg = "No authorization code provided";
            deliver_result(&state, CallbackResult::Error(msg.to_string())).await;
            return (StatusCode::BAD_REQUEST, Html(html_error(msg)));
        }
    };

    deliver_result(&state, CallbackResult::Code(code)).await;
    (StatusCode::OK, Html(HTML_SUCCESS.to_string()))
}

/// Deliver the result to the waiting login flow (if still waiting).
async fn deliver_result(state: &CallbackState, result: CallbackResult) {
    let mut guard = state.tx.lock().await;
    if let Some(tx) = guard.take() {
        let _ = tx.send(result);
    }
}
