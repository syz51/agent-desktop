use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use desktop_core::DesktopEngine;
use desktop_proto::{
    ErrorCode, ListenConfig, RpcError, RpcRequest, RpcResponse, ScreenshotParams, resolve_token,
    validate_bind_ip,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq as _;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::tailscale::{TailscaleWhoisCache, detect_tailscale_ipv4};

const MAX_HTTP_BODY_BYTES: usize = 32 * 1024 * 1024;
/// High bit set on remote HTTP client IDs so they never collide with local UDS client IDs.
const REMOTE_CLIENT_MASK: u64 = 1u64 << 63;

pub const CLIENT_ID_HEADER: &str = "x-desktop-client-id";
pub const META_HEADER: &str = "x-desktop-meta";

#[derive(Debug, Deserialize)]
struct BinaryScreenshotRequest {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    caller: Option<String>,
    params: ScreenshotParams,
}

struct RemoteState {
    engine: Arc<DesktopEngine>,
    token_hash: [u8; 32],
    require_tailscale: bool,
    whois: TailscaleWhoisCache,
}

/// Resolve `[listen]` config, bind a Tailscale/loopback TCP socket, and return `(listener, addr, router)`.
pub async fn bind_http(
    listen: &ListenConfig,
    engine: Arc<DesktopEngine>,
) -> Result<(TcpListener, SocketAddr, Router)> {
    let home = dirs::home_dir();
    let token = resolve_token(
        listen.token.as_deref(),
        listen.token_file.as_deref(),
        home.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let bind_ip: IpAddr = if listen.bind.eq_ignore_ascii_case("tailscale") {
        detect_tailscale_ipv4()?
    } else {
        listen
            .bind
            .parse()
            .with_context(|| format!("invalid listen.bind IP {:?}", listen.bind))?
    };

    validate_bind_ip(bind_ip, listen.allow_non_tailscale).map_err(|e| anyhow::anyhow!("{e}"))?;

    let addr = SocketAddr::new(bind_ip, listen.port);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind desktopd HTTP listener on {addr}"))?;
    let local_addr = listener.local_addr()?;

    let token_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let state = Arc::new(RemoteState {
        engine,
        token_hash,
        require_tailscale: listen.require_tailscale,
        whois: TailscaleWhoisCache::new(),
    });

    let router = Router::new()
        .route("/v1/health", get(handle_health))
        .route("/v1/rpc", post(handle_rpc))
        .route("/v1/screenshot", post(handle_screenshot_binary))
        .route("/v1/disconnect", post(handle_disconnect))
        .layer(DefaultBodyLimit::max(MAX_HTTP_BODY_BYTES))
        .with_state(state);

    Ok((listener, local_addr, router))
}

/// Serve a bound HTTP listener until `shutdown` signals `true`.
pub async fn serve_http_listener(
    listener: TcpListener,
    router: Router,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let local_addr = listener.local_addr()?;
    tracing::info!(%local_addr, "desktopd listening for Tailscale fleet RPC");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = shutdown.changed().await;
    })
    .await
    .context("serve desktopd HTTP listener")
}

fn verify_auth(
    state: &RemoteState,
    peer: SocketAddr,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, Json<Value>)> {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));

    let Some(provided) = auth else {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing Authorization: Bearer token" })),
        ));
    };

    let provided_hash: [u8; 32] = Sha256::digest(provided.as_bytes()).into();
    if provided_hash.ct_eq(&state.token_hash).unwrap_u8() != 1 {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid bearer token" })),
        ));
    }

    if state.require_tailscale
        && let Err(reason) = state.whois.verify_peer(peer.ip())
    {
        return Err((StatusCode::FORBIDDEN, Json(json!({ "error": reason }))));
    }
    Ok(())
}

fn parse_client_id(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CLIENT_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|id| id | REMOTE_CLIENT_MASK)
}

async fn handle_health(
    State(state): State<Arc<RemoteState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    verify_auth(&state, peer, &headers)?;
    Ok(Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
    })))
}

async fn handle_rpc(
    State(state): State<Arc<RemoteState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<RpcRequest>,
) -> Result<Json<RpcResponse>, (StatusCode, Json<Value>)> {
    verify_auth(&state, peer, &headers)?;
    let client_id = parse_client_id(&headers);
    let id = req.id;
    let engine = Arc::clone(&state.engine);
    let res = tokio::task::spawn_blocking(move || {
        engine.handle(
            client_id,
            req.session_id.as_deref(),
            req.caller.as_deref(),
            req.method,
        )
    })
    .await;

    let response = match res {
        Ok(Ok(val)) => RpcResponse::ok(id, val),
        Ok(Err(err)) => RpcResponse::err(id, err),
        Err(join_err) => RpcResponse::err(
            id,
            RpcError::new(ErrorCode::Internal, format!("worker panic: {join_err}")),
        ),
    };
    Ok(Json(response))
}

async fn handle_screenshot_binary(
    State(state): State<Arc<RemoteState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<BinaryScreenshotRequest>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    verify_auth(&state, peer, &headers)?;
    let client_id = parse_client_id(&headers);
    let engine = Arc::clone(&state.engine);
    let res = tokio::task::spawn_blocking(move || {
        engine.capture_screenshot_binary(
            client_id,
            req.session_id.as_deref(),
            req.caller.as_deref(),
            &req.params,
        )
    })
    .await;

    let (meta, bytes_opt) = match res {
        Ok(Ok(pair)) => pair,
        Ok(Err(err)) => {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::to_value(err).unwrap_or_else(|_| json!({"error": "rpc error"}))),
            ));
        }
        Err(join_err) => {
            let err = RpcError::new(ErrorCode::Internal, format!("worker panic: {join_err}"));
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::to_value(err).unwrap_or_else(|_| json!({"error": "panic"}))),
            ));
        }
    };

    let meta_json = serde_json::to_string(&meta).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("encode screenshot meta: {e}") })),
        )
    })?;
    let meta_val = HeaderValue::from_str(&meta_json).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("invalid screenshot meta header: {e}") })),
        )
    })?;
    let content_type = HeaderValue::from_str(&meta.mime_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));

    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(HeaderName::from_static(META_HEADER), meta_val);
    let body = match bytes_opt {
        Some(bytes) => Body::from(bytes),
        None => {
            resp = resp.header("x-desktop-unchanged", "1");
            Body::empty()
        }
    };
    resp.body(body).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("build binary screenshot response: {e}") })),
        )
    })
}

async fn handle_disconnect(
    State(state): State<Arc<RemoteState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    verify_auth(&state, peer, &headers)?;
    if let Some(client_id) = parse_client_id(&headers) {
        let engine = Arc::clone(&state.engine);
        let _ = tokio::task::spawn_blocking(move || engine.on_client_disconnect(client_id)).await;
    }
    Ok(Json(json!({ "ok": true })))
}
