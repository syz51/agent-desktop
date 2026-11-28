use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use desktop_proto::{
    DEFAULT_CONNECT_TIMEOUT_MS, ErrorCode, FleetConfig, HostEntryStatus, PeerHostConfig, RpcError,
    RpcMethod, RpcRequest, RpcResponse, RpcResult, ScreenshotParams, ScreenshotResult,
    resolve_token,
};
use futures_util::future::join_all;
use rand::RngExt as _;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

const CLIENT_ID_HEADER: &str = "x-desktop-client-id";
const META_HEADER: &str = "x-desktop-meta";

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

type FileStamp = (SystemTime, u64);
type FleetCache = Option<(Option<FileStamp>, FleetConfig)>;
type TokenCache = HashMap<PathBuf, (FileStamp, String)>;

/// Multi-host RPC client: uses a persistent Unix socket for `"local"` and Tailscale HTTP/JSON-RPC
/// for named fleet peers in `fleet.toml`.
pub struct DesktopClient {
    socket_path: PathBuf,
    fleet_path: PathBuf,
    caller: String,
    remote_client_id: u64,
    conn: Mutex<Option<Conn>>,
    http: reqwest::Client,
    /// Pooled HTTP clients keyed by `connect_timeout_ms` to preserve TCP keep-alive connections.
    http_pool: Mutex<HashMap<u64, reqwest::Client>>,
    /// `(mtime, byte_len)`-keyed cache of `fleet.toml` so repeated remote RPCs avoid disk reads
    /// and TOML parsing while noticing manual edits immediately.
    fleet_cache: Mutex<FleetCache>,
    /// `(mtime, byte_len)`-keyed cache of bearer token files so remote RPCs avoid reading token
    /// files on every call while still reloading on rotation.
    token_cache: Mutex<TokenCache>,
    /// Tracks active hosts (`Some(session_id)` for explicit sessions, `None` for auto-acquired
    /// remote sessions that still need `/v1/disconnect` cleanup on exit).
    sessions: Mutex<HashMap<String, Option<String>>>,
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
            .tcp_nodelay(true)
            .build()
            .expect("build reqwest client");
        Self {
            socket_path,
            fleet_path,
            caller: caller.into(),
            remote_client_id,
            conn: Mutex::new(None),
            http,
            http_pool: Mutex::new(HashMap::new()),
            fleet_cache: Mutex::new(None),
            token_cache: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    async fn load_fleet_cached(&self) -> Result<FleetConfig, RpcError> {
        let stamp = match std::fs::metadata(&self.fleet_path) {
            Ok(meta) => meta.modified().ok().map(|t| Some((t, meta.len()))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(None),
            Err(_) => None,
        };
        if let Some(current_stamp) = stamp {
            let guard = self.fleet_cache.lock().await;
            if let Some((cached_stamp, cfg)) = guard.as_ref()
                && *cached_stamp == current_stamp
            {
                return Ok(cfg.clone());
            }
        }
        let cfg = FleetConfig::load_from(&self.fleet_path)?;
        if let Some(current_stamp) = stamp {
            *self.fleet_cache.lock().await = Some((current_stamp, cfg.clone()));
        }
        Ok(cfg)
    }

    async fn resolve_token_cached(&self, peer: &PeerHostConfig) -> Result<String, RpcError> {
        if let Some(inline) = peer
            .token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Ok(inline.to_string());
        }
        let home = dirs::home_dir();
        if let Some(raw_path) = peer.token_file.as_deref() {
            let expanded = desktop_proto::expand_tilde(raw_path, home.as_deref());
            let stamp = std::fs::metadata(&expanded)
                .ok()
                .and_then(|m| m.modified().ok().map(|t| (t, m.len())));
            if let Some(current_stamp) = stamp {
                let guard = self.token_cache.lock().await;
                if let Some((cached_stamp, tok)) = guard.get(&expanded)
                    && *cached_stamp == current_stamp
                {
                    return Ok(tok.clone());
                }
            }
            let tok = resolve_token(None, Some(raw_path), home.as_deref())?;
            if let Some(current_stamp) = stamp {
                self.token_cache
                    .lock()
                    .await
                    .insert(expanded, (current_stamp, tok.clone()));
            }
            return Ok(tok);
        }
        resolve_token(
            peer.token.as_deref(),
            peer.token_file.as_deref(),
            home.as_deref(),
        )
    }

    async fn pooled_http(&self, connect_ms: u64) -> reqwest::Client {
        let mut pool = self.http_pool.lock().await;
        if let Some(existing) = pool.get(&connect_ms) {
            return existing.clone();
        }
        let built = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(connect_ms))
            .pool_idle_timeout(Duration::from_secs(60))
            .tcp_nodelay(true)
            .build()
            .unwrap_or_else(|_| self.http.clone());
        pool.insert(connect_ms, built.clone());
        built
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
        let fleet = self.load_fleet_cached().await.unwrap_or_default();

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
        let fleet = self.load_fleet_cached().await?;
        let Some(peer) = fleet.hosts.get(alias) else {
            return Ok(());
        };
        let token = self.resolve_token_cached(peer).await?;
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

    /// Release leases on every remote peer this client contacted concurrently (called when MCP stdio exits).
    pub async fn disconnect_all_remotes(&self) {
        let remotes: Vec<String> = {
            let guard = self.sessions.lock().await;
            guard
                .keys()
                .filter(|k| k.as_str() != "local")
                .cloned()
                .collect()
        };
        join_all(remotes.into_iter().map(|alias| async move {
            let _ = self.disconnect_remote(&alias).await;
        }))
        .await;
    }

    async fn call_local(&self, method: RpcMethod) -> Result<RpcResult, RpcError> {
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            *guard = Some(self.connect_uds().await?);
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let session_id = self
            .sessions
            .lock()
            .await
            .get("local")
            .and_then(Clone::clone);
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
        let fleet = self.load_fleet_cached().await?;
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
        let token = self.resolve_token_cached(peer).await?;
        let connect_ms = peer
            .connect_timeout_ms
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT_MS);
        let timeout = total_timeout.unwrap_or(Duration::from_secs(30));
        let client = self.pooled_http(connect_ms).await;

        // Fast binary screenshot transfer: avoid 33% Base64 wire bloat over Tailscale HTTP.
        if let RpcMethod::Screenshot(ref params) = method {
            match self
                .call_peer_screenshot_binary(&client, alias, peer, &token, params, timeout)
                .await
            {
                Ok(Some(shot)) => {
                    self.sessions
                        .lock()
                        .await
                        .entry(alias.to_string())
                        .or_insert(None);
                    return Ok(RpcResult::Screenshot(Box::new(shot)));
                }
                Ok(None) => {
                    // Older daemon without `/v1/screenshot`: fall back seamlessly to `/v1/rpc`.
                }
                Err(err) => return Err(err),
            }
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let session_id = self.sessions.lock().await.get(alias).and_then(Clone::clone);
        let req = RpcRequest {
            id,
            session_id,
            caller: Some(format!("{}@remote", self.caller)),
            method,
        };

        let url = format!("{}/v1/rpc", peer.base_url());
        let resp = client
            .post(&url)
            .bearer_auth(&token)
            .header(CLIENT_ID_HEADER, self.remote_client_id.to_string())
            .timeout(timeout)
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
            .or_insert(None);
        self.update_session_cache(alias, &result).await;
        Ok(result)
    }

    async fn call_peer_screenshot_binary(
        &self,
        client: &reqwest::Client,
        alias: &str,
        peer: &PeerHostConfig,
        token: &str,
        params: &ScreenshotParams,
        timeout: Duration,
    ) -> Result<Option<ScreenshotResult>, RpcError> {
        let session_id = self.sessions.lock().await.get(alias).and_then(Clone::clone);
        let body = serde_json::json!({
            "session_id": session_id,
            "caller": format!("{}@remote", self.caller),
            "params": params,
        });
        let url = format!("{}/v1/screenshot", peer.base_url());
        let resp = client
            .post(&url)
            .bearer_auth(token)
            .header(CLIENT_ID_HEADER, self.remote_client_id.to_string())
            .timeout(timeout)
            .json(&body)
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
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(RpcError::with_next(
                ErrorCode::PermissionDenied,
                format!("host {alias:?} rejected bearer token (401 Unauthorized)"),
                "Ensure both machines share the same token in ~/.config/agent-desktop/token.",
            ));
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            let text = resp.text().await.unwrap_or_default();
            return Err(RpcError::with_next(
                ErrorCode::PermissionDenied,
                format!("host {alias:?} refused Tailscale peer (403 Forbidden): {text}"),
                "Verify both machines belong to the same Tailscale tailnet.",
            ));
        }
        if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
            let text = resp.text().await.unwrap_or_default();
            if let Ok(rpc_err) = serde_json::from_str::<RpcError>(&text) {
                return Err(rpc_err);
            }
            return Err(RpcError::new(
                ErrorCode::InvalidInput,
                format!("host {alias:?} rejected screenshot request (422): {text}"),
            ));
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(RpcError::new(
                ErrorCode::BackendError,
                format!("host {alias:?} returned HTTP {status}: {text}"),
            ));
        }

        let meta_raw = resp
            .headers()
            .get(META_HEADER)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                RpcError::new(
                    ErrorCode::Internal,
                    format!("host {alias:?} omitted {META_HEADER} header on /v1/screenshot"),
                )
            })?;
        let mut meta: ScreenshotResult = serde_json::from_str(meta_raw).map_err(|e| {
            RpcError::new(
                ErrorCode::Internal,
                format!("decode {META_HEADER} from {alias:?}: {e}"),
            )
        })?;
        let bytes = resp.bytes().await.map_err(|e| {
            RpcError::new(
                ErrorCode::BackendError,
                format!("read binary screenshot body from {alias:?}: {e}"),
            )
        })?;
        if !meta.unchanged && !bytes.is_empty() {
            meta.data_base64 = Some(BASE64.encode(&bytes));
        }
        Ok(Some(meta))
    }

    async fn update_session_cache(&self, alias: &str, result: &RpcResult) {
        match result {
            RpcResult::Session { lease, .. } => {
                self.sessions
                    .lock()
                    .await
                    .insert(alias.to_string(), Some(lease.session_id.clone()));
            }
            RpcResult::Released { .. } | RpcResult::Stopped { .. } => {
                self.sessions.lock().await.remove(alias);
            }
            RpcResult::Act(act) if act.released => {
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

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn token_cache_reloads_on_file_rotation() {
        let tmp = TempDir::new().unwrap();
        let token_path = tmp.path().join("token");
        std::fs::write(&token_path, "token-v1\n").unwrap();

        let client = DesktopClient::new_with_fleet(
            tmp.path().join("desktopd.sock"),
            tmp.path().join("fleet.toml"),
            "test-agent",
        );
        let peer = PeerHostConfig {
            addr: "127.0.0.1:9999".into(),
            token: None,
            token_file: Some(token_path.display().to_string()),
            connect_timeout_ms: None,
        };

        assert_eq!(
            client.resolve_token_cached(&peer).await.unwrap(),
            "token-v1"
        );
        assert_eq!(
            client.resolve_token_cached(&peer).await.unwrap(),
            "token-v1"
        );

        // Rotating token with a different length invalidates the (mtime, len) cache immediately.
        std::fs::write(&token_path, "token-v2-rotated\n").unwrap();
        assert_eq!(
            client.resolve_token_cached(&peer).await.unwrap(),
            "token-v2-rotated"
        );
    }

    #[tokio::test]
    async fn auto_acquired_session_entry_stays_none_until_explicit_session_result() {
        let tmp = TempDir::new().unwrap();
        let client = DesktopClient::new_with_fleet(
            tmp.path().join("desktopd.sock"),
            tmp.path().join("fleet.toml"),
            "test-agent",
        );

        client
            .sessions
            .lock()
            .await
            .entry("pro".to_string())
            .or_insert(None);
        assert_eq!(
            client
                .sessions
                .lock()
                .await
                .get("pro")
                .and_then(Clone::clone),
            None,
            "auto-acquired remote host must send session_id: None, not an empty string"
        );

        client
            .update_session_cache(
                "pro",
                &RpcResult::Released {
                    message: "released".into(),
                },
            )
            .await;
        assert!(!client.sessions.lock().await.contains_key("pro"));
    }
}
