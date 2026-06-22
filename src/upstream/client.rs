//! Connect to a single upstream MCP server (stdio or streamable-http).

use std::collections::BTreeMap;

use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{
    streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    TokioChildProcess,
};
use rmcp::ServiceExt;
use tokio::process::Command;

use crate::config::ServerSpec;
use crate::error::Error;

/// A live connection to one upstream server. The unit handler `()` is a valid
/// client that just doesn't react to server-initiated requests.
pub type UpstreamService = RunningService<RoleClient, ()>;

/// Connect to the upstream described by `spec`.
pub async fn connect(name: &str, spec: &ServerSpec) -> Result<UpstreamService, Error> {
    match spec {
        ServerSpec::Local {
            command,
            environment,
            cwd,
            ..
        } => connect_stdio(name, command, environment, cwd.as_deref()).await,
        ServerSpec::Remote { url, headers, .. } => connect_http(name, url, headers).await,
    }
}

async fn connect_stdio(
    name: &str,
    command: &[String],
    environment: &BTreeMap<String, String>,
    cwd: Option<&str>,
) -> Result<UpstreamService, Error> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| Error::Config(format!("upstream {name}: empty command")))?;

    let mut cmd = Command::new(program);
    cmd.args(args);
    for (k, v) in environment {
        cmd.env(k, v);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let transport = TokioChildProcess::new(cmd)
        .map_err(|e| Error::Upstream(format!("{name}: spawn failed: {e}")))?;

    let service = ()
        .serve(transport)
        .await
        .map_err(|e| Error::Upstream(format!("{name}: initialize failed: {e}")))?;
    Ok(service)
}

async fn connect_http(
    name: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
) -> Result<UpstreamService, Error> {
    let transport = if headers.is_empty() {
        StreamableHttpClientTransport::from_uri(url.to_string())
    } else {
        // Apply arbitrary headers (Authorization, API keys, etc.) by baking them
        // into a custom reqwest client used for every request.
        let mut header_map = reqwest::header::HeaderMap::new();
        for (k, v) in headers {
            let name_hdr = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| Error::Config(format!("{name}: invalid header name {k:?}: {e}")))?;
            let val_hdr = reqwest::header::HeaderValue::from_str(v)
                .map_err(|e| Error::Config(format!("{name}: invalid header value for {k:?}: {e}")))?;
            header_map.insert(name_hdr, val_hdr);
        }
        let client = reqwest::Client::builder()
            .default_headers(header_map)
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|e| Error::Upstream(format!("{name}: http client build failed: {e}")))?;
        // `StreamableHttpClientTransportConfig` is `#[non_exhaustive]`; build via
        // Default and set the public `uri` field.
        let mut config = StreamableHttpClientTransportConfig::default();
        config.uri = url.to_string().into();
        StreamableHttpClientTransport::with_client(client, config)
    };

    let service = ()
        .serve(transport)
        .await
        .map_err(|e| Error::Upstream(format!("{name}: initialize failed: {e}")))?;
    Ok(service)
}
