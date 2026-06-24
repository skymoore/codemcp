//! Command-line interface (clap). With no subcommand, codemcp runs as the MCP
//! gateway. The `list`/`enable`/`disable` subcommands are a thin admin client
//! that talks to a running gateway over its Unix admin socket.

use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use crate::admin;
use crate::error::Error;
use crate::setup::{self, Harness};

#[derive(Parser)]
#[command(
    name = "codemcp",
    about = "Meta-MCP code-mode gateway. Run with no subcommand to start the server.",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Selects which running gateway an admin command targets when more than one is
/// live. Matches a substring of the config path, or an exact PID.
#[derive(clap::Args, Clone, Default)]
pub struct InstanceSel {
    /// Target a specific gateway by launcher name, config-path substring, or
    /// PID (only needed when multiple gateways are running).
    #[arg(short = 'i', long, global = true)]
    pub instance: Option<String>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run a long-lived Streamable HTTP gateway on a fixed port, suitable for
    /// sharing one codemcp instance between multiple harnesses.
    Start {
        /// TCP port to bind the HTTP MCP endpoint on. Fails if already in use.
        #[arg(short = 'p', long, default_value_t = crate::env::DEFAULT_HTTP_PORT)]
        port: u16,
        /// Address to bind (default 127.0.0.1).
        #[arg(short = 'H', long, default_value = "127.0.0.1")]
        host: String,
    },
    /// List all running codemcp gateways (one per harness, plus any `start`ed).
    Instances,
    /// List configured upstream servers and their live connection status.
    List {
        #[command(flatten)]
        instance: InstanceSel,
    },
    /// Print the `execute_python` tool definition (name + description +
    /// inputSchema) as the model sees it in `tools/list`.
    Tool {
        #[command(flatten)]
        instance: InstanceSel,
    },
    /// Print the generated `sdk.py` (the typed Python SDK preloaded into the
    /// worker).
    Sdk {
        #[command(flatten)]
        instance: InstanceSel,
    },
    /// Connect an upstream in the running gateway (no restart).
    Enable {
        /// Server name as it appears in mcp.json.
        name: String,
        /// Also persist `enabled: true` to mcp.json (changes boot default).
        #[arg(short = 'd', long)]
        make_default: bool,
        #[command(flatten)]
        instance: InstanceSel,
    },
    /// Disconnect an upstream in the running gateway (no restart).
    Disable {
        /// Server name as it appears in mcp.json.
        name: String,
        /// Also persist `enabled: false` to mcp.json (changes boot default).
        #[arg(short = 'd', long)]
        make_default: bool,
        #[command(flatten)]
        instance: InstanceSel,
    },
    /// List every tool across all configured servers with its effective enabled
    /// state, configured default, and any session override.
    Tools {
        #[command(flatten)]
        instance: InstanceSel,
    },
    /// Enable a single tool in the running gateway (no restart). Auto-connects
    /// the server if it is not already connected.
    EnableTool {
        /// Server name as it appears in mcp.json.
        server: String,
        /// Tool name as exposed by the upstream.
        tool: String,
        /// Also persist `tools.<tool>.enabled: true` to mcp.json (default on).
        #[arg(short = 'd', long)]
        make_default: bool,
        #[command(flatten)]
        instance: InstanceSel,
    },
    /// Disable a single tool in the running gateway (no restart). The server
    /// stays connected so its other tools keep working.
    DisableTool {
        /// Server name as it appears in mcp.json.
        server: String,
        /// Tool name as exposed by the upstream.
        tool: String,
        /// Also persist `tools.<tool>.enabled: false` to mcp.json (default off).
        #[arg(short = 'd', long)]
        make_default: bool,
        #[command(flatten)]
        instance: InstanceSel,
    },
    /// Authenticate with an OAuth-enabled remote MCP server. Opens a browser
    /// to complete the OAuth authorization flow.
    Auth {
        /// Server name as it appears in mcp.json. If omitted, lists OAuth-capable
        /// servers and their auth status.
        name: Option<String>,
        /// List OAuth-capable servers and their auth status (alias: `ls`).
        #[arg(long)]
        list: bool,
        #[command(flatten)]
        instance: InstanceSel,
    },
    /// Remove stored OAuth credentials for an MCP server.
    Logout {
        /// Server name as it appears in mcp.json. If omitted, lists servers with
        /// stored credentials.
        name: Option<String>,
        #[command(flatten)]
        instance: InstanceSel,
    },
    /// Wire codemcp into an agent harness: back up its config, move its MCP
    /// servers into codemcp's mcp.json, and point the harness at codemcp.
    Setup {
        /// Harness to set up. Supported: opencode.
        harness: Harness,
    },
    /// Interactive terminal UI for enabling/disabling servers and individual
    /// tools against a running gateway, with session-only or persisted
    /// (default) semantics. (Requires the `tui` feature.)
    Tui {
        #[command(flatten)]
        instance: InstanceSel,
    },
}

impl Command {
    /// Whether this command is handled synchronously without the gateway/admin
    /// socket (i.e. `setup`).
    pub fn is_local(&self) -> bool {
        matches!(self, Command::Setup { .. })
    }

    /// Whether this command runs the gateway itself (i.e. `start`).
    pub fn is_gateway(&self) -> bool {
        matches!(self, Command::Start { .. })
    }
}

/// Run a local (non-admin) subcommand. Currently just `setup`.
pub fn run_local(cmd: Command) -> Result<(), Error> {
    match cmd {
        Command::Setup { harness } => setup::run(harness),
        _ => unreachable!("run_local called with a non-local command"),
    }
}

/// Run an admin subcommand against a live gateway. Prints human-readable output.
pub async fn run_admin(cmd: Command) -> Result<(), Error> {
    match cmd {
        Command::Instances => {
            let instances = admin::live_instances().await;
            if instances.is_empty() {
                println!("no running codemcp gateways found");
            } else {
                println!("{:<14} {:<8} CONFIG", "LAUNCHER", "PID");
                for i in &instances {
                    println!("{:<14} {:<8} {}", i.launcher, i.pid, i.config);
                }
            }
        }
        Command::List { instance } => {
            let target = admin::select_instance(instance.instance.as_deref()).await?;
            println!("# gateway [{}] pid {}", target.launcher, target.pid);
            let resp =
                admin::client_request(instance.instance.as_deref(), "list", json!({})).await?;
            print_list(&resp);
        }
        Command::Tool { instance } => {
            let target = admin::select_instance(instance.instance.as_deref()).await?;
            let _ = target;
            let resp =
                admin::client_request(instance.instance.as_deref(), "tool", json!({})).await?;
            if let Some(err) = resp.get("error").and_then(Value::as_str) {
                eprintln!("error: {err}");
            } else if let Some(tool) = resp.get("tool") {
                println!("{}", serde_json::to_string_pretty(tool).unwrap_or_default());
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&resp).unwrap_or_default()
                );
            }
        }
        Command::Sdk { instance } => {
            admin::select_instance(instance.instance.as_deref()).await?;
            let resp =
                admin::client_request(instance.instance.as_deref(), "sdk", json!({})).await?;
            if let Some(err) = resp.get("error").and_then(Value::as_str) {
                eprintln!("error: {err}");
            } else if let Some(sdk) = resp.get("sdk").and_then(Value::as_str) {
                print!("{sdk}");
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&resp).unwrap_or_default()
                );
            }
        }
        Command::Enable {
            name,
            make_default,
            instance,
        } => {
            let resp = admin::client_request(
                instance.instance.as_deref(),
                "enable",
                json!({ "name": name, "make_default": make_default }),
            )
            .await?;
            print_action("enabled", &resp);
        }
        Command::Disable {
            name,
            make_default,
            instance,
        } => {
            let resp = admin::client_request(
                instance.instance.as_deref(),
                "disable",
                json!({ "name": name, "make_default": make_default }),
            )
            .await?;
            print_action("disabled", &resp);
        }
        Command::Tools { instance } => {
            admin::select_instance(instance.instance.as_deref()).await?;
            let resp =
                admin::client_request(instance.instance.as_deref(), "tools", json!({})).await?;
            print_tools(&resp);
        }
        Command::EnableTool {
            server,
            tool,
            make_default,
            instance,
        } => {
            let resp = admin::client_request(
                instance.instance.as_deref(),
                "enable_tool",
                json!({ "server": server, "tool": tool, "make_default": make_default }),
            )
            .await?;
            print_tool_action("enabled", &resp);
        }
        Command::DisableTool {
            server,
            tool,
            make_default,
            instance,
        } => {
            let resp = admin::client_request(
                instance.instance.as_deref(),
                "disable_tool",
                json!({ "server": server, "tool": tool, "make_default": make_default }),
            )
            .await?;
            print_tool_action("disabled", &resp);
        }
        Command::Auth {
            name,
            list,
            instance,
        } => {
            if list || name.is_none() {
                // List OAuth-capable servers and their auth status.
                let resp =
                    admin::client_request(instance.instance.as_deref(), "auth_status", json!({}))
                        .await?;
                print_auth_list(&resp);
            } else {
                let server_name = name.as_deref().unwrap();
                // Start the OAuth flow.
                let resp = admin::client_request(
                    instance.instance.as_deref(),
                    "auth_start",
                    json!({ "name": server_name }),
                )
                .await?;
                if let Some(err) = resp.get("error").and_then(Value::as_str) {
                    eprintln!("error: {err}");
                    return Ok(());
                }
                let auth_url = resp
                    .get("authorization_url")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if auth_url.is_empty() {
                    eprintln!("error: no authorization URL returned");
                    return Ok(());
                }
                println!("Open this URL to authenticate:\n{auth_url}");
                // Try to open the browser (non-fatal if it fails).
                let _ = webbrowser::open(auth_url);
                println!("\nWaiting for authorization (timeout: 5 minutes)...");
                // Finish the OAuth flow (blocks until callback or timeout).
                let resp = admin::client_request(
                    instance.instance.as_deref(),
                    "auth_finish",
                    json!({ "name": server_name }),
                )
                .await?;
                if let Some(err) = resp.get("error").and_then(Value::as_str) {
                    eprintln!("error: {err}");
                } else {
                    let tools = resp.get("tools").and_then(Value::as_u64).unwrap_or(0);
                    println!("{server_name} authenticated ({tools} tools)");
                }
            }
        }
        Command::Logout { name, instance } => {
            if let Some(name) = name {
                let resp = admin::client_request(
                    instance.instance.as_deref(),
                    "auth_remove",
                    json!({ "name": name }),
                )
                .await?;
                if let Some(err) = resp.get("error").and_then(Value::as_str) {
                    eprintln!("error: {err}");
                } else {
                    let removed = resp
                        .get("removed")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if removed {
                        println!("{name} credentials removed");
                    } else {
                        println!("{name} had no stored credentials");
                    }
                }
            } else {
                let resp =
                    admin::client_request(instance.instance.as_deref(), "auth_status", json!({}))
                        .await?;
                print_auth_list(&resp);
            }
        }
        Command::Tui { instance } => {
            crate::tui::run(instance.instance.as_deref()).await?;
        }
        Command::Start { .. } => unreachable!("start is handled by run_gateway"),
        Command::Setup { .. } => unreachable!("setup is handled by run_local"),
    }
    Ok(())
}

fn print_list(resp: &Value) {
    if let Some(err) = resp.get("error").and_then(Value::as_str) {
        eprintln!("error: {err}");
        return;
    }
    let servers = match resp.get("servers").and_then(Value::as_array) {
        Some(s) => s,
        None => {
            println!("{}", serde_json::to_string_pretty(resp).unwrap_or_default());
            return;
        }
    };
    println!(
        "{:<22} {:<7} {:<9} {:<10} {:<16} TOOLS",
        "NAME", "TYPE", "DEFAULT", "CONNECTED", "AUTH"
    );
    for s in servers {
        let name = s.get("name").and_then(Value::as_str).unwrap_or("?");
        let kind = s.get("kind").and_then(Value::as_str).unwrap_or("?");
        let enabled = s
            .get("enabled_in_config")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let connected = s.get("connected").and_then(Value::as_bool).unwrap_or(false);
        let tools = s.get("tools").and_then(Value::as_u64).unwrap_or(0);
        let auth_status = s
            .get("auth_status")
            .and_then(Value::as_str)
            .unwrap_or("n/a");
        let auth_hint = s.get("auth_hint").and_then(Value::as_str);
        let auth_display = match auth_hint {
            Some(hint) => format!("{auth_status} ({hint})"),
            None => auth_status.to_string(),
        };
        println!(
            "{:<22} {:<7} {:<9} {:<10} {:<16} {}",
            name,
            kind,
            if enabled { "yes" } else { "no" },
            if connected { "yes" } else { "no" },
            auth_display,
            tools
        );
    }
}

/// Print OAuth auth status for all servers (used by `codemcp auth` with no name
/// or `codemcp auth --list`).
fn print_auth_list(resp: &Value) {
    if let Some(err) = resp.get("error").and_then(Value::as_str) {
        eprintln!("error: {err}");
        return;
    }
    let servers = match resp.get("servers").and_then(Value::as_array) {
        Some(s) => s,
        None => {
            println!("{}", serde_json::to_string_pretty(resp).unwrap_or_default());
            return;
        }
    };
    let oauth_servers: Vec<_> = servers
        .iter()
        .filter(|s| {
            let kind = s.get("kind").and_then(Value::as_str).unwrap_or("");
            kind == "remote"
        })
        .collect();
    if oauth_servers.is_empty() {
        println!("no remote MCP servers configured");
        return;
    }
    println!("{:<22} {:<16} HINT", "NAME", "AUTH STATUS");
    for s in oauth_servers {
        let name = s.get("name").and_then(Value::as_str).unwrap_or("?");
        let auth_status = s
            .get("auth_status")
            .and_then(Value::as_str)
            .unwrap_or("n/a");
        let hint = s.get("auth_hint").and_then(Value::as_str).unwrap_or("");
        println!("{:<22} {:<16} {}", name, auth_status, hint);
    }
}

fn print_action(verb: &str, resp: &Value) {
    if let Some(err) = resp.get("error").and_then(Value::as_str) {
        eprintln!("error: {err}");
        return;
    }
    let name = resp.get("name").and_then(Value::as_str).unwrap_or("?");
    let made_default = resp
        .get("made_default")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut msg = format!("{name} {verb}");
    if let Some(tools) = resp.get("tools").and_then(Value::as_u64) {
        msg.push_str(&format!(" ({tools} tools)"));
    }
    if made_default {
        msg.push_str(" [persisted to mcp.json]");
    }
    println!("{msg}");
}

fn print_tools(resp: &Value) {
    if let Some(err) = resp.get("error").and_then(Value::as_str) {
        eprintln!("error: {err}");
        return;
    }
    let tools = match resp.get("tools").and_then(Value::as_array) {
        Some(t) => t,
        None => {
            println!("{}", serde_json::to_string_pretty(resp).unwrap_or_default());
            return;
        }
    };
    println!(
        "{:<18} {:<26} {:<7} {:<8} {:<8} SUMMARY",
        "SERVER", "TOOL", "ON", "DEFAULT", "SESSION"
    );
    for t in tools {
        let server = t.get("server").and_then(Value::as_str).unwrap_or("?");
        let tool = t.get("tool").and_then(Value::as_str).unwrap_or("?");
        let enabled = t.get("enabled").and_then(Value::as_bool).unwrap_or(false);
        let default = t.get("default").and_then(Value::as_bool).unwrap_or(true);
        let session = t.get("session").and_then(Value::as_bool);
        let summary = t.get("summary").and_then(Value::as_str).unwrap_or("");
        println!(
            "{:<18} {:<26} {:<7} {:<8} {:<8} {}",
            server,
            tool,
            if enabled { "yes" } else { "no" },
            if default { "on" } else { "off" },
            match session {
                Some(true) => "on",
                Some(false) => "off",
                None => "-",
            },
            summary,
        );
    }
}

fn print_tool_action(verb: &str, resp: &Value) {
    if let Some(err) = resp.get("error").and_then(Value::as_str) {
        eprintln!("error: {err}");
        return;
    }
    let server = resp.get("server").and_then(Value::as_str).unwrap_or("?");
    let tool = resp.get("tool").and_then(Value::as_str).unwrap_or("?");
    let enabled = resp
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let made_default = resp
        .get("made_default")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut msg = format!(
        "{server}/{tool} {verb} (now {})",
        if enabled { "on" } else { "off" }
    );
    if made_default {
        msg.push_str(" [persisted to mcp.json]");
    }
    println!("{msg}");
}
