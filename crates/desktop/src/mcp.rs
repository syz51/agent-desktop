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
controls. Collapse multi-turn latency by chaining steps inside one `act` call: e.g., chain \
`open_app` + `wait` (with `after: \"snapshot\"`) to launch and inspect an app in one turn, and \
chain `click` + `read_text` + `menu` with `release_after: true` (or a trailing `session_end` step) \
to act, read back values/clipboard, and release the control lease in a single turn. Use \
`focus_mode: \"background\"` to operate on unfocused windows without stealing focus or moving the \
cursor, `\"auto\"` (default) to try background AX first and keep `open_app` in the background \
(falling back to brief focus-and-restore only when HID input is required), or `\"foreground\"` to \
explicitly activate/raise windows. Sessions auto-start in open policy mode; set `release_after: true` \
on your final `act` call or call `session_end` to release the control lease early.";

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
            if act.released {
                summary.push_str(" · session released");
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
        "description": "auto (default: background AX + background open_app, brief focus-restore fallback only when HID input is required), background (strict zero-focus; never pops windows or moves cursor), or foreground (explicitly activates/raises target window)."
    });
    let target_point_or_ref = json!({
        "type": "object",
        "description": "Target element or coordinate: either {\"ref\": \"@eN\"} or {\"x\": <number>, \"y\": <number>, \"display\"?: <integer>}.",
        "properties": {
            "ref": { "type": "string", "description": "Element ref from snapshot/find (e.g. \"@e12\")." },
            "x": { "type": "number", "description": "Horizontal pixel coordinate in the latest screenshot (or display point)." },
            "y": { "type": "number", "description": "Vertical pixel coordinate in the latest screenshot (or display point)." },
            "display": { "type": "integer", "minimum": 0, "description": "Optional display index for point coordinates." }
        },
        "additionalProperties": false
    });
    let screenshot_target_schema = json!({
        "type": "object",
        "description": "Capture target tagged with \"kind\": {\"kind\":\"display\",\"index\"?:0} | {\"kind\":\"window\",\"window_id\":123} | {\"kind\":\"app\",\"app\":\"Notes\"} | {\"kind\":\"region\",\"x\":0,\"y\":0,\"width\":400,\"height\":300,\"display\"?:0}.",
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["display", "window", "app", "region"],
                "description": "Screenshot target kind."
            },
            "index": { "type": "integer", "minimum": 0, "description": "display: optional display index (default: primary display)." },
            "window_id": { "type": "integer", "minimum": 0, "description": "window: window ID from list_windows/snapshot." },
            "app": { "type": "string", "description": "app: application name or bundle ID." },
            "x": { "type": "number", "description": "region: left coordinate." },
            "y": { "type": "number", "description": "region: top coordinate." },
            "width": { "type": "number", "description": "region: positive width." },
            "height": { "type": "number", "description": "region: positive height." },
            "display": { "type": "integer", "minimum": 0, "description": "region: optional display index." }
        },
        "additionalProperties": false
    });
    let wait_condition_schema = json!({
        "type": "object",
        "required": ["kind"],
        "description": "Condition tagged with \"kind\": exists {query, role?, app?} | gone {ref?, query?, app?} | focused_app {app} | window_title_contains {text, app?} | visual_change {target?}.",
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["exists", "gone", "focused_app", "window_title_contains", "visual_change"],
                "description": "Condition kind: exists (AX element appears), gone (ref becomes stale or query disappears), focused_app (frontmost app matches), window_title_contains (any window title contains substring), or visual_change (frame diff on target)."
            },
            "query": { "type": "string", "description": "exists|gone: case-insensitive text substring to match in the AX tree." },
            "role": { "type": "string", "description": "exists: optional AX role filter (e.g. \"button\", \"textfield\")." },
            "ref": { "type": "string", "description": "gone: optional @eN ref to wait until stale/removed." },
            "app": { "type": "string", "description": "exists|gone|focused_app|window_title_contains: app name or bundle ID (required for focused_app, optional filter for others)." },
            "text": { "type": "string", "description": "window_title_contains: case-insensitive substring to match in a window title." },
            "target": screenshot_target_schema
        },
        "additionalProperties": false
    });
    let action_step_schema = json!({
        "type": "object",
        "required": ["op"],
        "description": "ActionStep tagged with \"op\": \
            click {ref | (x, y, display?), button?: left|right|middle, count?: 1..3, modifiers?: string[], mode?: auto|semantic|pointer} | \
            hover {ref | (x, y, display?)} | \
            drag {from: {ref|(x,y,display?)}, to: {ref|(x,y,display?)}, modifiers?: string[]} | \
            scroll {ref | (x, y, display?), direction: up|down|left|right, amount?: 1..100} | \
            set_text {text, ref?, replace?: bool, submit?: bool, method?: auto|ax|paste|type} | \
            key {combo, repeat?: 1..50} | \
            open_app {app} | \
            focus_window {window_id} | \
            menu {app, path: string[]} | \
            read_text {mode?: value|selection|clipboard, ref?, window_id?} | \
            wait {until: WaitCondition, timeout_ms?: 1..15000} | \
            sleep {ms: 0..5000} | \
            session_end {} (must be final step).",
        "properties": {
            "op": {
                "type": "string",
                "enum": [
                    "click",
                    "hover",
                    "drag",
                    "scroll",
                    "set_text",
                    "key",
                    "open_app",
                    "focus_window",
                    "menu",
                    "read_text",
                    "wait",
                    "sleep",
                    "session_end"
                ],
                "description": "Step operation kind."
            },
            "ref": {
                "type": "string",
                "description": "click|hover|scroll|set_text|read_text: target element ref from snapshot/find (e.g. \"@e3\")."
            },
            "x": {
                "type": "number",
                "description": "click|hover|scroll: horizontal coordinate (use instead of ref)."
            },
            "y": {
                "type": "number",
                "description": "click|hover|scroll: vertical coordinate (use instead of ref)."
            },
            "display": {
                "type": "integer",
                "minimum": 0,
                "description": "click|hover|scroll: optional display index when using x/y coordinates."
            },
            "button": {
                "type": "string",
                "enum": ["left", "right", "middle"],
                "description": "click: mouse button (default: \"left\")."
            },
            "count": {
                "type": "integer",
                "minimum": 1,
                "maximum": 3,
                "description": "click: click count 1..=3 (default: 1)."
            },
            "modifiers": {
                "type": "array",
                "items": { "type": "string" },
                "description": "click|drag: optional modifier keys (e.g. [\"shift\"], [\"mod\"], [\"cmd\"], [\"ctrl\"], [\"alt\"])."
            },
            "mode": {
                "type": "string",
                "enum": ["auto", "semantic", "pointer", "value", "selection", "clipboard"],
                "description": "click: auto|semantic|pointer (default: \"auto\"). read_text: value|selection|clipboard (default: \"value\")."
            },
            "from": target_point_or_ref,
            "to": target_point_or_ref,
            "direction": {
                "type": "string",
                "enum": ["up", "down", "left", "right"],
                "description": "scroll: scroll direction (required for scroll)."
            },
            "amount": {
                "type": "integer",
                "minimum": 1,
                "maximum": 100,
                "description": "scroll: scroll notches 1..=100 (default: 3)."
            },
            "text": {
                "type": "string",
                "description": "set_text: text to insert or replace (required for set_text)."
            },
            "replace": {
                "type": "boolean",
                "description": "set_text: replace existing field value instead of appending (default: false)."
            },
            "submit": {
                "type": "boolean",
                "description": "set_text: press Return after entering text (default: false)."
            },
            "method": {
                "type": "string",
                "enum": ["auto", "ax", "paste", "type"],
                "description": "set_text: entry tier — auto (AXSetValue -> paste/type), ax, paste, or type (default: \"auto\")."
            },
            "combo": {
                "type": "string",
                "description": "key: portable key combo such as \"Return\", \"Escape\", \"mod+s\", \"shift+tab\" (required for key)."
            },
            "repeat": {
                "type": "integer",
                "minimum": 1,
                "maximum": 50,
                "description": "key: repeat count 1..=50 (default: 1)."
            },
            "app": {
                "type": "string",
                "description": "open_app|menu: application name or bundle ID (e.g. \"Notes\" or \"com.apple.Notes\"). open_app respects focus_mode (background in auto/background, activates only in foreground)."
            },
            "window_id": {
                "type": "integer",
                "minimum": 0,
                "description": "focus_window|read_text: target window ID (required for focus_window, optional for read_text)."
            },
            "path": {
                "type": "array",
                "minItems": 1,
                "items": { "type": "string" },
                "description": "menu: menu-bar item path, e.g. [\"File\", \"New Note\"] (required for menu)."
            },
            "until": wait_condition_schema,
            "timeout_ms": {
                "type": "integer",
                "minimum": 1,
                "maximum": 15000,
                "description": "wait: timeout in milliseconds 1..=15000 (default: 5000)."
            },
            "ms": {
                "type": "integer",
                "minimum": 0,
                "maximum": 5000,
                "description": "sleep: settle delay in milliseconds 0..=5000 (required for sleep)."
            }
        },
        "additionalProperties": false
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
                    "target": screenshot_target_schema,
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
            "Execute 1 to 25 ordered UI steps atomically in one turn (click, hover, drag, scroll, set_text, key, open_app, focus_window, menu, read_text, wait, sleep, session_end) with optional post-action snapshot/screenshot and release_after.",
            json!({
                "type": "object",
                "required": ["steps"],
                "properties": {
                    "host": host_prop,
                    "steps": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 25,
                        "items": action_step_schema,
                        "description": "Ordered ActionStep objects. Chain open_app + wait, or click + read_text + menu, to minimize turns."
                    },
                    "after": { "type": "string", "enum": ["none", "snapshot", "screenshot", "annotated"], "default": "none", "description": "Optional post-batch perception capture." },
                    "expect_snapshot": { "type": "string", "description": "Abort with stale_ref if current snapshot_id differs." },
                    "focus_mode": focus_prop,
                    "release_after": { "type": "boolean", "default": false, "description": "Release the control lease after the batch (and any `after` capture) completes, avoiding an extra session_end turn." }
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
                    "condition": wait_condition_schema,
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

        // Batched act with post-action snapshot, inline read_text, and release_after.
        let act_blocks = mcp
            .dispatch(
                "act",
                json!({
                    "steps": [
                        { "op": "open_app", "app": "Notes" },
                        { "op": "set_text", "ref": "@e2", "text": "hello", "replace": true },
                        { "op": "click", "ref": "@e3" },
                        { "op": "read_text", "mode": "value", "ref": "@e2" },
                        { "op": "menu", "app": "Notes", "path": ["File", "New Note"] }
                    ],
                    "after": "snapshot",
                    "focus_mode": "background",
                    "release_after": true
                }),
            )
            .await
            .unwrap();
        let act_txt = format!("{:?}", act_blocks[0]);
        assert!(act_txt.contains("5/5 steps succeeded"));
        assert!(act_txt.contains("session released"));
        assert!(act_txt.contains("mock-Value-@e2"));
        assert!(act_txt.contains("snapshot_id:"));

        // Explicit session_end remains safe & idempotent after release_after.
        let end_blocks = mcp.dispatch("session_end", json!({})).await.unwrap();
        assert!(format!("{:?}", end_blocks[0]).contains("No matching active session"));

        let _ = tx.send(true);
        let _ = server.await;
    }

    fn assert_mcp_schema_safe(value: &Value, path: &str) {
        const DISALLOWED_KEYS: &[&str] =
            &["$ref", "$defs", "definitions", "oneOf", "anyOf", "allOf"];
        match value {
            Value::Object(map) => {
                for (k, v) in map {
                    assert!(
                        !DISALLOWED_KEYS.contains(&k.as_str()),
                        "schema at {path}.{k} uses unsupported JSON Schema keyword {k:?}"
                    );
                    assert_mcp_schema_safe(v, &format!("{path}.{k}"));
                }
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    assert_mcp_schema_safe(item, &format!("{path}[{i}]"));
                }
            }
            _ => {}
        }
    }

    #[test]
    fn mcp_tools_schema_documents_every_act_op_and_wait_condition() {
        let all_tools = tools();
        assert_eq!(all_tools.len(), 12);

        for tool in &all_tools {
            let schema_val = Value::Object((*tool.input_schema).clone());
            assert_eq!(
                schema_val["type"], "object",
                "tool {} root must be object",
                tool.name
            );
            assert_mcp_schema_safe(&schema_val, tool.name.as_ref());
        }

        let act = all_tools.iter().find(|t| t.name == "act").unwrap();
        let act_schema = Value::Object((*act.input_schema).clone());
        assert_eq!(act_schema["properties"]["release_after"]["type"], "boolean");

        let step_items = &act_schema["properties"]["steps"]["items"];
        let ops: Vec<&str> = step_items["properties"]["op"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for expected_op in [
            "click",
            "hover",
            "drag",
            "scroll",
            "set_text",
            "key",
            "open_app",
            "focus_window",
            "menu",
            "read_text",
            "wait",
            "sleep",
            "session_end",
        ] {
            assert!(
                ops.contains(&expected_op),
                "missing op {expected_op} in act schema"
            );
            assert!(
                step_items["description"]
                    .as_str()
                    .unwrap()
                    .contains(expected_op),
                "missing op {expected_op} in act step description"
            );
        }

        let step_props = step_items["properties"].as_object().unwrap();
        for expected_field in [
            "op",
            "ref",
            "x",
            "y",
            "display",
            "button",
            "count",
            "modifiers",
            "mode",
            "from",
            "to",
            "direction",
            "amount",
            "text",
            "replace",
            "submit",
            "method",
            "combo",
            "repeat",
            "app",
            "window_id",
            "path",
            "until",
            "timeout_ms",
            "ms",
        ] {
            assert!(
                step_props.contains_key(expected_field),
                "missing ActionStep field {expected_field} in act schema"
            );
        }

        let wait = all_tools.iter().find(|t| t.name == "wait").unwrap();
        let wait_schema = Value::Object((*wait.input_schema).clone());
        for cond_schema in [
            &wait_schema["properties"]["condition"],
            &step_items["properties"]["until"],
        ] {
            let kinds: Vec<&str> = cond_schema["properties"]["kind"]["enum"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            for expected_kind in [
                "exists",
                "gone",
                "focused_app",
                "window_title_contains",
                "visual_change",
            ] {
                assert!(
                    kinds.contains(&expected_kind),
                    "missing WaitCondition kind {expected_kind}"
                );
            }
            let cond_props = cond_schema["properties"].as_object().unwrap();
            for field in ["kind", "query", "role", "ref", "app", "text", "target"] {
                assert!(
                    cond_props.contains_key(field),
                    "missing WaitCondition field {field}"
                );
            }
        }

        assert!(INSTRUCTIONS.contains("open_app"));
        assert!(INSTRUCTIONS.contains("wait"));
        assert!(INSTRUCTIONS.contains("read_text"));
        assert!(INSTRUCTIONS.contains("menu"));
        assert!(INSTRUCTIONS.contains("release_after"));
    }
}
