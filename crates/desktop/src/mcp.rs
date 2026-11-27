use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use desktop_proto::{
    ActParams, FindParams, ListWindowsParams, MenuParams, ReadTextParams, RpcMethod, RpcResult,
    ScreenshotParams, SessionStartParams, SnapshotParams, WaitParams,
};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, ServiceExt as _};
use serde_json::{Value, json};

use crate::client::DesktopClient;

const INSTRUCTIONS: &str = "Fast Rust desktop control across your machines. Prefer `snapshot` or \
`find` (fast Accessibility tree with @eN refs) over `screenshot` when interacting with standard UI \
controls, and chain multi-step interactions inside one `act` call. Use `focus_mode: \"background\"` \
to operate on unfocused windows without moving the user's cursor, or `\"auto\"` (default) to try \
background AX first and fall back to brief focus-and-restore only when needed. Sessions auto-start \
in open policy mode; call `session_end` when finished to release the control lease early.";

pub struct DesktopMcp {
    client: Arc<DesktopClient>,
}

impl DesktopMcp {
    #[must_use]
    pub fn new(socket_path: PathBuf) -> Self {
        Self::new_with_fleet(socket_path, crate::client::default_fleet_path())
    }

    #[must_use]
    pub fn new_with_fleet(socket_path: PathBuf, fleet_path: PathBuf) -> Self {
        let caller = std::env::var("DESKTOP_CALLER")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "mcp".into());
        Self {
            client: Arc::new(DesktopClient::new_with_fleet(
                socket_path,
                fleet_path,
                caller,
            )),
        }
    }

    pub async fn dispatch(&self, name: &str, mut args: Value) -> Result<Vec<ContentBlock>, String> {
        let host = args
            .as_object_mut()
            .and_then(|m| m.remove("host"))
            .and_then(|v| v.as_str().map(str::to_string));

        let method = match name {
            "list_hosts" => {
                let hosts = self.client.list_hosts().await;
                return Ok(vec![ContentBlock::text(
                    serde_json::to_string_pretty(&json!({ "hosts": hosts }))
                        .unwrap_or_else(|_| "{}".into()),
                )]);
            }
            "status" => RpcMethod::Status,
            "session_start" => {
                let p: SessionStartParams = parse_args(name, args)?;
                RpcMethod::SessionStart(p)
            }
            "session_end" => RpcMethod::SessionEnd,
            "list_windows" => {
                let p: ListWindowsParams = parse_args(name, args)?;
                RpcMethod::ListWindows(p)
            }
            "snapshot" => {
                let p: SnapshotParams = parse_args(name, args)?;
                RpcMethod::Snapshot(p)
            }
            "find" => {
                let p: FindParams = parse_args(name, args)?;
                RpcMethod::Find(p)
            }
            "screenshot" => {
                let p: ScreenshotParams = parse_args(name, args)?;
                RpcMethod::Screenshot(p)
            }
            "act" => {
                let p: ActParams = parse_args(name, args)?;
                RpcMethod::Act(p)
            }
            "read_text" => {
                let p: ReadTextParams = parse_args(name, args)?;
                RpcMethod::ReadText(p)
            }
            "menu" => {
                let p: MenuParams = parse_args(name, args)?;
                RpcMethod::Menu(p)
            }
            "wait" => {
                let p: WaitParams = parse_args(name, args)?;
                RpcMethod::Wait(p)
            }
            other => {
                return Err(format!("unknown tool {other:?}; call tools/list"));
            }
        };

        let result = self
            .client
            .call(host.as_deref(), method)
            .await
            .map_err(|e| e.to_string())?;

        Ok(render_result(result))
    }
}

fn parse_args<T: serde::de::DeserializeOwned>(name: &str, args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|err| format!("invalid arguments for {name}: {err}"))
}

fn render_result(result: RpcResult) -> Vec<ContentBlock> {
    match result {
        RpcResult::Snapshot(snap) => vec![ContentBlock::text(snap.render_compact())],
        RpcResult::Screenshot(shot) => render_screenshot(&shot),
        RpcResult::Act(act) => {
            let mut blocks = Vec::new();
            let mut summary = format!(
                "act: {}/{} steps succeeded in {} ms",
                act.completed, act.total, act.duration_ms
            );
            if act.stopped_early {
                summary.push_str(" (stopped early)");
            }
            if let Some(err) = &act.error {
                summary.push_str(&format!("\nerror: {err}"));
            }
            for step in &act.steps {
                summary.push_str(&format!(
                    "\n  [{}] {} ({}{}ms): {}",
                    step.index,
                    step.op,
                    if step.used_background { "bg, " } else { "" },
                    step.duration_ms,
                    step.detail
                ));
            }
            if let Some(snap) = &act.snapshot {
                summary.push_str("\n\n");
                summary.push_str(&snap.render_compact());
            }
            blocks.push(ContentBlock::text(summary));
            if let Some(shot) = &act.screenshot {
                blocks.extend(render_screenshot(shot));
            }
            blocks
        }
        other => vec![ContentBlock::text(
            serde_json::to_string_pretty(&other).unwrap_or_else(|_| "{}".into()),
        )],
    }
}

fn render_screenshot(shot: &desktop_proto::ScreenshotResult) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    if let Some(b64) = &shot.data_base64 {
        blocks.push(ContentBlock::image(b64.clone(), shot.mime_type.clone()));
    }
    let mut meta = format!(
        "{}x{} {} · hash={} · bounds=[{:.0},{:.0} {:.0}x{:.0}]",
        shot.width,
        shot.height,
        shot.mime_type,
        shot.hash,
        shot.display_bounds.x,
        shot.display_bounds.y,
        shot.display_bounds.width,
        shot.display_bounds.height,
    );
    if shot.unchanged {
        meta.push_str(" · unchanged (omitted image)");
    }
    if let Some(sid) = &shot.snapshot_id {
        meta.push_str(&format!(
            " · snapshot_id={sid} · annotated={} refs",
            shot.annotated_refs.len()
        ));
    }
    blocks.push(ContentBlock::text(meta));
    blocks
}

fn make_tool(name: &'static str, description: &'static str, schema: Value) -> Tool {
    let obj: JsonObject = schema.as_object().cloned().unwrap_or_default();
    Tool::new(
        Cow::Borrowed(name),
        Cow::Borrowed(description),
        Arc::new(obj),
    )
}

#[must_use]
pub fn tools() -> Vec<Tool> {
    let host_prop = json!({
        "type": "string",
        "description": "Target host alias (default: \"local\")."
    });
    let focus_prop = json!({
        "type": "string",
        "enum": ["auto", "background", "foreground"],
        "description": "auto (try background AX first, brief focus-restore fallback), background (strict zero-focus), or foreground."
    });

    vec![
        make_tool(
            "list_hosts",
            "List reachable machines with OS, display/lock state, permissions, and active lease.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
        make_tool(
            "status",
            "Check daemon health, TCC permissions, screen lock/display state, policy mode, and active lease.",
            json!({
                "type": "object",
                "properties": { "host": host_prop },
                "additionalProperties": false
            }),
        ),
        make_tool(
            "session_start",
            "Explicitly acquire the control lease on a host (optional in open mode; auto-wakes sleeping unlocked display).",
            json!({
                "type": "object",
                "properties": {
                    "host": host_prop,
                    "reason": { "type": "string" },
                    "apps": { "type": "array", "items": { "type": "string" }, "description": "Optional app scope; omit for all apps." },
                    "wait_for_lease_ms": { "type": "integer", "minimum": 0, "maximum": 15000 }
                },
                "additionalProperties": false
            }),
        ),
        make_tool(
            "session_end",
            "Release the active control lease and hide the status overlay.",
            json!({
                "type": "object",
                "properties": { "host": host_prop },
                "additionalProperties": false
            }),
        ),
        make_tool(
            "list_windows",
            "List active displays and visible windows without capturing pixels.",
            json!({
                "type": "object",
                "properties": {
                    "host": host_prop,
                    "app": { "type": "string", "description": "Optional app name or bundle ID filter." },
                    "include_displays": { "type": "boolean", "default": true }
                },
                "additionalProperties": false
            }),
        ),
        make_tool(
            "snapshot",
            "Capture a fast, compact Accessibility tree with @eN element refs and bounds (works on unfocused background windows).",
            json!({
                "type": "object",
                "properties": {
                    "host": host_prop,
                    "app": { "type": "string" },
                    "window_id": { "type": "integer" },
                    "root_ref": { "type": "string", "description": "Drill down into an existing @eN subtree." },
                    "max_depth": { "type": "integer", "minimum": 1, "maximum": 32 },
                    "max_nodes": { "type": "integer", "minimum": 1, "maximum": 1000 },
                    "focus_mode": focus_prop
                },
                "additionalProperties": false
            }),
        ),
        make_tool(
            "find",
            "Search the Accessibility tree by text/label/value/role and return matching @eN refs.",
            json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "host": host_prop,
                    "query": { "type": "string" },
                    "role": { "type": "string" },
                    "app": { "type": "string" },
                    "window_id": { "type": "integer" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200 }
                },
                "additionalProperties": false
            }),
        ),
        make_tool(
            "screenshot",
            "Capture a display, background/foreground window, app, or region crop, with optional @eN Set-of-Marks boxes and unchanged short-circuit.",
            json!({
                "type": "object",
                "properties": {
                    "host": host_prop,
                    "target": {
                        "type": "object",
                        "description": "{\"kind\":\"display\",\"index\":0} | {\"kind\":\"window\",\"window_id\":123} | {\"kind\":\"app\",\"app\":\"Notes\"} | {\"kind\":\"region\",\"x\":0,\"y\":0,\"width\":400,\"height\":300}"
                    },
                    "annotate": { "type": "boolean", "default": false, "description": "Draw numbered @eN boxes from the live AX snapshot." },
                    "format": { "type": "string", "enum": ["jpeg", "png"], "default": "jpeg" },
                    "if_changed_since": { "type": "string", "description": "Previous screenshot hash; skips image payload if unchanged." },
                    "focus_mode": focus_prop
                },
                "additionalProperties": false
            }),
        ),
        make_tool(
            "act",
            "Execute 1 to 25 ordered UI steps atomically (click, hover, drag, scroll, set_text, key, open_app, focus_window, menu, wait, sleep) with optional post-action snapshot/screenshot.",
            json!({
                "type": "object",
                "required": ["steps"],
                "properties": {
                    "host": host_prop,
                    "steps": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 25,
                        "items": { "type": "object" },
                        "description": "Ordered ActionStep objects tagged with \"op\": click|hover|drag|scroll|set_text|key|open_app|focus_window|menu|wait|sleep."
                    },
                    "after": { "type": "string", "enum": ["none", "snapshot", "screenshot", "annotated"], "default": "none" },
                    "expect_snapshot": { "type": "string", "description": "Abort if current snapshot_id differs." },
                    "focus_mode": focus_prop
                },
                "additionalProperties": false
            }),
        ),
        make_tool(
            "read_text",
            "Read text directly from an @eN element value, focused selection, or clipboard without OCR.",
            json!({
                "type": "object",
                "properties": {
                    "host": host_prop,
                    "ref": { "type": "string" },
                    "window_id": { "type": "integer" },
                    "mode": { "type": "string", "enum": ["value", "selection", "clipboard"], "default": "value" }
                },
                "additionalProperties": false
            }),
        ),
        make_tool(
            "menu",
            "Invoke a native application menu-bar item directly via Accessibility (background, no cascading menu animation).",
            json!({
                "type": "object",
                "required": ["app", "path"],
                "properties": {
                    "host": host_prop,
                    "app": { "type": "string" },
                    "path": { "type": "array", "minItems": 1, "items": { "type": "string" } }
                },
                "additionalProperties": false
            }),
        ),
        make_tool(
            "wait",
            "Wait for a UI condition (exists, gone, focused_app, window_title_contains, visual_change) up to timeout_ms.",
            json!({
                "type": "object",
                "required": ["condition"],
                "properties": {
                    "host": host_prop,
                    "condition": { "type": "object" },
                    "timeout_ms": { "type": "integer", "minimum": 1, "maximum": 15000, "default": 5000 }
                },
                "additionalProperties": false
            }),
        ),
    ]
}

impl ServerHandler for DesktopMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "agent-desktop",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = Value::Object(request.arguments.unwrap_or_default());
        let result = match self.dispatch(&request.name, args).await {
            Ok(blocks) => CallToolResult::success(blocks),
            Err(err) => CallToolResult::error(vec![ContentBlock::text(format!("error: {err}"))]),
        };
        Ok(result.into())
    }
}

/// Serve the MCP protocol over stdio until EOF.
pub async fn serve_stdio(socket_path: PathBuf, fleet_path: PathBuf) -> Result<()> {
    let handler = DesktopMcp::new_with_fleet(socket_path, fleet_path);
    let client = Arc::clone(&handler.client);
    let running = handler.serve(rmcp::transport::stdio()).await?;
    let res = running.waiting().await;
    client.disconnect_all_remotes().await;
    res?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use desktop_core::mock::MockBackend;
    use desktop_core::{AuditLog, DesktopEngine, PolicyStore, SystemClock};
    use desktop_daemon::server::serve_uds;
    use tempfile::TempDir;
    use tokio::sync::watch;

    use super::*;

    #[tokio::test]
    async fn mcp_end_to_end_over_real_uds_socket() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("desktopd.sock");
        let engine = Arc::new(DesktopEngine::new(
            Arc::new(MockBackend::new()),
            Arc::new(SystemClock),
            PolicyStore::new(tmp.path().join("policy.toml")),
            AuditLog::new(tmp.path()),
        ));
        let (tx, rx) = watch::channel(false);
        let s_sock = sock.clone();
        let server = tokio::spawn(async move { serve_uds(s_sock, engine, rx).await });
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let mcp = DesktopMcp::new(sock);

        // Verify exact 12-tool surface.
        assert_eq!(tools().len(), 12);

        // Snapshot works directly (auto-lease).
        let snap_blocks = mcp.dispatch("snapshot", json!({})).await.unwrap();
        assert_eq!(snap_blocks.len(), 1);
        let snap_txt = format!("{:?}", snap_blocks[0]);
        assert!(snap_txt.contains("snapshot_id: s1"));
        assert!(snap_txt.contains("@e3 button"));

        // Screenshot returns image + metadata, then unchanged omits image.
        let shot_blocks = mcp
            .dispatch("screenshot", json!({ "annotate": true }))
            .await
            .unwrap();
        assert_eq!(shot_blocks.len(), 2);

        // Batched act with post-action snapshot.
        let act_blocks = mcp
            .dispatch(
                "act",
                json!({
                    "steps": [
                        { "op": "set_text", "ref": "@e2", "text": "hello", "replace": true },
                        { "op": "click", "ref": "@e3" }
                    ],
                    "after": "snapshot"
                }),
            )
            .await
            .unwrap();
        let act_txt = format!("{:?}", act_blocks[0]);
        assert!(act_txt.contains("2/2 steps succeeded"));
        assert!(act_txt.contains("snapshot_id:"));

        // Release lease cleanly.
        let end_blocks = mcp.dispatch("session_end", json!({})).await.unwrap();
        assert!(format!("{:?}", end_blocks[0]).contains("released"));

        let _ = tx.send(true);
        let _ = server.await;
    }
}
