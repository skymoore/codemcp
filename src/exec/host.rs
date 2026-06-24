//! Host CPython executor: a persistent `python3` worker running `bootstrap.py`.
//!
//! The worker self-provisions `websockets`, connects back to the gateway's
//! WebSocket control channel, authenticates, imports the generated `sdk.py`, and
//! serves `run` requests. The SDK is loaded once; only user code travels per call.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::{Child, Command};

use crate::control::{ControlHandle, ControlServer, RunOutput};
use crate::env::Settings;
use crate::error::Error;
use crate::upstream::SharedUpstreams;

const BOOTSTRAP_PY: &str = include_str!("../../pyworker/bootstrap.py");

pub struct HostExecutor {
    handle: ControlHandle,
    _child: Child,
    _workdir: PathBuf,
    timeout: Duration,
}

impl HostExecutor {
    /// Materialize the worker files, start the control server + python worker,
    /// and wait for the worker to authenticate.
    pub async fn start(
        settings: &Settings,
        sdk_py: &str,
        upstreams: SharedUpstreams,
    ) -> Result<Self, Error> {
        // Working directory holding bootstrap.py + sdk.py.
        let workdir = std::env::temp_dir().join(format!("codemcp-{}", std::process::id()));
        std::fs::create_dir_all(&workdir)?;
        std::fs::write(workdir.join("bootstrap.py"), BOOTSTRAP_PY)?;
        std::fs::write(workdir.join("sdk.py"), sdk_py)?;

        // Bind control server first to learn the actual port.
        let token = settings.control_token().to_string();
        let server = ControlServer::bind(&settings.control_bind, token.clone(), upstreams).await?;
        let addr = server.local_addr()?;
        let host = settings
            .control_host_for_worker
            .clone()
            .unwrap_or_else(|| addr.ip().to_string());
        let control_url = format!("ws://{host}:{}", addr.port());

        // Locate python.
        let python = match &settings.python {
            Some(p) => p.clone(),
            None => which::which("python3")
                .or_else(|_| which::which("python"))
                .map_err(|_| Error::Config("python3 not found on PATH".into()))?,
        };

        tracing::info!(%control_url, python = %python.display(), "starting host python worker");

        let mut cmd = Command::new(&python);
        cmd.arg(workdir.join("bootstrap.py"))
            .current_dir(&workdir)
            .env("CODEMCP_CONTROL_URL", &control_url)
            .env("CODEMCP_CONTROL_TOKEN", &token)
            .env("CODEMCP_SDK_DIR", &workdir)
            .env(
                "CODEMCP_WS_AUTO_INSTALL",
                if settings.ws_auto_install {
                    "true"
                } else {
                    "false"
                },
            )
            .kill_on_drop(true);
        if let Some(v) = &settings.ws_version {
            cmd.env("CODEMCP_WS_VERSION", v);
        }
        if !settings.ws_pip_args.is_empty() {
            cmd.env("CODEMCP_WS_PIP_ARGS", settings.ws_pip_args.join(" "));
        }

        let child = cmd
            .spawn()
            .map_err(|e| Error::Worker(format!("failed to spawn python worker: {e}")))?;

        // Wait for the worker to connect + authenticate (it may install websockets
        // first, so allow generous time).
        let handle = tokio::time::timeout(Duration::from_secs(120), server.accept_worker())
            .await
            .map_err(|_| Error::Worker("worker did not connect within 120s".into()))??;

        Ok(Self {
            handle,
            _child: child,
            _workdir: workdir,
            timeout: settings.exec_timeout(),
        })
    }
}

#[async_trait]
impl super::Executor for HostExecutor {
    async fn run(&self, code: String) -> Result<RunOutput, Error> {
        match tokio::time::timeout(self.timeout, self.handle.run(&code)).await {
            Ok(res) => res,
            Err(_) => Err(Error::Timeout(self.timeout)),
        }
    }

    async fn reload_sdk(&self, sdk_py: &str) -> Result<(), Error> {
        self.handle.reload(sdk_py).await
    }

    async fn shutdown(&self) {
        // Dropping the child (kill_on_drop) handles teardown.
    }
}
