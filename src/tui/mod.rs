//! Interactive terminal UI (admin client) for enabling/disabling upstream
//! servers and individual tools against a running gateway.
//!
//! Talks to the gateway's admin Unix socket (the same protocol used by
//! `codemcp list/enable/disable`). Two panes: servers (left) and the selected
//! server's tools (right). `Space` toggles a session-only state; `D` toggles
//! and persists the default to `mcp.json`.
//!
//! The TUI is a client only — it never runs inside the gateway process, so it
//! cannot disturb the MCP stdio/HTTP terminal.
//!
//! The full implementation requires the `tui` cargo feature (ratatui +
//! crossterm). Without it, `run` returns an error.

#[cfg(feature = "tui")]
mod ui;

#[cfg(feature = "tui")]
use std::time::Duration;

#[cfg(feature = "tui")]
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
#[cfg(feature = "tui")]
use ratatui::DefaultTerminal;
#[cfg(feature = "tui")]
use serde_json::{json, Value};
#[cfg(feature = "tui")]
use tokio::sync::mpsc;
#[cfg(feature = "tui")]
use tokio::time::{self, MissedTickBehavior};

#[cfg(feature = "tui")]
use crate::admin::{self, Instance};
use crate::error::Error;

/// Entry point: pick a gateway instance, take over the terminal, run the UI.
pub async fn run(selector: Option<&str>) -> Result<(), Error> {
    #[cfg(feature = "tui")]
    {
        return run_tui(selector).await;
    }

    #[cfg(not(feature = "tui"))]
    {
        let _ = selector;
        Err(Error::Other(
            "codemcp was not built with the 'tui' feature. Rebuild with: cargo build --features tui".to_string(),
        ))
    }
}

#[cfg(feature = "tui")]
async fn run_tui(selector: Option<&str>) -> Result<(), Error> {
    let instance = admin::select_instance(selector).await?;

    // Restore the terminal even if a panic escapes the app loop.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = ratatui::try_restore();
        prev_hook(info);
    }));

    let terminal = ratatui::init();
    let res = run_app(terminal, instance).await;
    ratatui::restore();
    res
}

#[cfg(feature = "tui")]
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "tui")]
enum Pane {
    Servers,
    Tools,
}

#[derive(Clone)]
#[cfg(feature = "tui")]
struct ServerRow {
    name: String,
    kind: String,
    default: bool,
    connected: bool,
    tools: u64,
}

#[derive(Clone)]
#[cfg(feature = "tui")]
#[allow(dead_code)]
struct ToolRow {
    server: String,
    tool: String,
    summary: String,
    connected: bool,
    enabled: bool,
    default: bool,
    session: Option<bool>,
}

#[cfg(feature = "tui")]
struct App {
    instance: Instance,
    servers: Vec<ServerRow>,
    tools: Vec<ToolRow>,
    pane: Pane,
    server_sel: usize,
    tool_sel: usize,
    message: String,
    error: Option<String>,
    show_help: bool,
    quit: bool,
}

#[cfg(feature = "tui")]
impl App {
    fn selected_server_name(&self) -> Option<&str> {
        self.servers.get(self.server_sel).map(|s| s.name.as_str())
    }

    /// Tools belonging to the currently selected server (references into
    /// `self.tools`), in display order.
    fn tools_for_selected(&self) -> Vec<&ToolRow> {
        let Some(name) = self.selected_server_name() else {
            return Vec::new();
        };
        self.tools.iter().filter(|t| t.server == name).collect()
    }
}

#[cfg(feature = "tui")]
async fn run_app(mut terminal: DefaultTerminal, instance: Instance) -> Result<(), Error> {
    let mut app = App {
        instance,
        servers: Vec::new(),
        tools: Vec::new(),
        pane: Pane::Servers,
        server_sel: 0,
        tool_sel: 0,
        message: String::from("ready"),
        error: None,
        show_help: false,
        quit: false,
    };
    refresh(&mut app).await;

    // Forward crossterm events from a blocking reader into an async channel so
    // the draw loop can `select!` over events, the refresh ticker, and admin
    // calls. Polling with a short timeout lets the task notice receiver drop.
    let (tx, mut rx) = mpsc::channel::<Event>(64);
    tokio::task::spawn_blocking(move || loop {
        if event::poll(Duration::from_millis(250)).unwrap_or(false) {
            if let Ok(ev) = event::read() {
                if tx.blocking_send(ev).is_err() {
                    break;
                }
            }
        } else if tx.is_closed() {
            break;
        }
    });

    let mut refresh_ticker = time::interval(Duration::from_secs(3));
    refresh_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Don't fire immediately (we just refreshed); the first tick is delayed.
    refresh_ticker.tick().await;

    while !app.quit {
        terminal.draw(|frame| ui::render(frame, &app))?;

        tokio::select! {
            Some(ev) = rx.recv() => handle_event(&mut app, ev).await,
            _ = refresh_ticker.tick() => refresh(&mut app).await,
        }
    }
    Ok(())
}

#[cfg(feature = "tui")]
async fn handle_event(app: &mut App, ev: Event) {
    let Event::Key(k) = ev else { return };
    if k.kind != KeyEventKind::Press {
        return;
    }

    // Global keys take priority.
    if matches!(k.code, KeyCode::Char('?')) {
        app.show_help = !app.show_help;
        return;
    }
    if matches!(k.code, KeyCode::Char('q'))
        || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL))
    {
        app.quit = true;
        return;
    }
    if app.show_help {
        // While help is up, only `?`/`q`/Ctrl-C act (handled above); ignore the rest.
        return;
    }

    match k.code {
        KeyCode::Tab | KeyCode::BackTab => {
            app.pane = match app.pane {
                Pane::Servers => Pane::Tools,
                Pane::Tools => Pane::Servers,
            };
        }
        KeyCode::Down | KeyCode::Char('j') => move_sel(app, 1),
        KeyCode::Up | KeyCode::Char('k') => move_sel(app, -1),
        KeyCode::Char(' ') | KeyCode::Enter => toggle(app, false).await,
        KeyCode::Char('D') => toggle(app, true).await,
        KeyCode::Char('r') | KeyCode::Char('R') => refresh(app).await,
        _ => {}
    }
}

#[cfg(feature = "tui")]
fn move_sel(app: &mut App, delta: i32) {
    match app.pane {
        Pane::Servers => {
            let n = app.servers.len();
            if n == 0 {
                app.server_sel = 0;
                return;
            }
            app.server_sel = wrap(app.server_sel as i32 + delta, n as i32);
            // Reset tool selection when the focused server changes.
            app.tool_sel = 0;
        }
        Pane::Tools => {
            let cnt = app.tools_for_selected().len();
            if cnt == 0 {
                app.tool_sel = 0;
                return;
            }
            app.tool_sel = wrap(app.tool_sel as i32 + delta, cnt as i32);
        }
    }
}

#[cfg(feature = "tui")]
fn wrap(i: i32, n: i32) -> usize {
    debug_assert!(n > 0);
    i.rem_euclid(n) as usize
}

/// Toggle the effective enabled state of the focused server or tool. When
/// `make_default` is set, the change is also persisted to `mcp.json`.
#[cfg(feature = "tui")]
async fn toggle(app: &mut App, make_default: bool) {
    app.error = None;
    match app.pane {
        Pane::Servers => {
            let Some(s) = app.servers.get(app.server_sel) else {
                return;
            };
            let name = s.name.clone();
            let was_connected = s.connected;
            let method = if was_connected { "disable" } else { "enable" };
            let params = json!({ "name": name, "make_default": make_default });
            do_action(app, method, params, || {
                format!(
                    "{name} {} [{}]",
                    if was_connected { "disabled" } else { "enabled" },
                    if make_default { "default" } else { "session" }
                )
            })
            .await;
        }
        Pane::Tools => {
            let tools = app.tools_for_selected();
            let Some(t) = tools.get(app.tool_sel) else {
                return;
            };
            let server = t.server.clone();
            let tool = t.tool.clone();
            let was_enabled = t.enabled;
            let method = if was_enabled {
                "disable_tool"
            } else {
                "enable_tool"
            };
            let params = json!({ "server": server, "tool": tool, "make_default": make_default });
            do_action(app, method, params, || {
                format!(
                    "{server}/{tool} {} [{}]",
                    if was_enabled { "disabled" } else { "enabled" },
                    if make_default { "default" } else { "session" }
                )
            })
            .await;
        }
    }
}

/// Send an admin request, surface any error, and on success set a message and
/// refresh the model. `msg` is computed only on success.
#[cfg(feature = "tui")]
async fn do_action<F: FnOnce() -> String>(app: &mut App, method: &str, params: Value, msg: F) {
    match admin::client_request_on(&app.instance.socket, method, params).await {
        Ok(v) => {
            if let Some(err) = v.get("error").and_then(Value::as_str) {
                app.error = Some(err.to_string());
                app.message.clear();
            } else {
                app.message = msg();
                refresh(app).await;
            }
        }
        Err(e) => {
            app.error = Some(e.to_string());
            app.message.clear();
        }
    }
}

/// Re-fetch server + tool status from the gateway and re-clamp selections.
#[cfg(feature = "tui")]
async fn refresh(app: &mut App) {
    match admin::client_request_on(&app.instance.socket, "list", json!({})).await {
        Ok(v) => {
            if let Some(err) = v.get("error").and_then(Value::as_str) {
                app.error = Some(err.to_string());
            } else {
                app.servers = parse_servers(&v);
            }
        }
        Err(e) => app.error = Some(format!("list: {e}")),
    }
    match admin::client_request_on(&app.instance.socket, "tools", json!({})).await {
        Ok(v) => {
            if let Some(err) = v.get("error").and_then(Value::as_str) {
                app.error = Some(err.to_string());
            } else {
                app.tools = parse_tools(&v);
            }
        }
        Err(e) => app.error = Some(format!("tools: {e}")),
    }

    if app.servers.is_empty() {
        app.server_sel = 0;
    } else if app.server_sel >= app.servers.len() {
        app.server_sel = app.servers.len() - 1;
    }
    let cnt = app.tools_for_selected().len();
    if cnt == 0 || app.tool_sel >= cnt {
        app.tool_sel = 0;
    }
}

#[cfg(feature = "tui")]
fn parse_servers(v: &Value) -> Vec<ServerRow> {
    let Some(arr) = v.get("servers").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .map(|s| ServerRow {
            name: s
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            kind: s
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            default: s
                .get("enabled_in_config")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            connected: s.get("connected").and_then(Value::as_bool).unwrap_or(false),
            tools: s.get("tools").and_then(Value::as_u64).unwrap_or(0),
        })
        .collect()
}

#[cfg(feature = "tui")]
fn parse_tools(v: &Value) -> Vec<ToolRow> {
    let Some(arr) = v.get("tools").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .map(|t| ToolRow {
            server: t
                .get("server")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            tool: t
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            summary: t
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            connected: t.get("connected").and_then(Value::as_bool).unwrap_or(false),
            enabled: t.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            default: t.get("default").and_then(Value::as_bool).unwrap_or(true),
            session: t.get("session").and_then(Value::as_bool),
        })
        .collect()
}
