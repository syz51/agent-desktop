use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use desktop_proto::{
    DEFAULT_CONNECT_TIMEOUT_MS, ErrorCode, FleetConfig, HostEntryStatus, PeerHostConfig, RpcError,
    RpcMethod, RpcRequest, RpcResponse, RpcResult, resolve_token,
};
use futures_util::future::join_all;
use rand::RngExt as _;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

const CLIENT_ID_HEADER: &str = "x-desktop-client-id";

/// Resolve the local `desktopd` socket path without pulling daemon dependencies into `desktop`.
#[must_use]
pub fn default_socket_path() -> PathBuf {
    if let Ok(custom) = std::env::var("DESKTOP_SOCKET")
        && !custom.trim().is_empty()
    {
        return PathBuf::from(custom);
    }
    #[cfg(target_os = "macos")]
    {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("agent-desktop")
            .join("desktopd.sock")
    }
    #[cfg(not(target_os = "macos"))]
    {
        dirs::runtime_dir()
            .or_else(dirs::state_dir)
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("agent-desktop")
            .join("desktopd.sock")
    }
}

/// Resolve the private fleet config path (`~/.config/agent-desktop/fleet.toml`).
#[must_use]
pub fn default_fleet_path() -> PathBuf {
    if let Ok(custom) = std::env::var("DESKTOP_FLEET")
        && !custom.trim().is_empty()
    {
        return PathBuf::from(custom);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".config")
        .join("agent-desktop")
        .join("fleet.toml")
}

struct Conn {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

/// Multi-host RPC client: uses a persistent Unix socket for `"local"` and Tailscale HTTP/JSON-RPC
/// for named fleet peers in `fleet.toml`.
pub struct DesktopClient {
    socket_path: PathBuf,
    fleet_path: PathBuf,
    caller: String,
    remote_client_id: u64,
    conn: Mutex<Option<Conn>>,
    http: reqwest::Client,
    sessions: Mutex<HashMap<String, String>>,
    next_id: AtomicU64,
}

impl DesktopClient {
    #[must_use]
    pub fn new(socket_path: PathBuf, caller: impl Into<String>) -> Self {
        Self::new_with_fleet(socket_path, default_fleet_path(), caller)
    }

    #[must_use]
    pub fn new_with_fleet(
        socket_path: PathBuf,
        fleet_path: PathBuf,
        caller: impl Into<String>,
    ) -> Self {
        let remote_client_id = rand::rng().random::<u64>().max(1);
        let http = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .expect("build reqwest client");
        Self {
            socket_path,
            fleet_path,
            caller: caller.into(),
            remote_client_id,
            conn: Mutex::new(None),
            http,
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    #[must_use]
    pub fn fleet_path(&self) -> &Path {
        &self.fleet_path
    }

    pub async fn call(&self, host: Option<&str>, method: RpcMethod) -> Result<RpcResult, RpcError> {
        let alias = host
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("local");
        if alias == "local" {
            self.call_local(method).await
        } else {
            self.call_remote(alias, method, None).await
        }
    }

    /// Query `"local"` and all peers configured in `fleet.toml` concurrently.
    pub async fn list_hosts(&self) -> Vec<HostEntryStatus> {
        let fleet = FleetConfig::load_from(&self.fleet_path).unwrap_or_default();

        let local_fut = async {
            let start = Instant::now();
            match self.call_local(RpcMethod::Status).await {
                Ok(RpcResult::Status(st)) => HostEntryStatus {
                    alias: "local".into(),
                    addr: self.socket_path.display().to_string(),
                    reachable: true,
                    latency_ms: start.elapsed().as_millis() as u64,
                    status: Some(st),
                    error: None,
                },
                Ok(_) => HostEntryStatus {
                    alias: "local".into(),
                    addr: self.socket_path.display().to_string(),
                    reachable: false,
                    latency_ms: start.elapsed().as_millis() as u64,
                    status: None,
                    error: Some("unexpected response".into()),
                },
                Err(err) => HostEntryStatus {
                    alias: "local".into(),
                    addr: self.socket_path.display().to_string(),
                    reachable: false,
                    latency_ms: start.elapsed().as_millis() as u64,
                    status: None,
                    error: Some(err.message),
                },
            }
        };

        let peer_futs = fleet.hosts.into_iter().map(|(alias, peer)| async move {
            let start = Instant::now();
            let addr = peer.base_url();
            let timeout = Duration::from_millis(
                peer.connect_timeout_ms
                    .unwrap_or(DEFAULT_CONNECT_TIMEOUT_MS)
                    .max(1500),
            );
            match self
                .call_peer(&alias, &peer, RpcMethod::Status, Some(timeout))
                .await
            {
                Ok(RpcResult::Status(st)) => HostEntryStatus {
                    alias,
                    addr,
                    reachable: true,
                    latency_ms: start.elapsed().as_millis() as u64,
                    status: Some(st),
                    error: None,
                },
                Ok(_) => HostEntryStatus {
                    alias,
                    addr,
                    reachable: false,
                    latency_ms: start.elapsed().as_millis() as u64,
                    status: None,
                    error: Some("unexpected response".into()),
                },
                Err(err) => HostEntryStatus {
                    alias,
                    addr,
                    reachable: false,
                    latency_ms: start.elapsed().as_millis() as u64,
                    status: None,
                    error: Some(err.message),
                },
            }
        });

        let (local, mut peers) = tokio::join!(local_fut, join_all(peer_futs));
        let mut all = Vec::with_capacity(1 + peers.len());
        all.push(local);
        all.append(&mut peers);
        all
    }

    /// Notify a remote peer that this client is disconnecting so its lease frees immediately.
    pub async fn disconnect_remote(&self, alias: &str) -> Result<(), RpcError> {
        if alias.is_empty() || alias == "local" {
            return Ok(());
        }
        let fleet = FleetConfig::load_from(&self.fleet_path)?;
        let Some(peer) = fleet.hosts.get(alias) else {
            return Ok(());
        };
        let token = resolve_token(
            peer.token.as_deref(),
            peer.token_file.as_deref(),
            dirs::home_dir().as_deref(),
        )?;
        let url = format!("{}/v1/disconnect", peer.base_url());
        let _ = self
            .http
            .post(url)
            .bearer_auth(token)
            .header(CLIENT_ID_HEADER, self.remote_client_id.to_string())
            .timeout(Duration::from_secs(2))
            .send()
            .await;
        self.sessions.lock().await.remove(alias);
        Ok(())
    }

    /// Release leases on every remote peer this client contacted (called when MCP stdio exits).
    pub async fn disconnect_all_remotes(&self) {
        let remotes: Vec<String> = {
            let guard = self.sessions.lock().await;
            guard
                .keys()
                .filter(|k| k.as_str() != "local")
                .cloned()
                .collect()
        };
        for alias in remotes {
            let _ = self.disconnect_remote(&alias).await;
        }
    }

    async fn call_local(&self, method: RpcMethod) -> Result<RpcResult, RpcError> {
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            *guard = Some(self.connect_uds().await?);
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let session_id = self.sessions.lock().await.get("local").cloned();
        let req = RpcRequest {
            id,
            session_id,
            caller: Some(self.caller.clone()),
            method,
        };

        let first_try = send_on(guard.as_mut().expect("connected"), &req).await;
        let resp = match first_try {
            Ok(resp) => resp,
            Err(_) => {
                *guard = Some(self.connect_uds().await?);
                send_on(guard.as_mut().expect("reconnected"), &req).await?
            }
        };

        let result = resp.into_result()?;
        self.update_session_cache("local", &result).await;
        Ok(result)
    }

    async fn call_remote(
        &self,
        alias: &str,
        method: RpcMethod,
        total_timeout: Option<Duration>,
    ) -> Result<RpcResult, RpcError> {
        let fleet = FleetConfig::load_from(&self.fleet_path)?;
        let peer = fleet.hosts.get(alias).ok_or_else(|| {
            let known: Vec<&str> = fleet.hosts.keys().map(String::as_str).collect();
            RpcError::with_next(
                ErrorCode::NotFound,
                format!("unknown host {alias:?} (configured peers: {known:?})"),
                format!(
                    "Add [hosts.{alias}] to {} or run `desktop hosts` to see configured peers.",
                    self.fleet_path.display()
                ),
            )
        })?;
        self.call_peer(alias, peer, method, total_timeout).await
    }

    async fn call_peer(
        &self,
        alias: &str,
        peer: &PeerHostConfig,
        method: RpcMethod,
        total_timeout: Option<Duration>,
    ) -> Result<RpcResult, RpcError> {
        let token = resolve_token(
            peer.token.as_deref(),
            peer.token_file.as_deref(),
            dirs::home_dir().as_deref(),
        )?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let session_id = self.sessions.lock().await.get(alias).cloned();
        let req = RpcRequest {
            id,
            session_id,
            caller: Some(format!("{}@remote", self.caller)),
            method,
        };

        let url = format!("{}/v1/rpc", peer.base_url());
        let connect_ms = peer
            .connect_timeout_ms
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT_MS);
        let timeout = total_timeout.unwrap_or(Duration::from_secs(30));

        // Use a dedicated short connect timeout so sleeping Macs fail fast instead of hanging.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(connect_ms))
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| self.http.clone());

        let resp = client
            .post(&url)
            .bearer_auth(&token)
            .header(CLIENT_ID_HEADER, self.remote_client_id.to_string())
            .json(&req)
            .send()
            .await
            .map_err(|err| {
                RpcError::with_next(
                    ErrorCode::BackendError,
                    format!("could not reach host {alias:?} at {}: {err}", peer.base_url()),
                    "Verify the peer machine is awake, Tailscale is connected, and desktopd is listening.",
                )
            })?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(RpcError::with_next(
                ErrorCode::PermissionDenied,
                format!("host {alias:?} rejected bearer token (401 Unauthorized)"),
                "Ensure both machines share the same token in ~/.config/agent-desktop/token.",
            ));
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            let body = resp.text().await.unwrap_or_default();
            return Err(RpcError::with_next(
                ErrorCode::PermissionDenied,
                format!("host {alias:?} refused Tailscale peer (403 Forbidden): {body}"),
                "Verify both machines belong to the same Tailscale tailnet.",
            ));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RpcError::new(
                ErrorCode::BackendError,
                format!("host {alias:?} returned HTTP {status}: {body}"),
            ));
        }

        let rpc_resp: RpcResponse = resp.json().await.map_err(|err| {
            RpcError::new(
                ErrorCode::Internal,
                format!("decode RPC response from {alias:?}: {err}"),
            )
        })?;
        let result = rpc_resp.into_result()?;
        // Track that this client touched `alias` so `disconnect_all_remotes()` cleans up auto-leases too.
        self.sessions
            .lock()
            .await
            .entry(alias.to_string())
            .or_default();
        self.update_session_cache(alias, &result).await;
        Ok(result)
    }

    async fn update_session_cache(&self, alias: &str, result: &RpcResult) {
        match result {
            RpcResult::Session { lease, .. } => {
                self.sessions
                    .lock()
                    .await
                    .insert(alias.to_string(), lease.session_id.clone());
            }
            RpcResult::Released { .. } | RpcResult::Stopped { .. } => {
                self.sessions.lock().await.remove(alias);
            }
            _ => {}
        }
    }

    async fn connect_uds(&self) -> Result<Conn, RpcError> {
        let stream = UnixStream::connect(&self.socket_path).await.map_err(|err| {
            RpcError::with_next(
                ErrorCode::BackendError,
                format!(
                    "could not connect to desktopd at {}: {err}",
                    self.socket_path.display()
                ),
                "Start AgentDesktop.app (or run `desktopd` / `launchctl kickstart -k gui/$(id -u)/com.syz.agent-desktop`).",
            )
        })?;
        let (r, w) = stream.into_split();
        Ok(Conn {
            reader: BufReader::new(r),
            writer: w,
        })
    }
}

async fn send_on(conn: &mut Conn, req: &RpcRequest) -> Result<RpcResponse, RpcError> {
    let mut payload = serde_json::to_vec(req)
        .map_err(|e| RpcError::new(ErrorCode::Internal, format!("encode request: {e}")))?;
    payload.push(b'\n');
    conn.writer
        .write_all(&payload)
        .await
        .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("socket write: {e}")))?;
    conn.writer
        .flush()
        .await
        .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("socket flush: {e}")))?;

    let mut line = String::new();
    let n = conn
        .reader
        .read_line(&mut line)
        .await
        .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("socket read: {e}")))?;
    if n == 0 {
        return Err(RpcError::new(
            ErrorCode::BackendError,
            "desktopd closed the connection",
        ));
    }
    serde_json::from_str(&line)
        .map_err(|e| RpcError::new(ErrorCode::Internal, format!("decode response: {e}")))
}
