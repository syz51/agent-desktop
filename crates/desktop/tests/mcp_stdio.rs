use std::process::Stdio;
use std::sync::Arc;

use desktop_core::mock::MockBackend;
use desktop_core::{AuditLog, DesktopEngine, PolicyStore, SystemClock};
use desktop_daemon::server::serve_uds;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::Command;
use tokio::sync::watch;

async fn rpc_roundtrip(
    stdin: &mut tokio::process::ChildStdin,
    stdout: &mut BufReader<tokio::process::ChildStdout>,
    msg: Value,
) -> Value {
    let mut line = serde_json::to_vec(&msg).unwrap();
    line.push(b'\n');
    stdin.write_all(&line).await.unwrap();
    stdin.flush().await.unwrap();

    let mut resp = String::new();
    let n = stdout.read_line(&mut resp).await.unwrap();
    assert!(n > 0, "MCP server exited before responding to {msg}");
    serde_json::from_str(&resp).unwrap()
}

#[tokio::test]
async fn compiled_desktop_mcp_binary_full_jsonrpc_flow() {
    let tmp = TempDir::new().unwrap();
    let sock = tmp.path().join("desktopd.sock");

    let engine = Arc::new(DesktopEngine::new(
        Arc::new(MockBackend::new()),
        Arc::new(SystemClock),
        PolicyStore::new(tmp.path().join("policy.toml")),
        AuditLog::new(tmp.path()),
    ));
    let (tx, rx) = watch::channel(false);
    let server_sock = sock.clone();
    let server = tokio::spawn(async move { serve_uds(server_sock, engine, rx).await });
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let bin = env!("CARGO_BIN_EXE_desktop");
    let mut child = Command::new(bin)
        .args(["--socket", sock.to_str().unwrap(), "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn desktop mcp");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    // 1. Initialize handshake.
    let init = rpc_roundtrip(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "integration-test", "version": "0.1" }
            }
        }),
    )
    .await;
    assert_eq!(init["result"]["serverInfo"]["name"], "agent-desktop");

    // 2. Send initialized notification.
    let mut notif = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    }))
    .unwrap();
    notif.push(b'\n');
    stdin.write_all(&notif).await.unwrap();
    stdin.flush().await.unwrap();

    // 3. tools/list returns all 12 tools with rich schemas for act and wait.
    let list = rpc_roundtrip(
        &mut stdin,
        &mut stdout,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 12);

    let act_tool = tools.iter().find(|t| t["name"] == "act").unwrap();
    let act_ops: Vec<&str> = act_tool["inputSchema"]["properties"]["steps"]["items"]["properties"]
        ["op"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(act_ops.len(), 13);
    assert!(act_ops.contains(&"read_text"));
    assert!(act_ops.contains(&"session_end"));
    assert_eq!(
        act_tool["inputSchema"]["properties"]["release_after"]["type"],
        "boolean"
    );

    let wait_tool = tools.iter().find(|t| t["name"] == "wait").unwrap();
    let wait_kinds: Vec<&str> =
        wait_tool["inputSchema"]["properties"]["condition"]["properties"]["kind"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
    assert_eq!(wait_kinds.len(), 5);
    assert!(wait_kinds.contains(&"window_title_contains"));
    assert!(wait_kinds.contains(&"visual_change"));

    // 4. Call status.
    let status = rpc_roundtrip(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "status", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(status["result"]["isError"], Value::Bool(false));
    assert!(
        status["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("mock-mac")
    );

    // 5. Call snapshot + batched act + screenshot + session_end.
    let snap = rpc_roundtrip(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "snapshot", "arguments": {} }
        }),
    )
    .await;
    assert!(
        snap["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("@e3 button \"Save\"")
    );

    let act = rpc_roundtrip(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "act",
                "arguments": {
                    "steps": [
                        { "op": "open_app", "app": "Notes" },
                        { "op": "set_text", "ref": "@e2", "text": "fast rust", "replace": true },
                        { "op": "click", "ref": "@e3" },
                        { "op": "read_text", "mode": "value", "ref": "@e2" },
                        { "op": "menu", "app": "Notes", "path": ["File", "New Note"] }
                    ],
                    "after": "snapshot",
                    "release_after": true
                }
            }
        }),
    )
    .await;
    let act_text = act["result"]["content"][0]["text"].as_str().unwrap();
    assert!(act_text.contains("5/5 steps succeeded"));
    assert!(act_text.contains("mock-Value-@e2"));
    assert!(act_text.contains("session released"));

    let shot = rpc_roundtrip(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": { "name": "screenshot", "arguments": { "annotate": true } }
        }),
    )
    .await;
    assert_eq!(shot["result"]["content"][0]["type"], "image");
    assert_eq!(shot["result"]["content"][0]["mimeType"], "image/jpeg");

    let end = rpc_roundtrip(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "session_end", "arguments": {} }
        }),
    )
    .await;
    assert!(
        end["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("released")
    );

    // Close stdin -> MCP exits cleanly.
    drop(stdin);
    let exit = child.wait().await.unwrap();
    assert!(exit.success());

    let _ = tx.send(true);
    let _ = server.await;
}
