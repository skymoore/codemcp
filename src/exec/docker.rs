//! Docker executor: runs the Python worker inside an isolated container.
//!
//! The worker is the *same* `bootstrap.py` used by the host executor and speaks
//! the *same* WebSocket control protocol — only the spawn mechanism and the
//! control-channel networking differ.
//!
//! ## Security model
//!
//! The control channel is effectively god-mode over every upstream MCP, so it
//! must never be reachable from the LAN/public wifi. Instead of binding to
//! `0.0.0.0` (LAN-exposed) or host loopback (unreachable from a container), we:
//!
//! 1. ensure a dedicated user-defined **bridge** network (`docker_network`),
//! 2. inspect it to learn the host-side **gateway IP** (e.g. `172.18.0.1`),
//! 3. bind the control server to **that gateway IP only**.
//!
//! The bridge gateway is a host-internal interface; it is not routed to the
//! physical network, so only containers attached to that one bridge can reach
//! the control port. The auth token remains the second line of defense.
//!
//! Files (`bootstrap.py` + generated `sdk.py`) are delivered via a **read-only
//! bind-mount** of a host workdir, which also gives SDK hot-reload for free: a
//! reload just rewrites the mounted `sdk.py` on the host.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use bollard::models::{ContainerCreateBody, HostConfig, Ipam, IpamConfig, NetworkCreateRequest};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, RemoveContainerOptionsBuilder,
};
use bollard::Docker;
use futures::StreamExt;

use crate::control::{ControlHandle, RunOutput, ShapeSink};
use crate::env::{self, Settings};
use crate::error::Error;
use crate::upstream::SharedUpstreams;

const BOOTSTRAP_PY: &str = include_str!("../../pyworker/bootstrap.py");
/// Mount point of the workdir inside the container.
const CONTAINER_WORKDIR: &str = "/codemcp";

pub struct DockerExecutor {
    handle: ControlHandle,
    docker: Docker,
    container_id: String,
    workdir: PathBuf,
    timeout: Duration,
}

impl DockerExecutor {
    pub async fn start(
        settings: &Settings,
        sdk_py: &str,
        upstreams: SharedUpstreams,
        shapes: ShapeSink,
    ) -> Result<Self, Error> {
        // 1. Connect to the daemon (auto-detects Docker/Podman socket).
        let docker = Docker::connect_with_local_defaults().map_err(|e| {
            Error::Docker(format!(
                "cannot reach the Docker daemon ({e}). Is Docker (or Podman) running?"
            ))
        })?;
        // Fail fast with a clear message if the daemon is unresponsive.
        docker
            .ping()
            .await
            .map_err(|e| Error::Docker(format!("Docker daemon did not respond to ping: {e}")))?;

        // 2. Ensure the dedicated bridge network + learn its gateway IP.
        let gateway_ip = ensure_network(&docker, &settings.docker_network).await?;

        // 3. Bind the control channel securely (platform-aware; see ChannelPlan).
        let token = settings.control_token().to_string();
        let plan =
            bind_control_channel(settings, &gateway_ip, upstreams, token.clone(), shapes).await?;
        let ControlChannel {
            server,
            control_url,
            add_host_gateway,
        } = plan;
        let addr = server.local_addr()?;
        tracing::info!(
            network = %settings.docker_network,
            %control_url,
            "control channel ready (not exposed to the LAN)"
        );

        // 4. Materialize the workdir under a Docker-shareable path ($HOME/.cache).
        let workdir = env::docker_workdir();
        std::fs::create_dir_all(&workdir)?;
        std::fs::write(workdir.join("bootstrap.py"), BOOTSTRAP_PY)?;
        std::fs::write(workdir.join("sdk.py"), sdk_py)?;

        // 5. Pull the image if it isn't present locally.
        ensure_image(&docker, &settings.docker_image).await?;

        // 6. Create + start a hardened container.
        let container_id = create_and_start(
            &docker,
            settings,
            &workdir,
            &control_url,
            &token,
            add_host_gateway,
        )
        .await
        .inspect_err(|_| {
            // Best-effort cleanup of the workdir on a failed start.
            let _ = std::fs::remove_dir_all(&workdir);
        })?;
        let _ = addr;

        tracing::info!(%control_url, image = %settings.docker_image, container = %container_id, "started docker python worker");

        // 7. Wait for the worker to connect + authenticate (cold start includes
        // image pull, container boot, and a fresh `pip install websockets`).
        let handle = match tokio::time::timeout(Duration::from_secs(180), server.accept_worker())
            .await
        {
            Ok(res) => res?,
            Err(_) => {
                let _ = remove_container(&docker, &container_id).await;
                let _ = std::fs::remove_dir_all(&workdir);
                return Err(Error::Worker(
                    "docker worker did not connect within 180s (image pull or pip install may have failed; check container logs)".into(),
                ));
            }
        };

        Ok(Self {
            handle,
            docker,
            container_id,
            workdir,
            timeout: settings.exec_timeout(),
        })
    }
}

/// A bound control channel plus how the container should reach it.
struct ControlChannel {
    server: crate::control::ControlServer,
    /// `ws://host:port` the worker dials.
    control_url: String,
    /// Whether the container needs `host.docker.internal:host-gateway` added.
    add_host_gateway: bool,
}

/// Bind the control channel so that **only the worker container** can reach it,
/// never the LAN. Two host topologies exist:
///
/// * **Native Linux Docker:** the bridge gateway IP (e.g. `172.18.0.1`) is a
///   real host-internal interface. We bind there directly and the worker dials
///   that IP. Not routed to the physical network.
/// * **Docker Desktop (macOS/Windows, or some Linux):** there is no host-side
///   bridge interface — the gateway IP lives inside the VM and the host cannot
///   bind it. Instead the container reaches the host via `host.docker.internal`,
///   which Docker Desktop forwards to the host's **loopback**. So we bind to
///   `127.0.0.1` (loopback-only, not LAN-exposed) and add a
///   `host.docker.internal:host-gateway` mapping so the worker can find us.
///
/// We try the gateway IP first and fall back to loopback if the OS refuses to
/// assign it (`EADDRNOTAVAIL`), which is exactly the Docker Desktop signal.
async fn bind_control_channel(
    settings: &Settings,
    gateway_ip: &str,
    upstreams: SharedUpstreams,
    token: String,
    shapes: ShapeSink,
) -> Result<ControlChannel, Error> {
    // Honor an explicit override unconditionally.
    if let Some(host) = settings.control_host_for_worker.clone() {
        let server =
            crate::control::ControlServer::bind(&settings.control_bind, token, upstreams, shapes)
                .await?;
        let port = server.local_addr()?.port();
        return Ok(ControlChannel {
            server,
            control_url: format!("ws://{host}:{port}"),
            // If the user points us at host.docker.internal, ensure it resolves.
            add_host_gateway: host == "host.docker.internal",
        });
    }

    // Linux-native path: bind the bridge gateway IP directly.
    match crate::control::ControlServer::bind(
        &format!("{gateway_ip}:0"),
        token.clone(),
        upstreams.clone(),
        shapes.clone(),
    )
    .await
    {
        Ok(server) => {
            let port = server.local_addr()?.port();
            tracing::debug!(%gateway_ip, "bound control channel to bridge gateway (linux topology)");
            Ok(ControlChannel {
                server,
                control_url: format!("ws://{gateway_ip}:{port}"),
                add_host_gateway: false,
            })
        }
        Err(e) => {
            // Docker Desktop topology: host can't bind the bridge IP. Fall back
            // to loopback + host.docker.internal.
            tracing::debug!(error = %e, "bridge gateway unbindable; using loopback + host.docker.internal (docker desktop topology)");
            let server =
                crate::control::ControlServer::bind("127.0.0.1:0", token, upstreams, shapes)
                    .await?;
            let port = server.local_addr()?.port();
            Ok(ControlChannel {
                server,
                control_url: format!("ws://host.docker.internal:{port}"),
                add_host_gateway: true,
            })
        }
    }
}

/// Ensure the user-defined bridge network exists and return its gateway IP.
async fn ensure_network(docker: &Docker, name: &str) -> Result<String, Error> {
    // Try to inspect first; create only if missing.
    if let Ok(net) = docker.inspect_network(name, None).await {
        if let Some(ip) = gateway_of(&net.ipam) {
            return Ok(ip);
        }
    }

    // Create a plain bridge network (Docker assigns a subnet + gateway).
    let req = NetworkCreateRequest {
        name: name.to_string(),
        driver: Some("bridge".to_string()),
        ..Default::default()
    };
    match docker.create_network(req).await {
        Ok(_) => {}
        Err(e) => {
            // A concurrent codemcp instance may have created it first; tolerate.
            tracing::debug!(error = %e, "create_network failed (may already exist), re-inspecting");
        }
    }

    let net = docker
        .inspect_network(name, None)
        .await
        .map_err(|e| Error::Docker(format!("failed to inspect network {name}: {e}")))?;
    gateway_of(&net.ipam).ok_or_else(|| {
        Error::Docker(format!(
            "network {name} has no IPAM gateway; cannot bind the control channel safely"
        ))
    })
}

/// Extract the first non-empty IPAM gateway address from a network's IPAM config.
fn gateway_of(ipam: &Option<Ipam>) -> Option<String> {
    ipam.as_ref()?
        .config
        .as_ref()?
        .iter()
        .filter_map(|c: &IpamConfig| c.gateway.clone())
        .find(|g| !g.is_empty())
}

/// Pull the image if it is not already present locally, logging progress.
async fn ensure_image(docker: &Docker, image: &str) -> Result<(), Error> {
    if docker.inspect_image(image).await.is_ok() {
        return Ok(());
    }
    tracing::info!(%image, "image not found locally; pulling");

    let options = CreateImageOptionsBuilder::default()
        .from_image(image)
        .build();
    let mut stream = docker.create_image(Some(options), None, None);
    while let Some(item) = stream.next().await {
        match item {
            Ok(info) => {
                if let Some(status) = info.status {
                    tracing::debug!(%image, "{status}");
                }
            }
            Err(e) => {
                return Err(Error::Docker(format!(
                    "failed to pull image {image} ({e}). Pre-pull it with `docker pull {image}` if you are offline."
                )));
            }
        }
    }
    tracing::info!(%image, "image pull complete");
    Ok(())
}

/// Build the hardened container spec, create it, and start it.
async fn create_and_start(
    docker: &Docker,
    settings: &Settings,
    workdir: &std::path::Path,
    control_url: &str,
    token: &str,
    add_host_gateway: bool,
) -> Result<String, Error> {
    let workdir_str = workdir
        .to_str()
        .ok_or_else(|| Error::Docker("worker workdir path is not valid UTF-8".into()))?;

    // Read-only bind-mount of the workdir.
    let bind = format!("{workdir_str}:{CONTAINER_WORKDIR}:ro");

    let mut env_vars = vec![
        format!("CODEMCP_CONTROL_URL={control_url}"),
        format!("CODEMCP_CONTROL_TOKEN={token}"),
        format!("CODEMCP_SDK_DIR={CONTAINER_WORKDIR}"),
        "CODEMCP_WS_AUTO_INSTALL=true".to_string(),
        // websockets is installed into a writable dir even with a read-only mount.
        "CODEMCP_WS_CACHE_DIR=/tmp/codemcp-pylib".to_string(),
    ];
    if let Some(v) = &settings.ws_version {
        env_vars.push(format!("CODEMCP_WS_VERSION={v}"));
    }
    if !settings.ws_pip_args.is_empty() {
        env_vars.push(format!(
            "CODEMCP_WS_PIP_ARGS={}",
            settings.ws_pip_args.join(" ")
        ));
    }

    // On Docker Desktop topology the worker reaches us via host.docker.internal,
    // which must be mapped to the host gateway inside the container.
    let extra_hosts =
        add_host_gateway.then(|| vec!["host.docker.internal:host-gateway".to_string()]);

    let host_config = HostConfig {
        binds: Some(vec![bind]),
        network_mode: Some(settings.docker_network.clone()),
        extra_hosts,
        auto_remove: Some(true),
        // Hardening defaults: drop all caps + no privilege escalation.
        cap_drop: Some(vec!["ALL".to_string()]),
        security_opt: Some(vec!["no-new-privileges".to_string()]),
        readonly_rootfs: Some(settings.docker_readonly),
        memory: (settings.docker_memory > 0).then_some(settings.docker_memory),
        nano_cpus: (settings.docker_cpus > 0.0).then_some((settings.docker_cpus * 1e9) as i64),
        pids_limit: (settings.docker_pids_limit > 0).then_some(settings.docker_pids_limit),
        ..Default::default()
    };

    let config = ContainerCreateBody {
        image: Some(settings.docker_image.clone()),
        cmd: Some(vec![
            "python".to_string(),
            format!("{CONTAINER_WORKDIR}/bootstrap.py"),
        ]),
        env: Some(env_vars),
        working_dir: Some(CONTAINER_WORKDIR.to_string()),
        host_config: Some(host_config),
        labels: Some(std::collections::HashMap::from([(
            "com.skymoore.codemcp".to_string(),
            std::process::id().to_string(),
        )])),
        ..Default::default()
    };

    let name = format!("codemcp-worker-{}", std::process::id());
    let options = CreateContainerOptionsBuilder::default().name(&name).build();

    let created = docker
        .create_container(Some(options), config)
        .await
        .map_err(|e| Error::Docker(format!("failed to create container: {e}")))?;

    docker
        .start_container(
            &created.id,
            None::<bollard::query_parameters::StartContainerOptions>,
        )
        .await
        .map_err(|e| Error::Docker(format!("failed to start container {}: {e}", created.id)))?;

    Ok(created.id)
}

async fn remove_container(docker: &Docker, id: &str) -> Result<(), Error> {
    let options = RemoveContainerOptionsBuilder::default().force(true).build();
    docker
        .remove_container(id, Some(options))
        .await
        .map_err(|e| Error::Docker(format!("failed to remove container {id}: {e}")))
}

#[async_trait]
impl super::Executor for DockerExecutor {
    async fn run(&self, code: String) -> Result<RunOutput, Error> {
        match tokio::time::timeout(self.timeout, self.handle.run(&code)).await {
            Ok(res) => res,
            Err(_) => Err(Error::Timeout(self.timeout)),
        }
    }

    async fn reload_sdk(&self, sdk_py: &str) -> Result<(), Error> {
        // Rewrite the mounted sdk.py on the host (visible read-only in the
        // container), then ask the worker to re-import it.
        std::fs::write(self.workdir.join("sdk.py"), sdk_py)?;
        self.handle.reload(sdk_py).await
    }

    async fn shutdown(&self) {
        if let Err(e) = remove_container(&self.docker, &self.container_id).await {
            tracing::warn!(error = %e, "failed to remove worker container during shutdown");
        }
        let _ = std::fs::remove_dir_all(&self.workdir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ipam_with(configs: Vec<IpamConfig>) -> Option<Ipam> {
        Some(Ipam {
            driver: Some("default".into()),
            config: Some(configs),
            options: None,
        })
    }

    #[test]
    fn gateway_of_picks_first_non_empty_gateway() {
        let ipam = ipam_with(vec![
            IpamConfig {
                subnet: Some("172.18.0.0/16".into()),
                gateway: Some("172.18.0.1".into()),
                ..Default::default()
            },
            IpamConfig {
                gateway: Some("10.0.0.1".into()),
                ..Default::default()
            },
        ]);
        assert_eq!(gateway_of(&ipam).as_deref(), Some("172.18.0.1"));
    }

    #[test]
    fn gateway_of_skips_empty_then_finds_next() {
        let ipam = ipam_with(vec![
            IpamConfig {
                gateway: Some(String::new()),
                ..Default::default()
            },
            IpamConfig {
                gateway: Some("172.20.0.1".into()),
                ..Default::default()
            },
        ]);
        assert_eq!(gateway_of(&ipam).as_deref(), Some("172.20.0.1"));
    }

    #[test]
    fn gateway_of_none_when_absent() {
        assert_eq!(gateway_of(&None), None);
        assert_eq!(gateway_of(&ipam_with(vec![])), None);
        let no_gw = ipam_with(vec![IpamConfig {
            subnet: Some("172.18.0.0/16".into()),
            gateway: None,
            ..Default::default()
        }]);
        assert_eq!(gateway_of(&no_gw), None);
    }
}
