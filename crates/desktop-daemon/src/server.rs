use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result};
use desktop_core::DesktopEngine;
use desktop_proto::{ErrorCode, RpcError, RpcRequest, RpcResponse};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Serve newline-delimited JSON RPC over a `0600` Unix domain socket until `shutdown` fires.
pub async fn serve_uds(
    socket_path: PathBuf,
    engine: Arc<DesktopEngine>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    prepare_socket_path(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind Unix socket {}", socket_path.display()))?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", socket_path.display()))?;

    tracing::info!(socket = %socket_path.display(), "desktopd listening on Unix socket");

    let next_client = Arc::new(AtomicU64::new(1));

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let client_id = next_client.fetch_add(1, Ordering::Relaxed);
                        let eng = Arc::clone(&engine);
                        tokio::spawn(async move {
                            handle_connection(client_id, stream, eng).await;
                        });
                    }
                    Err(err) => {
                        tracing::warn!(%err, "failed to accept UDS client");
                    }
                }
            }
        }
    }

    let _ = fs::remove_file(&socket_path);
    Ok(())
}

fn prepare_socket_path(socket_path: &Path) -> Result<()> {
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create socket directory {}", parent.display()))?;
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    if socket_path.exists() {
        // Check if another live daemon is already bound before removing a stale socket file.
        if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
            anyhow::bail!(
                "another desktopd instance is already listening at {}",
                socket_path.display()
            );
        }
        fs::remove_file(socket_path)
            .with_context(|| format!("remove stale socket {}", socket_path.display()))?;
    }
    Ok(())
}

async fn handle_connection(client_id: u64, stream: UnixStream, engine: Arc<DesktopEngine>) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    loop {
        line.clear();
        let read_res = reader.read_line(&mut line).await;
        match read_res {
            Ok(0) => break,
            Ok(n) if n > MAX_LINE_BYTES => {
                let resp = RpcResponse::err(
                    0,
                    RpcError::new(ErrorCode::InvalidInput, "request exceeds 16 MiB limit"),
                );
                let _ = write_response(&mut write_half, &resp).await;
                break;
            }
            Ok(_) => {}
            Err(err) => {
                tracing::debug!(client_id, %err, "client read error");
                break;
            }
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<RpcRequest>(trimmed) {
            Ok(req) => {
                let id = req.id;
                let eng = Arc::clone(&engine);
                let res = tokio::task::spawn_blocking(move || {
                    eng.handle(
                        Some(client_id),
                        req.session_id.as_deref(),
                        req.caller.as_deref(),
                        req.method,
                    )
                })
                .await;
                match res {
                    Ok(Ok(val)) => RpcResponse::ok(id, val),
                    Ok(Err(err)) => RpcResponse::err(id, err),
                    Err(join_err) => RpcResponse::err(
                        id,
                        RpcError::new(ErrorCode::Internal, format!("worker panic: {join_err}")),
                    ),
                }
            }
            Err(err) => RpcResponse::err(
                0,
                RpcError::with_next(
                    ErrorCode::InvalidInput,
                    format!("invalid RPC JSON: {err}"),
                    "Send a valid RpcRequest JSON object followed by a newline.",
                ),
            ),
        };

        if write_response(&mut write_half, &response).await.is_err() {
            break;
        }
    }

    // Drop any lease held by this client immediately on disconnect.
    let eng = Arc::clone(&engine);
    let _ = tokio::task::spawn_blocking(move || eng.on_client_disconnect(client_id)).await;
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &RpcResponse,
) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(response).unwrap_or_else(|_| b"{}".to_vec());
    buf.push(b'\n');
    writer.write_all(&buf).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use desktop_core::mock::MockBackend;
    use desktop_core::{AuditLog, DesktopEngine, PolicyStore, SystemClock};
    use desktop_proto::{RpcMethod, RpcRequest, RpcResponse, RpcResult, SnapshotParams};
    use tempfile::TempDir;
    use tokio::io::BufReader;
    use tokio::net::UnixStream;
    use tokio::sync::watch;

    use super::*;

    async fn send_rpc(stream: &mut UnixStream, req: &RpcRequest) -> RpcResponse {
        let mut line = serde_json::to_vec(req).unwrap();
        line.push(b'\n');
        stream.write_all(&line).await.unwrap();
        stream.flush().await.unwrap();

        let mut reader = BufReader::new(stream);
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).await.unwrap();
        serde_json::from_str(&resp_line).unwrap()
    }

    #[tokio::test]
    async fn uds_roundtrip_and_disconnect_releases_lease() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("desktopd.sock");
        let backend = Arc::new(MockBackend::new());
        let engine = Arc::new(DesktopEngine::new(
            backend,
            Arc::new(SystemClock),
            PolicyStore::new(tmp.path().join("policy.toml")),
            AuditLog::new(tmp.path()),
        ));

        let (tx, rx) = watch::channel(false);
        let server_sock = sock.clone();
        let server_task = tokio::spawn(async move { serve_uds(server_sock, engine, rx).await });

        // Wait briefly for socket creation.
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Client 1 connects and takes a snapshot (auto-acquires lease).
        let mut c1 = UnixStream::connect(&sock).await.unwrap();
        let r1 = send_rpc(
            &mut c1,
            &RpcRequest {
                id: 1,
                session_id: None,
                caller: Some("client1".into()),
                method: RpcMethod::Snapshot(SnapshotParams::default()),
            },
        )
        .await;
        assert!(matches!(r1.into_result().unwrap(), RpcResult::Snapshot(_)));

        // Client 2 is blocked by LeaseBusy while client 1 stays connected.
        let mut c2 = UnixStream::connect(&sock).await.unwrap();
        let r2 = send_rpc(
            &mut c2,
            &RpcRequest {
                id: 2,
                session_id: None,
                caller: Some("client2".into()),
                method: RpcMethod::Snapshot(SnapshotParams::default()),
            },
        )
        .await;
        assert_eq!(r2.into_result().unwrap_err().code, ErrorCode::LeaseBusy);

        // Drop client 1 -> server detects EOF and frees lease immediately.
        drop(c1);
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;

        let r3 = send_rpc(
            &mut c2,
            &RpcRequest {
                id: 3,
                session_id: None,
                caller: Some("client2".into()),
                method: RpcMethod::Snapshot(SnapshotParams::default()),
            },
        )
        .await;
        assert!(matches!(r3.into_result().unwrap(), RpcResult::Snapshot(_)));

        let _ = tx.send(true);
        let _ = server_task.await;
    }
}
