use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use desktop_proto::{
    ActParams, FindParams, HostEntryStatus, HostStatus, ListWindowsParams, MenuParams,
    ReadTextParams, RpcMethod, RpcResult, ScreenshotParams, SessionStartParams, SnapshotParams,
    SnapshotRenderOptions, WaitParams, is_input_value_role, rendered_display_name, sanitize_text,
    write_visible_actions,
};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, ServiceExt as _};
use serde_json::{Value, json};

use crate::client::DesktopClient;

const INSTRUCTIONS: &str = "Fast desktop control across machines (`host`, default \"local\"). \
Prefer `snapshot`/`find` (@eN refs) over `screenshot`. Chain `act` steps to cut turns: \
`open_app`+`wait` (`after:\"snapshot\"`), or `click`+`read_text`+`menu` (`release_after:true` or \
trailing `session_end`). `focus_mode`: \"auto\" (default: bg AX + bg `open_app`, brief \
focus-restore for HID), \"background\" (zero-focus), \"foreground\" (activates window).";

#[must_use]
pub fn instructions() -> &'static str {
    INSTRUCTIONS
}

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

        let mut snap_opts = SnapshotRenderOptions::default();

        let method = match name {
            "list_hosts" => {
                let hosts = self.client.list_hosts().await;
                return Ok(vec![ContentBlock::text(render_hosts(&hosts))]);
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
                snap_opts.include_bounds = p.include_bounds;
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

        Ok(render_result_with_options(result, snap_opts))
    }
}

fn parse_args<T: serde::de::DeserializeOwned>(name: &str, args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|err| format!("invalid arguments for {name}: {err}"))
}

fn format_status_line(st: &HostStatus) -> String {
    let screen = match st.screen_state {
        desktop_proto::ScreenState::Unlocked => "unlocked",
        desktop_proto::ScreenState::DisplayAsleep => "display_asleep",
        desktop_proto::ScreenState::ScreenLocked => "screen_locked",
    };
    let policy = match st.policy_mode {
        desktop_proto::PolicyMode::Open => "open",
        desktop_proto::PolicyMode::Confirm => "confirm",
    };
    let ax = if st.permissions.accessibility {
        "ok"
    } else {
        "missing"
    };
    let sc = if st.permissions.screen_recording {
        "ok"
    } else {
        "missing"
    };
    let lease = match &st.active_lease {
        Some(l) => format!("{} ({})", l.owner, l.session_id),
        None => "none".into(),
    };
    format!(
        "{} ({}) v{} · {} · displays={} · ax={} sc={} · policy={} · lease={}",
        st.host, st.os, st.version, screen, st.displays, ax, sc, policy, lease
    )
}

/// Render `list_hosts` concisely (1 line per host, omitting private socket paths on online peers).
#[must_use]
pub fn render_hosts(hosts: &[HostEntryStatus]) -> String {
    if hosts.is_empty() {
        return "no hosts configured".into();
    }
    let mut out = String::new();
    for (i, h) in hosts.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if let Some(st) = &h.status {
            let _ = write!(
                out,
                "{}: online ({}ms) · {}",
                h.alias,
                h.latency_ms,
                format_status_line(st)
            );
        } else {
            let err = sanitize_text(h.error.as_deref().unwrap_or("unreachable"), 120);
            let _ = write!(out, "{}: offline ({}ms) · {}", h.alias, h.latency_ms, err);
        }
    }
    out
}

/// Render an RPC result into compact MCP content blocks using default snapshot options.
#[must_use]
pub fn render_result(result: RpcResult) -> Vec<ContentBlock> {
    render_result_with_options(result, SnapshotRenderOptions::default())
}

/// Render an RPC result into compact MCP content blocks with explicit snapshot rendering options.
#[must_use]
pub fn render_result_with_options(
    result: RpcResult,
    snap_opts: SnapshotRenderOptions,
) -> Vec<ContentBlock> {
    match result {
        RpcResult::Status(st) => vec![ContentBlock::text(format_status_line(&st))],
        RpcResult::Windows { displays, windows } => {
            vec![ContentBlock::text(render_windows(&displays, &windows))]
        }
        RpcResult::Session {
            lease,
            woke_display,
        } => {
            let ttl_mins = (lease.expires_at - lease.acquired_at).num_minutes().max(0);
            let mut line = format!(
                "session: {} · owner={} · ttl={}m",
                lease.session_id, lease.owner, ttl_mins
            );
            if let Some(apps) = &lease.apps
                && !apps.is_empty()
            {
                let _ = write!(line, " · apps=[{}]", apps.join(","));
            }
            if woke_display {
                line.push_str(" · woke_display");
            }
            vec![ContentBlock::text(line)]
        }
        RpcResult::Released { message } => vec![ContentBlock::text(message)],
        RpcResult::Snapshot(snap) => {
            vec![ContentBlock::text(
                snap.render_compact_with_options(snap_opts),
            )]
        }
        RpcResult::Find {
            snapshot_id,
            matches,
        } => vec![ContentBlock::text(render_find(&snapshot_id, &matches))],
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
                let _ = write!(summary, "\nerror: {err}");
            }
            for step in &act.steps {
                let detail = if step.used_background {
                    step.detail
                        .strip_suffix(" (background)")
                        .unwrap_or(&step.detail)
                } else {
                    step.detail.as_str()
                };
                let _ = write!(
                    summary,
                    "\n  [{}] {} ({}{}ms): {}",
                    step.index,
                    step.op,
                    if step.used_background { "bg, " } else { "" },
                    step.duration_ms,
                    detail
                );
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
        RpcResult::ReadText(rt) => {
            let line = if rt.text.is_empty() {
                format!("read_text ({}): (empty)", rt.source)
            } else {
                format!("read_text ({}): {}", rt.source, rt.text)
            };
            vec![ContentBlock::text(line)]
        }
        RpcResult::Menu { detail } => vec![ContentBlock::text(detail)],
        RpcResult::Wait {
            matched,
            elapsed_ms,
            detail,
        } => {
            let state = if matched { "matched" } else { "timed out" };
            vec![ContentBlock::text(format!(
                "wait: {state} in {elapsed_ms}ms ({detail})"
            ))]
        }
        other => vec![ContentBlock::text(
            serde_json::to_string(&other).unwrap_or_else(|_| "{}".into()),
        )],
    }
}

fn render_windows(
    displays: &[desktop_proto::DisplayInfo],
    windows: &[desktop_proto::WindowInfo],
) -> String {
    if displays.is_empty() && windows.is_empty() {
        return "no windows".into();
    }
    let mut out = String::new();
    for d in displays {
        if !out.is_empty() {
            out.push('\n');
        }
        let _ = write!(
            out,
            "d{}{} {:.0}x{:.0} [{:.0},{:.0}] ({}x{}px {:.1}x)",
            d.index,
            if d.main { " main" } else { "" },
            d.bounds.width,
            d.bounds.height,
            d.bounds.x,
            d.bounds.y,
            d.pixel_width,
            d.pixel_height,
            d.scale
        );
    }
    for w in windows {
        if !out.is_empty() {
            out.push('\n');
        }
        let _ = write!(out, "w{} [{}", w.id, w.app.name);
        if let Some(bid) = &w.app.bundle_id
            && !bid.is_empty()
        {
            let _ = write!(out, " ({bid})");
        }
        out.push(']');
        let title = sanitize_text(&w.title, 80);
        if !title.is_empty() {
            let _ = write!(out, " {title:?}");
        }
        let _ = write!(
            out,
            " [{:.0},{:.0} {:.0}x{:.0}]",
            w.bounds.x, w.bounds.y, w.bounds.width, w.bounds.height
        );
        if let Some(di) = w.display_index {
            let _ = write!(out, " d{di}");
        }
        if w.focused {
            out.push_str(" focused");
        }
        if w.minimized {
            out.push_str(" minimized");
        }
        if !w.on_screen {
            out.push_str(" offscreen");
        }
        if w.layer != 0 {
            let _ = write!(out, " layer={}", w.layer);
        }
        if (w.alpha - 1.0).abs() > 0.01 {
            let _ = write!(out, " alpha={:.2}", w.alpha);
        }
    }
    out
}

fn render_find(snapshot_id: &str, matches: &[desktop_proto::FindMatch]) -> String {
    let mut out = format!("snapshot_id: {snapshot_id} · {} match(es)", matches.len());
    for m in matches {
        let _ = write!(out, "\n{} {}", m.ref_id, m.role);
        let name = rendered_display_name(m.title.as_deref(), m.label.as_deref());
        let val = m
            .value
            .as_deref()
            .map(|s| sanitize_text(s, 120))
            .filter(|s| !s.is_empty());
        let folded_text_value =
            name.is_none() && matches!(m.role.as_str(), "text" | "heading") && val.is_some();
        if let Some(n) = &name {
            let _ = write!(out, " {n:?}");
        } else if folded_text_value && let Some(v) = &val {
            let _ = write!(out, " {v:?}");
        }
        if !folded_text_value && let Some(v) = &val {
            let duplicates_name =
                !is_input_value_role(&m.role) && name.as_deref().is_some_and(|n| n == v.as_str());
            if !duplicates_name {
                let _ = write!(out, " value={v:?}");
            }
        }
        write_visible_actions(&mut out, &m.role, &m.actions);
        let compact_path = compact_find_path(&m.path);
        if !compact_path.is_empty() {
            let _ = write!(out, " · path: {compact_path}");
        }
    }
    out
}

fn compact_find_path(path: &[String]) -> String {
    let window_title = path
        .first()
        .and_then(|s| s.strip_prefix("window:"))
        .map(str::trim);
    let filtered: Vec<String> = path
        .iter()
        .filter(|seg| {
            if seg.starts_with("window:") || seg.as_str() == "window" {
                return false;
            }
            if seg.starts_with("webarea:") || seg.as_str() == "webarea" {
                return false;
            }
            if let Some(wt) = window_title
                && seg.strip_prefix("group:") == Some(wt)
            {
                return false;
            }
            true
        })
        .map(|seg| sanitize_text(seg, 40))
        .collect();
    let keep = filtered.len().saturating_sub(3);
    filtered[keep..].join(" > ")
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
        let _ = write!(
            meta,
            " · snapshot_id={sid} · annotated={} refs",
            shot.annotated_refs.len()
        );
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
    let host_prop = json!({ "type": "string" });
    let focus_prop = json!({
        "type": "string",
        "enum": ["auto", "background", "foreground"]
    });
    let target_point_or_ref = json!({
        "type": "object",
        "description": "{ref} or {x,y,display?} (screenshot px)",
        "properties": {
            "ref": { "type": "string" },
            "x": { "type": "number" },
            "y": { "type": "number" },
            "display": { "type": "integer" }
        },
        "additionalProperties": false
    });
    let screenshot_target_schema = json!({
        "type": "object",
        "required": ["kind"],
        "description": "display{index?}|window{window_id}|app{app}|region{x,y,width,height,display?}",
        "properties": {
            "kind": { "type": "string", "enum": ["display", "window", "app", "region"] },
            "index": { "type": "integer" },
            "window_id": { "type": "integer" },
            "app": { "type": "string" },
            "x": { "type": "number" },
            "y": { "type": "number" },
            "width": { "type": "number" },
            "height": { "type": "number" },
            "display": { "type": "integer" }
        },
        "additionalProperties": false
    });
    let wait_condition_schema = json!({
        "type": "object",
        "required": ["kind"],
        "description": "exists{query,role?,app?}|gone{ref?,query?,app?}|focused_app{app}|window_title_contains{text,app?}|visual_change{target?}",
        "properties": {
            "kind": { "type": "string", "enum": ["exists", "gone", "focused_app", "window_title_contains", "visual_change"] },
            "query": { "type": "string" },
            "role": { "type": "string" },
            "ref": { "type": "string" },
            "app": { "type": "string" },
            "text": { "type": "string" },
            "target": { "type": "object", "description": "display|window|app|region (see screenshot.target)" }
        },
        "additionalProperties": false
    });
    let action_step_schema = json!({
        "type": "object",
        "required": ["op"],
        "description": concat!(
            "click{ref|(x,y,display?),button?,count?,modifiers?,mode?:auto|semantic|pointer}|",
            "hover{ref|(x,y,display?)}|",
            "drag{from,to,modifiers?}|",
            "scroll{ref|(x,y,display?),direction,amount?}|",
            "set_text{text,ref?,replace?:false,submit?,method?}|",
            "key{combo,repeat?}|",
            "open_app{app}|",
            "focus_window{window_id}|",
            "menu{app,path}|",
            "read_text{mode?:value|selection|clipboard,ref?,window_id?}|",
            "wait{until,timeout_ms?}|",
            "sleep{ms}|",
            "session_end{}(final)"
        ),
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
                ]
            },
            "ref": { "type": "string" },
            "x": { "type": "number" },
            "y": { "type": "number" },
            "display": { "type": "integer" },
            "button": { "type": "string", "enum": ["left", "right", "middle"] },
            "count": { "type": "integer" },
            "modifiers": { "type": "array", "items": { "type": "string" } },
            "mode": {
                "type": "string",
                "enum": ["auto", "semantic", "pointer", "value", "selection", "clipboard"]
            },
            "from": target_point_or_ref,
            "to": target_point_or_ref,
            "direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
            "amount": { "type": "integer" },
            "text": { "type": "string" },
            "replace": { "type": "boolean" },
            "submit": { "type": "boolean" },
            "method": { "type": "string", "enum": ["auto", "ax", "paste", "type"] },
            "combo": { "type": "string", "description": "e.g. Return, Escape, mod+s" },
            "repeat": { "type": "integer" },
            "app": { "type": "string" },
            "window_id": { "type": "integer" },
            "path": { "type": "array", "items": { "type": "string" } },
            "until": wait_condition_schema,
            "timeout_ms": { "type": "integer" },
            "ms": { "type": "integer" }
        },
        "additionalProperties": false
    });

    vec![
        make_tool(
            "list_hosts",
            "List reachable hosts with status, permissions, and lease.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
        make_tool(
            "status",
            "Check daemon health, permissions, screen lock, policy, and lease.",
            json!({ "type": "object", "properties": { "host": host_prop }, "additionalProperties": false }),
        ),
        make_tool(
            "session_start",
            "Acquire control lease (optional in open mode; wakes sleeping display).",
            json!({
                "type": "object",
                "properties": {
                    "host": host_prop,
                    "reason": { "type": "string" },
                    "apps": { "type": "array", "items": { "type": "string" } },
                    "wait_for_lease_ms": { "type": "integer" }
                },
                "additionalProperties": false
            }),
        ),
        make_tool(
            "session_end",
            "Release active control lease.",
            json!({ "type": "object", "properties": { "host": host_prop }, "additionalProperties": false }),
        ),
        make_tool(
            "list_windows",
            "List displays and visible windows.",
            json!({
                "type": "object",
                "properties": {
                    "host": host_prop,
                    "app": { "type": "string" },
                    "include_displays": { "type": "boolean", "default": true }
                },
                "additionalProperties": false
            }),
        ),
        make_tool(
            "snapshot",
            "Capture compact Accessibility tree with @eN refs (works in background).",
            json!({
                "type": "object",
                "properties": {
                    "host": host_prop,
                    "app": { "type": "string" },
                    "window_id": { "type": "integer" },
                    "root_ref": { "type": "string" },
                    "max_depth": { "type": "integer" },
                    "max_nodes": { "type": "integer" },
                    "focus_mode": focus_prop,
                    "include_bounds": { "type": "boolean", "default": false }
                },
                "additionalProperties": false
            }),
        ),
        make_tool(
            "find",
            "Search Accessibility tree by text/role and return matching @eN refs.",
            json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "host": host_prop,
                    "query": { "type": "string" },
                    "role": { "type": "string" },
                    "app": { "type": "string" },
                    "window_id": { "type": "integer" },
                    "limit": { "type": "integer" }
                },
                "additionalProperties": false
            }),
        ),
        make_tool(
            "screenshot",
            "Capture display, window, app, or region with optional @eN Set-of-Marks.",
            json!({
                "type": "object",
                "properties": {
                    "host": host_prop,
                    "target": screenshot_target_schema,
                    "annotate": { "type": "boolean", "default": false },
                    "format": { "type": "string", "enum": ["jpeg", "png"], "default": "jpeg" },
                    "if_changed_since": { "type": "string" },
                    "focus_mode": focus_prop
                },
                "additionalProperties": false
            }),
        ),
        make_tool(
            "act",
            "Run 1..25 ordered UI steps atomically with optional post-action capture and release_after.",
            json!({
                "type": "object",
                "required": ["steps"],
                "properties": {
                    "host": host_prop,
                    "steps": {
                        "type": "array",
                        "items": action_step_schema
                    },
                    "after": { "type": "string", "enum": ["none", "snapshot", "screenshot", "annotated"], "default": "none" },
                    "expect_snapshot": { "type": "string" },
                    "focus_mode": focus_prop,
                    "release_after": { "type": "boolean", "default": false }
                },
                "additionalProperties": false
            }),
        ),
        make_tool(
            "read_text",
            "Read text from @eN element value, selection, or clipboard.",
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
            "Invoke native menu-bar item by path via Accessibility.",
            json!({
                "type": "object",
                "required": ["app", "path"],
                "properties": {
                    "host": host_prop,
                    "app": { "type": "string" },
                    "path": { "type": "array", "items": { "type": "string" } }
                },
                "additionalProperties": false
            }),
        ),
        make_tool(
            "wait",
            "Wait for a UI condition up to timeout_ms.",
            json!({
                "type": "object",
                "required": ["condition"],
                "properties": {
                    "host": host_prop,
                    "condition": wait_condition_schema,
                    "timeout_ms": { "type": "integer", "default": 5000 }
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

        // Snapshot works directly (auto-lease) and omits child bounds by default.
        let snap_blocks = mcp.dispatch("snapshot", json!({})).await.unwrap();
        assert_eq!(snap_blocks.len(), 1);
        let snap_txt = snap_blocks[0].as_text().unwrap().text.as_str();
        assert!(snap_txt.contains("snapshot_id: s1"));
        assert!(snap_txt.contains("@e3 button \"Save\""));
        assert!(!snap_txt.contains("actions=[press]"));

        // Opt-in include_bounds=true emits element bounds.
        let snap_bounds_blocks = mcp
            .dispatch("snapshot", json!({ "include_bounds": true }))
            .await
            .unwrap();
        let snap_bounds_txt = snap_bounds_blocks[0].as_text().unwrap().text.as_str();
        assert!(snap_bounds_txt.contains("@e3 button \"Save\" ["));

        // Compact list_windows, status, find, read_text omit pretty-JSON bloat.
        let status_blocks = mcp.dispatch("status", json!({})).await.unwrap();
        let status_txt = status_blocks[0].as_text().unwrap().text.as_str();
        assert!(status_txt.contains("mock-mac"));
        assert!(!status_txt.starts_with('{'));

        let lw_blocks = mcp.dispatch("list_windows", json!({})).await.unwrap();
        let lw_txt = lw_blocks[0].as_text().unwrap().text.as_str();
        assert!(lw_txt.contains("w101 [Notes"));
        assert!(!lw_txt.starts_with('{'));

        let find_blocks = mcp
            .dispatch("find", json!({ "query": "Save" }))
            .await
            .unwrap();
        let find_txt = find_blocks[0].as_text().unwrap().text.as_str();
        assert!(find_txt.contains("snapshot_id:"));
        assert!(find_txt.contains("@e3 button \"Save\""));
        assert!(!find_txt.starts_with('{'));

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
        let act_txt = act_blocks[0].as_text().unwrap().text.as_str();
        assert!(act_txt.contains("5/5 steps succeeded"));
        assert!(act_txt.contains("session released"));
        assert!(act_txt.contains("mock-Value-@e2"));
        assert!(act_txt.contains("snapshot_id:"));
        assert!(!act_txt.contains("(background)"));

        // Explicit session_end remains safe & idempotent after release_after.
        let end_blocks = mcp.dispatch("session_end", json!({})).await.unwrap();
        let end_txt = end_blocks[0].as_text().unwrap().text.as_str();
        assert!(end_txt.contains("No matching active session"));
        assert!(!end_txt.starts_with('{'));

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

        let snapshot = all_tools.iter().find(|t| t.name == "snapshot").unwrap();
        let snapshot_schema = Value::Object((*snapshot.input_schema).clone());
        assert_eq!(
            snapshot_schema["properties"]["include_bounds"]["type"],
            "boolean"
        );

        let screenshot = all_tools.iter().find(|t| t.name == "screenshot").unwrap();
        let screenshot_schema = Value::Object((*screenshot.input_schema).clone());
        assert_eq!(
            screenshot_schema["properties"]["target"]["required"],
            json!(["kind"])
        );

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
        assert!(INSTRUCTIONS.len() < 550);

        let total_tools_bytes: usize = all_tools
            .iter()
            .map(|t| serde_json::to_string(t).unwrap().len())
            .sum();
        assert!(
            total_tools_bytes < 7_500,
            "tools/list compact JSON ({total_tools_bytes} B) exceeded 7500 B ceiling"
        );

        // Verify find preserves explicit `value=` on populated input fields matching their title.
        let find_input = render_find(
            "s9",
            &[desktop_proto::FindMatch {
                ref_id: "@e4".into(),
                role: "textfield".into(),
                title: Some("Search".into()),
                label: None,
                value: Some("Search".into()),
                bounds: None,
                actions: vec![desktop_proto::ElementAction::SetValue],
                path: vec!["window:App".into(), "toolbar".into()],
            }],
        );
        assert!(find_input.contains("@e4 textfield \"Search\" value=\"Search\""));
    }
}
