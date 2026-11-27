use std::sync::Arc;

use desktop::client::DesktopClient;
use desktop::mcp::DesktopMcp;
use desktop_core::mock::MockBackend;
use desktop_core::{AuditLog, DesktopEngine, PolicyStore, SystemClock};
use desktop_daemon::remote::{bind_http, serve_http_listener};
use desktop_daemon::server::serve_uds;
use desktop_proto::{
    ActParams, ActionStep, ClickMode, ErrorCode, FocusMode, ListenConfig, MouseButton, PostAction,
    RpcMethod, RpcResult, ScreenshotParams, SnapshotParams, Target, TextMethod,
};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::watch;

#[tokio::test]
async fn tailscale_http_fleet_auth_routing_leases_and_list_hosts() {
    let tmp = TempDir::new().unwrap();
    let local_sock = tmp.path().join("local.sock");
    let token_path = tmp.path().join("token");
    std::fs::write(&token_path, "super-secret-fleet-token\n").unwrap();

    // 1. Start local UDS desktopd.
    let local_engine = Arc::new(DesktopEngine::new(
        Arc::new(MockBackend::new()),
        Arc::new(SystemClock),
        PolicyStore::new(tmp.path().join("local-policy.toml")),
        AuditLog::new(&tmp.path().join("local-state")),
    ));
    let (tx, rx) = watch::channel(false);
    let s_sock = local_sock.clone();
    let uds_task = tokio::spawn(async move { serve_uds(s_sock, local_engine, rx).await });
    for _ in 0..50 {
        if local_sock.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // 2. Start remote HTTP peer desktopd on loopback ephemeral port.
    let peer_engine = Arc::new(DesktopEngine::new(
        Arc::new(MockBackend::new()),
        Arc::new(SystemClock),
        PolicyStore::new(tmp.path().join("peer-policy.toml")),
        AuditLog::new(&tmp.path().join("peer-state")),
    ));
    let listen = ListenConfig {
        bind: "127.0.0.1".into(),
        port: 0,
        token: None,
        token_file: Some(token_path.display().to_string()),
        require_tailscale: true, // loopback allowed cleanly in tests
        allow_non_tailscale: false,
    };
    let (listener, peer_addr, router) = bind_http(&listen, peer_engine).await.unwrap();
    let http_rx = tx.subscribe();
    let http_task =
        tokio::spawn(async move { serve_http_listener(listener, router, http_rx).await });

    // 3. Verify bearer token auth enforcement directly over HTTP.
    let http = reqwest::Client::new();
    let health_url = format!("http://{peer_addr}/v1/health");
    assert_eq!(
        http.get(&health_url).send().await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http.get(&health_url)
            .bearer_auth("wrong-token")
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http.get(&health_url)
            .bearer_auth("super-secret-fleet-token")
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );

    // 4. Write fleet.toml with one reachable peer and one unreachable/offline peer.
    let fleet_path = tmp.path().join("fleet.toml");
    std::fs::write(
        &fleet_path,
        format!(
            "[hosts.pro]\naddr = \"{peer_addr}\"\ntoken_file = \"{}\"\n\n\
             [hosts.offline]\naddr = \"127.0.0.1:1\"\ntoken = \"x\"\nconnect_timeout_ms = 200\n",
            token_path.display()
        ),
    )
    .unwrap();

    // 5. list_hosts queries local + pro + offline concurrently.
    let client1 = DesktopClient::new_with_fleet(local_sock.clone(), fleet_path.clone(), "agent-1");
    let hosts = client1.list_hosts().await;
    assert_eq!(hosts.len(), 3);
    assert_eq!(hosts[0].alias, "local");
    assert!(hosts[0].reachable);
    let pro = hosts.iter().find(|h| h.alias == "pro").unwrap();
    assert!(pro.reachable);
    assert!(pro.status.is_some());
    let offline = hosts.iter().find(|h| h.alias == "offline").unwrap();
    assert!(!offline.reachable);
    assert!(offline.error.is_some());

    // 6. Remote snapshot, batched act, screenshot + unchanged short-circuit over HTTP.
    let snap = match client1
        .call(Some("pro"), RpcMethod::Snapshot(SnapshotParams::default()))
        .await
        .unwrap()
    {
        RpcResult::Snapshot(s) => s,
        _ => unreachable!(),
    };
    assert_eq!(snap.snapshot_id, "s1");

    let act = match client1
        .call(
            Some("pro"),
            RpcMethod::Act(ActParams {
                steps: vec![
                    ActionStep::SetText {
                        ref_id: Some("@e2".into()),
                        text: "remote fleet".into(),
                        replace: true,
                        submit: false,
                        method: TextMethod::Auto,
                    },
                    ActionStep::Click {
                        target: Target::Ref {
                            ref_id: "@e3".into(),
                        },
                        button: MouseButton::Left,
                        count: 1,
                        modifiers: vec![],
                        mode: ClickMode::Auto,
                    },
                ],
                after: PostAction::Snapshot,
                expect_snapshot: Some(snap.snapshot_id),
                focus_mode: FocusMode::Auto,
            }),
        )
        .await
        .unwrap()
    {
        RpcResult::Act(r) => r,
        _ => unreachable!(),
    };
    assert_eq!(act.completed, 2);
    assert!(act.snapshot.is_some());

    let shot1 = match client1
        .call(
            Some("pro"),
            RpcMethod::Screenshot(ScreenshotParams::default()),
        )
        .await
        .unwrap()
    {
        RpcResult::Screenshot(s) => s,
        _ => unreachable!(),
    };
    assert!(!shot1.unchanged);
    assert!(shot1.data_base64.is_some());

    let shot2 = match client1
        .call(
            Some("pro"),
            RpcMethod::Screenshot(ScreenshotParams {
                if_changed_since: Some(shot1.hash),
                ..ScreenshotParams::default()
            }),
        )
        .await
        .unwrap()
    {
        RpcResult::Screenshot(s) => s,
        _ => unreachable!(),
    };
    assert!(shot2.unchanged);
    assert!(shot2.data_base64.is_none());

    // 6b. Verify raw binary /v1/screenshot endpoint directly (zero Base64 on the wire!).
    let bin_url = format!("http://{peer_addr}/v1/screenshot");
    let unauth_bin = http
        .post(&bin_url)
        .json(&json!({ "params": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth_bin.status(), reqwest::StatusCode::UNAUTHORIZED);

    let bin_resp = http
        .post(&bin_url)
        .bearer_auth("super-secret-fleet-token")
        .header("x-desktop-client-id", "99999")
        .json(&json!({ "params": {} }))
        .send()
        .await
        .unwrap();
    // Held by client1's lease -> 422 Unprocessable Entity with LeaseBusy RpcError!
    assert_eq!(bin_resp.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);

    // 7. Second remote client gets LeaseBusy until client1 releases via disconnect_remote.
    let client2 = DesktopClient::new_with_fleet(local_sock.clone(), fleet_path.clone(), "agent-2");
    let busy = client2
        .call(Some("pro"), RpcMethod::Snapshot(SnapshotParams::default()))
        .await
        .unwrap_err();
    assert_eq!(busy.code, ErrorCode::LeaseBusy);

    client1.disconnect_remote("pro").await.unwrap();

    // Now raw binary /v1/screenshot succeeds with raw image bytes + X-Desktop-Meta header.
    let bin_ok = http
        .post(&bin_url)
        .bearer_auth("super-secret-fleet-token")
        .header("x-desktop-client-id", "99999")
        .json(&json!({ "params": {} }))
        .send()
        .await
        .unwrap();
    assert_eq!(bin_ok.status(), reqwest::StatusCode::OK);
    assert_eq!(
        bin_ok
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("image/jpeg")
    );
    let meta_hdr = bin_ok
        .headers()
        .get("x-desktop-meta")
        .and_then(|v| v.to_str().ok())
        .expect("x-desktop-meta header present")
        .to_string();
    let meta_val: serde_json::Value = serde_json::from_str(&meta_hdr).unwrap();
    assert_eq!(meta_val["unchanged"], json!(false));
    let raw_bytes = bin_ok.bytes().await.unwrap();
    assert!(!raw_bytes.is_empty());

    // Release 99999's lease via /v1/disconnect before MCP tests.
    let _ = http
        .post(format!("http://{peer_addr}/v1/disconnect"))
        .bearer_auth("super-secret-fleet-token")
        .header("x-desktop-client-id", "99999")
        .send()
        .await;

    // Now MCP client can talk to "pro" and list all hosts cleanly.
    let mcp = DesktopMcp::new_with_fleet(local_sock, fleet_path);
    let list_blocks = mcp.dispatch("list_hosts", json!({})).await.unwrap();
    let list_txt = format!("{:?}", list_blocks[0]);
    assert!(list_txt.contains("pro"));
    assert!(list_txt.contains("offline"));

    let remote_snap = mcp
        .dispatch("snapshot", json!({ "host": "pro" }))
        .await
        .unwrap();
    assert!(format!("{:?}", remote_snap[0]).contains("@e3 button"));

    let _ = tx.send(true);
    let _ = uds_task.await;
    let _ = http_task.await;
}
