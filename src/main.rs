//! codemcp — meta-MCP code-mode gateway.
//!
//! Connects to many upstream MCP servers and exposes a single `execute_python`
//! tool. Agents write Python that calls all upstream tools as typed functions.
//!
//! With no subcommand, runs the gateway. The `list`/`enable`/`disable`
//! subcommands are an admin client that talks to a running gateway.

mod admin;
mod auth;
mod cli;
mod config;
mod control;
mod env;
mod error;
mod exec;
mod launcher;
mod prompt;
mod runtime;
mod sdk;
mod server;
mod setup;
mod tui;
mod upstream;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use crate::cli::Cli;
use crate::env::{Isolation, ServerTransport, Settings};
use crate::exec::host::HostExecutor;
use crate::exec::Executor;
use crate::runtime::{Runtime, SdkState};
use crate::sdk::SdkRegistry;
use crate::server::CodeServer;
use crate::upstream::UpstreamManager;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(command) = cli.command {
        // `setup` runs locally without touching the gateway/admin socket.
        if command.is_local() {
            return cli::run_local(command).map_err(Into::into);
        }
        // `start` runs the gateway itself (shared HTTP instance).
        if command.is_gateway() {
            let override_http = match command {
                cli::Command::Start { port, host } => Some((host, port)),
                _ => unreachable!(),
            };
            return run_gateway(override_http).await;
        }
        // Admin subcommands are a thin client to a running gateway.
        return cli::run_admin(command).await.map_err(Into::into);
    }

    run_gateway(None).await
}

/// Construct the Python execution backend selected by `CODEMCP_ISOLATION`.
async fn build_executor(
    settings: &Settings,
    sdk_py: &str,
    upstreams: crate::upstream::SharedUpstreams,
) -> Result<Arc<dyn Executor>> {
    match settings.isolation {
        Isolation::HostSystem => Ok(Arc::new(
            HostExecutor::start(settings, sdk_py, upstreams).await?,
        )),
        Isolation::Docker => {
            #[cfg(feature = "docker")]
            {
                Ok(Arc::new(
                    crate::exec::docker::DockerExecutor::start(settings, sdk_py, upstreams).await?,
                ))
            }
            #[cfg(not(feature = "docker"))]
            {
                let _ = (sdk_py, upstreams);
                anyhow::bail!(
                    "CODEMCP_ISOLATION=DOCKER but this binary was built without the `docker` feature; rebuild with --features docker"
                )
            }
        }
        Isolation::Monty => {
            anyhow::bail!(
                "CODEMCP_ISOLATION=MONTY is not implemented yet; use HOST_SYSTEM or DOCKER"
            )
        }
    }
}

/// Run the gateway. When `http_override` is `Some((host, port))` (from
/// `codemcp start`), force the Streamable HTTP transport on that address;
/// otherwise honor the `CODEMCP_*` settings.
async fn run_gateway(http_override: Option<(String, u16)>) -> Result<()> {
    let mut settings = Settings::from_env()?;

    if let Some((host, port)) = http_override {
        settings.transport = ServerTransport::Http;
        settings.http_bind = format!("{host}:{port}")
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid --host/--port {host}:{port}: {e}"))?;
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&settings.log))
        .with_writer(std::io::stderr)
        .init();

    tracing::info!(
        isolation = ?settings.isolation,
        transport = ?settings.transport,
        config = %settings.config.display(),
        "codemcp starting"
    );

    let configs = config::load(&settings.config)?;
    tracing::info!(count = configs.len(), "loaded upstream server configs");

    let manager = UpstreamManager::connect_all(&configs).await;
    let tools = manager.all_tools().await;
    tracing::info!(total_tools = tools.len(), "discovered upstream tools");

    let registry = SdkRegistry::build(&tools);
    tracing::info!(bindings = registry.bindings.len(), "generated SDK bindings");

    // Debug dump when requested.
    if std::env::var("CODEMCP_DUMP").is_ok() {
        eprintln!("===== sdk.py =====\n{}", registry.generate_sdk_py());
        eprintln!(
            "===== execute_python description =====\n{}",
            prompt::build_description(&registry, settings.isolation)
        );
    }

    let sdk_py = registry.generate_sdk_py();
    let upstreams = Arc::new(manager);

    // Smoke-test path: start the worker, run a snippet, print, exit.
    if let Ok(code) = std::env::var("CODEMCP_SMOKE") {
        let executor = build_executor(&settings, &sdk_py, upstreams.clone()).await?;
        let out = executor.run(code).await?;
        eprintln!(
            "=== result ===\n{}",
            serde_json::to_string_pretty(&out.result)?
        );
        eprintln!("=== stdout ===\n{}", out.stdout);
        eprintln!("=== stderr ===\n{}", out.stderr);
        if let Some(err) = &out.error {
            eprintln!("=== error ===\n{err}");
        }
        executor.shutdown().await;
        upstreams.shutdown().await;
        return Ok(());
    }

    // Start the Python worker and assemble the shared runtime.
    let executor = build_executor(&settings, &sdk_py, upstreams.clone()).await?;
    let description = prompt::build_description(&registry, settings.isolation);
    let launcher = launcher::Launcher::detect();
    tracing::info!(
        launcher = %launcher.name,
        source = ?launcher.source,
        parent_pid = ?launcher.parent_pid,
        "detected launching application"
    );
    let runtime = Runtime::new(
        upstreams.clone(),
        executor,
        settings.isolation,
        settings.config.clone(),
        launcher,
        SdkState {
            registry,
            description,
        },
    )
    .await?;

    // For HTTP, bind the TCP listener *before* starting the admin server, so a
    // port conflict fails fast without leaving a stale admin socket behind.
    let http_listener = match settings.transport {
        ServerTransport::Http => Some(
            tokio::net::TcpListener::bind(settings.http_bind)
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "failed to bind HTTP endpoint on {} ({e}). \
                         Is another process (or codemcp instance) already using that port?",
                        settings.http_bind
                    )
                })?,
        ),
        ServerTransport::Stdio => None,
    };

    // Admin socket: enable/disable upstreams at runtime.
    {
        let admin_rt = runtime.clone();
        tokio::spawn(async move {
            if let Err(e) = admin::serve(admin_rt).await {
                tracing::error!(error = %e, "admin interface failed");
            }
        });
    }

    let code_server = CodeServer::new(runtime.clone());

    match settings.transport {
        ServerTransport::Stdio => {
            tracing::info!("serving execute_python over stdio");
            let running = code_server
                .serve(stdio())
                .await
                .map_err(|e| anyhow::anyhow!("failed to start stdio server: {e}"))?;
            running
                .waiting()
                .await
                .map_err(|e| anyhow::anyhow!("server task error: {e}"))?;
        }
        ServerTransport::Http => {
            let config = StreamableHttpServerConfig::default()
                .with_stateful_mode(!settings.http_json_response)
                .with_json_response(settings.http_json_response);
            // Each session shares the single Python worker via the cloned server.
            let factory_server = code_server.clone();
            let service = StreamableHttpService::new(
                move || Ok(factory_server.clone()),
                Arc::new(LocalSessionManager::default()),
                config,
            );

            let app = axum::Router::new().nest_service(&settings.http_path, service);
            let listener = http_listener.expect("http listener bound above");
            tracing::info!(
                bind = %settings.http_bind,
                path = %settings.http_path,
                "serving execute_python over Streamable HTTP"
            );
            axum::serve(listener, app)
                .await
                .map_err(|e| anyhow::anyhow!("http server error: {e}"))?;
        }
    }

    runtime.shutdown().await;
    Ok(())
}
