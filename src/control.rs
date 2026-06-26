//! Bidirectional JSON-RPC control channel over WebSocket.
//!
//! The gateway runs a WS server. The Python worker connects as a client and, as
//! its **first message**, sends the shared auth token. After authentication the
//! two peers exchange JSON-RPC 2.0 messages on the one connection:
//!
//! - gateway -> worker: `run { code }` -> `{ result, stdout, stderr }`
//! - worker -> gateway: `call_tool { server, tool, args }` -> tool result
//!
//! The gateway side exposes a [`ControlHandle`] that lets the executor issue
//! `run` requests and await their results, while incoming `call_tool` requests
//! are routed to the [`UpstreamManager`].

use std::collections::HashMap;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;

use crate::error::Error;
use crate::upstream::SharedUpstreams;

/// An observed tool-result value, emitted after a successful `call_tool` when
/// shape-learning is enabled. The runtime consumes these to learn (once per
/// tool) the shape of what a tool returns and surface it to the model.
#[derive(Debug, Clone)]
pub struct ShapeEvent {
    pub server: String,
    pub tool: String,
    /// The post-unwrap value the worker's Python actually sees.
    pub value: Value,
}

/// Sink for [`ShapeEvent`]s. `None` (the default) disables shape learning, so
/// the `call_tool` path stays byte-identical when the feature is off.
pub type ShapeSink = Option<mpsc::UnboundedSender<ShapeEvent>>;

/// Reduce a serialized `CallToolResult` to the value the worker's Python sees.
///
/// Mirrors `pyworker/bootstrap.py::_unwrap_tool_result`: prefer
/// `structuredContent`; else parse the joined text content as JSON; else fall
/// back to the raw value. Keeping this in lockstep with the worker means the
/// learned shape matches what the model will actually index into.
fn unwrap_tool_result(result: &Value) -> Value {
    let Some(obj) = result.as_object() else {
        return result.clone();
    };
    if let Some(sc) = obj.get("structuredContent") {
        if !sc.is_null() {
            return sc.clone();
        }
    }
    if let Some(Value::Array(content)) = obj.get("content") {
        let mut texts = Vec::new();
        for item in content {
            if item.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    texts.push(t);
                }
            }
        }
        if !texts.is_empty() {
            let joined = texts.join("\n");
            if let Ok(parsed) = serde_json::from_str::<Value>(&joined) {
                return parsed;
            }
            return Value::String(joined);
        }
    }
    result.clone()
}

/// Result of a `run` request.
#[derive(Debug, Clone, Deserialize)]
pub struct RunOutput {
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    /// Set by the worker when user code raised.
    #[serde(default)]
    pub error: Option<String>,
}

/// A pending `run` request awaiting its response.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<RunOutput, Error>>>>>;

/// Handle used by the executor to drive a connected worker.
#[derive(Clone)]
pub struct ControlHandle {
    /// Sends outbound WS text frames to the worker.
    outbound: mpsc::UnboundedSender<Message>,
    pending: Pending,
    next_id: Arc<std::sync::atomic::AtomicU64>,
}

impl ControlHandle {
    /// Send `run { code }` and await the worker's response.
    pub async fn run(&self, code: &str) -> Result<RunOutput, Error> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "run",
            "params": { "code": code }
        });
        self.outbound
            .send(Message::Text(msg.to_string()))
            .map_err(|_| Error::Worker("worker connection closed".into()))?;

        rx.await
            .map_err(|_| Error::Worker("worker dropped run request".into()))?
    }

    /// Push a new `sdk.py` to the worker and have it re-import + re-inject the
    /// SDK functions, without restarting the worker. Awaits the worker's ack.
    pub async fn reload(&self, sdk_py: &str) -> Result<(), Error> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "reload",
            "params": { "sdk": sdk_py }
        });
        self.outbound
            .send(Message::Text(msg.to_string()))
            .map_err(|_| Error::Worker("worker connection closed".into()))?;

        let out = rx
            .await
            .map_err(|_| Error::Worker("worker dropped reload request".into()))??;
        if let Some(err) = out.error {
            return Err(Error::Worker(format!("worker reload failed: {err}")));
        }
        Ok(())
    }

    /// Push the validation keyset map (`fn_name -> KeySet`, as JSON) to the
    /// worker. The worker stores it and uses it for strict pre-flight field
    /// validation. Awaits the worker's ack.
    pub async fn set_shapes(&self, shapes_json: Value) -> Result<(), Error> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "set_shapes",
            "params": { "shapes": shapes_json }
        });
        self.outbound
            .send(Message::Text(msg.to_string()))
            .map_err(|_| Error::Worker("worker connection closed".into()))?;

        let out = rx
            .await
            .map_err(|_| Error::Worker("worker dropped set_shapes request".into()))??;
        if let Some(err) = out.error {
            return Err(Error::Worker(format!("worker set_shapes failed: {err}")));
        }
        Ok(())
    }
}

/// The control server. Bind first to learn the actual port, then accept one
/// worker.
pub struct ControlServer {
    listener: TcpListener,
    token: String,
    upstreams: SharedUpstreams,
    shapes: ShapeSink,
}

impl ControlServer {
    pub async fn bind(
        bind_addr: &str,
        token: String,
        upstreams: SharedUpstreams,
        shapes: ShapeSink,
    ) -> Result<Self, Error> {
        let listener = TcpListener::bind(bind_addr)
            .await
            .map_err(|e| Error::Other(format!("control bind {bind_addr} failed: {e}")))?;
        Ok(Self {
            listener,
            token,
            upstreams,
            shapes,
        })
    }

    /// The actual bound address (resolves ephemeral `:0` ports).
    pub fn local_addr(&self) -> Result<std::net::SocketAddr, Error> {
        self.listener
            .local_addr()
            .map_err(|e| Error::Other(format!("control local_addr failed: {e}")))
    }

    /// Accept exactly one worker, authenticate it, and return a handle plus the
    /// background task driving the connection.
    pub async fn accept_worker(self) -> Result<ControlHandle, Error> {
        let (stream, peer) = self
            .listener
            .accept()
            .await
            .map_err(|e| Error::Worker(format!("accept failed: {e}")))?;
        tracing::debug!(%peer, "worker tcp connected");

        let ws = tokio_tungstenite::accept_async(stream)
            .await
            .map_err(|e| Error::Worker(format!("websocket handshake failed: {e}")))?;

        let (mut ws_tx, mut ws_rx) = ws.split();

        // First message must be the auth token.
        let first = ws_rx
            .next()
            .await
            .ok_or_else(|| Error::Worker("worker closed before auth".into()))?
            .map_err(|e| Error::Worker(format!("auth read failed: {e}")))?;
        let provided = match first {
            Message::Text(t) => t.to_string(),
            _ => return Err(Error::Worker("auth frame was not text".into())),
        };
        if provided != self.token {
            let _ = ws_tx.send(Message::Close(None)).await;
            return Err(Error::Worker("worker auth token mismatch".into()));
        }
        tracing::info!("worker authenticated");

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Message>();

        // Writer task: forward outbound frames to the worker.
        tokio::spawn(async move {
            while let Some(msg) = outbound_rx.recv().await {
                if ws_tx.send(msg).await.is_err() {
                    break;
                }
            }
        });

        // Reader task: dispatch incoming frames (run responses + call_tool reqs).
        let upstreams = self.upstreams.clone();
        let shapes = self.shapes.clone();
        let pending_reader = pending.clone();
        let outbound_for_reader = outbound_tx.clone();
        tokio::spawn(async move {
            while let Some(frame) = ws_rx.next().await {
                let text = match frame {
                    Ok(Message::Text(t)) => t.to_string(),
                    Ok(Message::Close(_)) => {
                        tracing::info!("worker closed connection");
                        break;
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        tracing::warn!(error = %e, "worker read error");
                        break;
                    }
                };
                // `call_tool` requests are dispatched on their own task so the
                // read loop keeps draining frames. This lets a worker fire many
                // tool calls in a burst and have their upstream round-trips
                // overlap instead of serializing one frame at a time. `run`
                // responses are routed inline (cheap, no I/O).
                if is_call_tool(&text) {
                    let upstreams = upstreams.clone();
                    let outbound = outbound_for_reader.clone();
                    let shapes = shapes.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_call_tool(&text, &upstreams, &outbound, &shapes).await
                        {
                            tracing::warn!(error = %e, "error handling call_tool frame");
                        }
                    });
                } else if let Err(e) =
                    handle_incoming(&text, &pending_reader, &upstreams, &outbound_for_reader).await
                {
                    tracing::warn!(error = %e, "error handling worker frame");
                }
            }
            // Connection gone: fail all pending run requests.
            let mut p = pending_reader.lock().await;
            for (_, tx) in p.drain() {
                let _ = tx.send(Err(Error::Worker("worker connection lost".into())));
            }
        });

        Ok(ControlHandle {
            outbound: outbound_tx,
            pending,
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        })
    }
}

#[derive(Deserialize)]
struct CallToolParams {
    server: String,
    tool: String,
    #[serde(default)]
    args: serde_json::Map<String, Value>,
}

#[derive(Serialize)]
struct JsonRpcResponse<'a> {
    jsonrpc: &'a str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

/// Cheap check for whether a frame is a `call_tool` request, so the reader can
/// route it to a spawned task without fully parsing/handling it inline.
fn is_call_tool(text: &str) -> bool {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => v.get("method").and_then(Value::as_str) == Some("call_tool"),
        Err(_) => false,
    }
}

/// Dispatch an incoming JSON-RPC frame from the worker (run responses + any
/// non-`call_tool` request). `call_tool` requests are handled separately by
/// [`handle_call_tool`] so they can run concurrently.
async fn handle_incoming(
    text: &str,
    pending: &Pending,
    _upstreams: &SharedUpstreams,
    outbound: &mpsc::UnboundedSender<Message>,
) -> Result<(), Error> {
    let msg: Value = serde_json::from_str(text)?;

    // Is this a response to one of our `run` requests? (has "result"/"error"
    // and a numeric id, no "method")
    if msg.get("method").is_none() {
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            if let Some(tx) = pending.lock().await.remove(&id) {
                if let Some(err) = msg.get("error") {
                    let _ = tx.send(Err(Error::Worker(err.to_string())));
                } else {
                    let out: RunOutput =
                        serde_json::from_value(msg.get("result").cloned().unwrap_or(Value::Null))
                            .unwrap_or(RunOutput {
                                result: Value::Null,
                                stdout: String::new(),
                                stderr: String::new(),
                                error: Some("malformed run result".into()),
                            });
                    let _ = tx.send(Ok(out));
                }
            }
        }
        return Ok(());
    }

    // Otherwise it's a request from the worker. `call_tool` is handled by the
    // caller via a spawned task; anything else is unknown.
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    tracing::warn!(method = %method, "unknown method from worker");
    let response = JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(json!({ "code": -32601, "message": "method not found" })),
    };
    let text = serde_json::to_string(&response)?;
    let _ = outbound.send(Message::Text(text));
    Ok(())
}

/// Forward a single `call_tool` request to the upstream and send back its
/// response. Runs on its own task so concurrent calls overlap.
async fn handle_call_tool(
    text: &str,
    upstreams: &SharedUpstreams,
    outbound: &mpsc::UnboundedSender<Message>,
    shapes: &ShapeSink,
) -> Result<(), Error> {
    let msg: Value = serde_json::from_str(text)?;
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let params: CallToolParams =
        serde_json::from_value(msg.get("params").cloned().unwrap_or(Value::Null))?;

    let response = match upstreams
        .call_tool(&params.server, &params.tool, Some(params.args))
        .await
    {
        Ok(result) => {
            let value = serde_json::to_value(&result).unwrap_or(Value::Null);
            // Emit the (unwrapped) result shape for learning. Best-effort and
            // non-blocking: a closed/absent sink is simply ignored. Skip results
            // the upstream marked as errors so we don't learn an error shape.
            if let Some(tx) = shapes {
                let is_error = value
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !is_error {
                    let _ = tx.send(ShapeEvent {
                        server: params.server.clone(),
                        tool: params.tool.clone(),
                        value: unwrap_tool_result(&value),
                    });
                }
            }
            JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(value),
                error: None,
            }
        }
        Err(e) => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(json!({ "code": -32000, "message": e.to_string() })),
        },
    };
    let text = serde_json::to_string(&response)?;
    outbound
        .send(Message::Text(text))
        .map_err(|_| Error::Worker("cannot send call_tool response".into()))?;
    Ok(())
}
