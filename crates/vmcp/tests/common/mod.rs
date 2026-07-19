//! Shared helpers for spawning a local HTTP gateway under test.
//!
//! Boots a real `vmcp serve` (Streamable-HTTP ingress) on an ephemeral port,
//! waits for `/health` to answer `ok`, and hands back a [`Gateway`] whose
//! child process is killed on drop. Tests connect to `mcp_url` with an rmcp
//! [`StreamableHttpClientTransport`].

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::process::{Child, Command};

/// RAII temp dir that removes itself on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("{prefix}-{nanos}"));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Pick an ephemeral free TCP port on 127.0.0.1.
///
/// Binds to port 0, reads the assigned port, then drops the listener. There is
/// a small race window before the gateway rebinds, but on a loopback test host
/// it is negligible in practice.
pub async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

/// Poll `http://127.0.0.1:{port}/health` until it answers `ok` or `timeout`.
pub async fn wait_health(port: u16, timeout: Duration) -> Result<(), String> {
    let url = format!("http://127.0.0.1:{port}/health");
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(resp) = reqwest::get(&url).await {
            if resp.status().is_success() {
                if let Ok(body) = resp.text().await {
                    if body.trim() == "ok" {
                        return Ok(());
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("gateway /health not ready within {timeout:?}"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A running `vmcp serve` gateway. Killed on drop.
pub struct Gateway {
    pub port: u16,
    /// `http://127.0.0.1:{port}/mcp`
    pub mcp_url: String,
    _child: Child,
}

/// Spawn `vmcp --config <cfg> serve` bound to an ephemeral loopback port.
///
/// Host / port / public base URL are supplied via env overrides so configs do
/// not need to bake in the free port. Auth is forced off as belt-and-suspenders.
pub async fn spawn_gateway(cfg: &Path) -> Gateway {
    spawn_gateway_inner(cfg, true).await
}

/// Like [`spawn_gateway`], but leaves `auth.enabled` as configured (for auth e2e).
pub async fn spawn_gateway_auth(cfg: &Path) -> Gateway {
    spawn_gateway_inner(cfg, false).await
}

async fn spawn_gateway_inner(cfg: &Path, force_auth_off: bool) -> Gateway {
    let port = free_port().await;
    let base = format!("http://127.0.0.1:{port}");
    let exe = env!("CARGO_BIN_EXE_vmcp");

    let mut cmd = Command::new(exe);
    cmd.arg("--config")
        .arg(cfg)
        .arg("serve")
        .env("VMCP_HOST", "127.0.0.1")
        .env("VMCP_PORT", port.to_string())
        .env("VMCP_PUBLIC_BASE_URL", &base)
        .env("RUST_LOG", "warn")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if force_auth_off {
        cmd.env("VMCP_AUTH__ENABLED", "false");
    }

    let child = cmd.spawn().expect("spawn vmcp serve");

    wait_health(port, Duration::from_secs(30))
        .await
        .expect("gateway became healthy");

    Gateway {
        port,
        mcp_url: format!("{base}/mcp"),
        _child: child,
    }
}

/// Connect an MCP client over Streamable HTTP and complete the handshake.
pub async fn connect_client<H>(
    handler: H,
    url: impl Into<String>,
) -> rmcp::service::RunningService<rmcp::RoleClient, H>
where
    H: rmcp::ClientHandler,
{
    connect_client_with_token(handler, url, None).await
}

/// Connect with an optional bearer token (static `vmcp_…` or JWT, no `Bearer ` prefix).
pub async fn connect_client_with_token<H>(
    handler: H,
    url: impl Into<String>,
    bearer: Option<&str>,
) -> rmcp::service::RunningService<rmcp::RoleClient, H>
where
    H: rmcp::ClientHandler,
{
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::ServiceExt;

    let mut config = StreamableHttpClientTransportConfig::with_uri(url.into());
    if let Some(token) = bearer {
        config = config.auth_header(token);
    }
    let transport = StreamableHttpClientTransport::from_config(config);
    handler.serve(transport).await.expect("MCP handshake")
}
