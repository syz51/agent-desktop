use std::collections::BTreeMap;

use anyhow::Result;
use desktop_proto::{
    ActResult, FindParams, FocusMode, ListWindowsParams, RpcMethod, RpcResult, ScreenshotParams,
    ScreenshotTarget, SnapshotParams, SnapshotRenderOptions, StepOutcome,
};
use serde::Serialize;

use crate::client::DesktopClient;
use crate::mcp;

/// Conservative UTF-8 byte-to-token ratio for mixed ASCII/JSON/punctuation MCP output.
pub const BYTES_PER_TOKEN: f64 = 3.6;

#[must_use]
pub fn estimate_tokens(bytes: usize) -> usize {
    ((bytes as f64) / BYTES_PER_TOKEN).ceil() as usize
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSchemaMetric {
    pub name: String,
    pub total_bytes: usize,
    pub description_bytes: usize,
    pub schema_bytes: usize,
    pub est_tokens: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaProfile {
    pub instructions_bytes: usize,
    pub instructions_est_tokens: usize,
    pub tools_total_bytes: usize,
    pub tools_total_est_tokens: usize,
    pub tools: Vec<ToolSchemaMetric>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StageMetric {
    pub stage: &'static str,
    pub operation: String,
    pub bytes: usize,
    pub lines: usize,
    pub est_tokens: usize,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotAnatomy {
    pub total_bytes: usize,
    pub est_tokens: usize,
    pub emitted_nodes: usize,
    pub total_nodes: usize,
    pub truncated: bool,
    pub header_bytes: usize,
    pub indent_bytes: usize,
    pub bounds_bytes: usize,
    pub actions_bytes: usize,
    pub show_menu_lines: usize,
    pub text_nodes: usize,
    pub duplicate_child_text_lines: usize,
    pub roles: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenProfileReport {
    pub host: String,
    pub schema: SchemaProfile,
    pub stages: Vec<StageMetric>,
    pub snapshot_anatomy: Option<SnapshotAnatomy>,
}

/// Measure MCP `initialize` instructions and `tools/list` schema bytes across all tools.
#[must_use]
pub fn profile_schema() -> SchemaProfile {
    let instr = mcp::instructions();
    let instructions_bytes = instr.len();
    let mut tools = Vec::new();
    let mut tools_total_bytes = 0usize;

    for tool in mcp::tools() {
        let total = serde_json::to_string(&tool).map_or(0, |s| s.len());
        let description_bytes = tool.description.as_deref().map_or(0, str::len);
        let schema_bytes = serde_json::to_string(&*tool.input_schema).map_or(0, |s| s.len());
        tools_total_bytes += total;
        tools.push(ToolSchemaMetric {
            name: tool.name.to_string(),
            total_bytes: total,
            description_bytes,
            schema_bytes,
            est_tokens: estimate_tokens(total),
        });
    }
    tools.sort_by_key(|a| std::cmp::Reverse(a.total_bytes));

    SchemaProfile {
        instructions_bytes,
        instructions_est_tokens: estimate_tokens(instructions_bytes),
        tools_total_bytes,
        tools_total_est_tokens: estimate_tokens(tools_total_bytes),
        tools,
    }
}

/// Analyze the byte anatomy of a rendered compact snapshot string.
#[must_use]
pub fn analyze_snapshot_text(
    rendered: &str,
    emitted_nodes: usize,
    total_nodes: usize,
    truncated: bool,
) -> SnapshotAnatomy {
    let total_bytes = rendered.len();
    let mut lines = rendered.lines();
    let header_bytes = lines.next().map_or(0, str::len);

    let mut indent_bytes = 0usize;
    let mut bounds_bytes = 0usize;
    let mut actions_bytes = 0usize;
    let mut show_menu_lines = 0usize;
    let mut text_nodes = 0usize;
    let mut duplicate_child_text_lines = 0usize;
    let mut roles: BTreeMap<String, usize> = BTreeMap::new();
    let mut stack: Vec<(usize, String)> = Vec::new();

    for line in lines {
        let indent = line.bytes().take_while(|&b| b == b' ').count();
        indent_bytes += indent;
        while stack.last().is_some_and(|(d, _)| *d >= indent) {
            stack.pop();
        }

        let trimmed = &line[indent..];
        let mut parts = trimmed.split_whitespace();
        let _ref_id = parts.next();
        let raw_role = parts.next().unwrap_or("");
        let base_role = raw_role.split(':').next().unwrap_or(raw_role);
        if !base_role.is_empty() {
            *roles.entry(base_role.to_string()).or_insert(0) += 1;
        }
        if base_role == "text" {
            text_nodes += 1;
        }

        if let Some(b_start) = find_bounds_span(trimmed) {
            bounds_bytes += b_start;
        }
        if let Some(a_idx) = trimmed.find(" actions=[")
            && let Some(end_rel) = trimmed[a_idx..].find(']')
        {
            let slice = &trimmed[a_idx..=a_idx + end_rel];
            actions_bytes += slice.len();
            if slice.contains("show_menu") {
                show_menu_lines += 1;
            }
        }

        if base_role == "text"
            && let Some(quoted) = extract_first_quoted(trimmed)
            && !quoted.is_empty()
            && let Some((_, parent_line)) = stack.last()
            && extract_first_quoted(parent_line)
                .unwrap_or_else(|| parent_line.clone())
                .to_lowercase()
                .contains(&quoted.to_lowercase())
        {
            duplicate_child_text_lines += 1;
        }

        stack.push((indent, trimmed.to_string()));
    }

    SnapshotAnatomy {
        total_bytes,
        est_tokens: estimate_tokens(total_bytes),
        emitted_nodes,
        total_nodes,
        truncated,
        header_bytes,
        indent_bytes,
        bounds_bytes,
        actions_bytes,
        show_menu_lines,
        text_nodes,
        duplicate_child_text_lines,
        roles,
    }
}

fn find_bounds_span(line: &str) -> Option<usize> {
    let mut search = line;
    while let Some(idx) = search.find(" [") {
        let rest = &search[idx + 2..];
        if let Some(end) = rest.find(']') {
            let inner = &rest[..end];
            if inner.contains(',')
                && inner.contains('x')
                && inner
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b',' | b' ' | b'x'))
            {
                return Some(end + 3);
            }
        }
        search = &search[idx + 2..];
    }
    None
}

fn extract_first_quoted(line: &str) -> Option<String> {
    let start = line.find('"')?;
    let mut escaped = false;
    let mut end = None;
    for (rel_idx, ch) in line[start + 1..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '"' {
            end = Some(start + 1 + rel_idx);
            break;
        }
    }
    let end = end?;
    let literal = &line[start..=end];
    serde_json::from_str::<String>(literal)
        .ok()
        .or_else(|| Some(line[start + 1..end].to_string()))
}

fn blocks_text(blocks: &[rmcp::model::ContentBlock]) -> String {
    let mut out = String::new();
    for b in blocks {
        if let Some(txt) = b.as_text() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&txt.text);
        }
    }
    out
}

fn metric_from_text(
    stage: &'static str,
    operation: impl Into<String>,
    text: &str,
    note: impl Into<String>,
) -> StageMetric {
    let bytes = text.len();
    let lines = if text.is_empty() {
        0
    } else {
        text.lines().count()
    };
    StageMetric {
        stage,
        operation: operation.into(),
        bytes,
        lines,
        est_tokens: estimate_tokens(bytes),
        note: note.into(),
    }
}

/// Build a representative 6-step read-only synthetic `ActResult` (matching the 2-turn Calculator
/// batch) so `desktop profile` measures real `mcp::render_result` action overhead without mutating
/// the desktop or arming the kill-switch.
#[must_use]
pub fn sample_act_result() -> ActResult {
    ActResult {
        completed: 6,
        total: 6,
        stopped_early: false,
        error: None,
        duration_ms: 116,
        steps: vec![
            StepOutcome {
                index: 0,
                op: "click".into(),
                ok: true,
                detail: "pressed @e11 in Calculator via AXPress (background)".into(),
                used_background: true,
                duration_ms: 40,
                text: None,
            },
            StepOutcome {
                index: 1,
                op: "click".into(),
                ok: true,
                detail: "pressed @e22 in Calculator via AXPress (background)".into(),
                used_background: true,
                duration_ms: 14,
                text: None,
            },
            StepOutcome {
                index: 2,
                op: "click".into(),
                ok: true,
                detail: "pressed @e16 in Calculator via AXPress (background)".into(),
                used_background: true,
                duration_ms: 8,
                text: None,
            },
            StepOutcome {
                index: 3,
                op: "click".into(),
                ok: true,
                detail: "pressed @e26 in Calculator via AXPress (background)".into(),
                used_background: true,
                duration_ms: 15,
                text: None,
            },
            StepOutcome {
                index: 4,
                op: "read_text".into(),
                ok: true,
                detail: "read @e6 (value): 12".into(),
                used_background: true,
                duration_ms: 1,
                text: Some("12".into()),
            },
            StepOutcome {
                index: 5,
                op: "menu".into(),
                ok: true,
                detail: "invoked Calculator > Quit Calculator in Calculator".into(),
                used_background: true,
                duration_ms: 37,
                text: None,
            },
        ],
        snapshot: None,
        screenshot: None,
        released: true,
    }
}

/// Run a strictly read-only live stage-by-stage MCP token profile against `host`.
pub async fn run_live_profile(
    client: &DesktopClient,
    host: Option<&str>,
    app: Option<String>,
    query: Option<String>,
    include_screenshot: bool,
) -> Result<TokenProfileReport> {
    let schema = profile_schema();
    let mut stages = Vec::new();
    let mut snapshot_anatomy = None;

    // Stage 1: Discovery
    let hosts = client.list_hosts().await;
    let hosts_txt = mcp::render_hosts(&hosts);
    stages.push(metric_from_text(
        "discovery",
        "list_hosts",
        &hosts_txt,
        format!("{} host(s)", hosts.len()),
    ));

    let status_res = client.call(host, RpcMethod::Status).await?;
    let status_txt = blocks_text(&mcp::render_result(status_res));
    stages.push(metric_from_text(
        "discovery",
        "status",
        &status_txt,
        "1 host",
    ));

    let wins_all = client
        .call(
            host,
            RpcMethod::ListWindows(ListWindowsParams {
                app: None,
                include_displays: true,
            }),
        )
        .await?;
    let win_count = match &wins_all {
        RpcResult::Windows { windows, .. } => windows.len(),
        _ => 0,
    };
    let wins_all_txt = blocks_text(&mcp::render_result(wins_all));
    stages.push(metric_from_text(
        "discovery",
        "list_windows (all)",
        &wins_all_txt,
        format!("{win_count} windows"),
    ));

    if let Some(app_filter) = &app {
        let wins_app = client
            .call(
                host,
                RpcMethod::ListWindows(ListWindowsParams {
                    app: Some(app_filter.clone()),
                    include_displays: false,
                }),
            )
            .await?;
        let app_win_count = match &wins_app {
            RpcResult::Windows { windows, .. } => windows.len(),
            _ => 0,
        };
        let wins_app_txt = blocks_text(&mcp::render_result(wins_app));
        stages.push(metric_from_text(
            "discovery",
            format!("list_windows (app={app_filter})"),
            &wins_app_txt,
            format!("{app_win_count} windows"),
        ));
    }

    // Stage 2: Perception (strictly background/read-only)
    let snap_res = client
        .call(
            host,
            RpcMethod::Snapshot(SnapshotParams {
                app: app.clone(),
                focus_mode: FocusMode::Background,
                ..SnapshotParams::default()
            }),
        )
        .await?;
    if let RpcResult::Snapshot(snap) = snap_res {
        let emitted = snap.emitted_nodes;
        let total = snap.total_nodes;
        let truncated = snap.truncated;
        let snap_txt = snap.render_compact_with_options(SnapshotRenderOptions::default());
        let snap_label = match &app {
            Some(a) => format!("snapshot ({a})"),
            None => "snapshot (focused)".into(),
        };
        stages.push(metric_from_text(
            "perception",
            snap_label,
            &snap_txt,
            format!(
                "{emitted}/{total} nodes{}",
                if truncated { " truncated" } else { "" }
            ),
        ));
        snapshot_anatomy = Some(analyze_snapshot_text(&snap_txt, emitted, total, truncated));
    }

    let find_q = query.unwrap_or_else(|| "New".into());
    let find_res = client
        .call(
            host,
            RpcMethod::Find(FindParams {
                query: find_q.clone(),
                role: None,
                app: app.clone(),
                window_id: None,
                limit: 25,
            }),
        )
        .await?;
    let match_count = match &find_res {
        RpcResult::Find { matches, .. } => matches.len(),
        _ => 0,
    };
    let find_txt = blocks_text(&mcp::render_result(find_res));
    stages.push(metric_from_text(
        "perception",
        format!("find (q={find_q})"),
        &find_txt,
        format!("{match_count} match(es)"),
    ));

    if include_screenshot {
        let target = match &app {
            Some(a) => ScreenshotTarget::App { app: a.clone() },
            None => ScreenshotTarget::Display { index: None },
        };
        let shot_res = client
            .call(
                host,
                RpcMethod::Screenshot(ScreenshotParams {
                    target,
                    annotate: true,
                    focus_mode: FocusMode::Background,
                    ..ScreenshotParams::default()
                }),
            )
            .await?;
        let img_kb = match &shot_res {
            RpcResult::Screenshot(s) => {
                s.data_base64.as_ref().map_or(0, |b| b.len() * 3 / 4 / 1024)
            }
            _ => 0,
        };
        let shot_txt = blocks_text(&mcp::render_result(shot_res));
        stages.push(metric_from_text(
            "perception",
            "screenshot (meta)",
            &shot_txt,
            format!("+{img_kb} KB image"),
        ));
    }

    // Stage 3: Action (synthetic representative batch rendered through real MCP formatter)
    let act_txt = blocks_text(&mcp::render_result(RpcResult::Act(Box::new(
        sample_act_result(),
    ))));
    stages.push(metric_from_text(
        "action",
        "act (6-step sample)",
        &act_txt,
        "read-only synthetic",
    ));

    Ok(TokenProfileReport {
        host: host.unwrap_or("local").to_string(),
        schema,
        stages,
        snapshot_anatomy,
    })
}

/// Format a human-readable stage-by-stage token profile report.
#[must_use]
pub fn render_report_table(report: &TokenProfileReport) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "agent-desktop MCP token profile (host={})\n",
        report.host
    );
    let _ = writeln!(out, "=== Stage 0: MCP Schema & Instructions ===");
    let _ = writeln!(out, "{:<24} {:>8} {:>9}  Notes", "Item", "Bytes", "~Tokens");
    let _ = writeln!(out, "{}", "-".repeat(64));
    let _ = writeln!(
        out,
        "{:<24} {:>8} {:>9}  server instructions",
        "initialize.instructions",
        report.schema.instructions_bytes,
        report.schema.instructions_est_tokens
    );
    let _ = writeln!(
        out,
        "{:<24} {:>8} {:>9}  {} tools total",
        "tools/list (all)",
        report.schema.tools_total_bytes,
        report.schema.tools_total_est_tokens,
        report.schema.tools.len()
    );
    for t in report.schema.tools.iter().take(5) {
        let _ = writeln!(
            out,
            "  {:<22} {:>8} {:>9}  desc={} B, schema={} B",
            t.name, t.total_bytes, t.est_tokens, t.description_bytes, t.schema_bytes
        );
    }

    let _ = writeln!(out, "\n=== Stages 1-3: Tool Outputs ===");
    let _ = writeln!(
        out,
        "{:<11} {:<26} {:>8} {:>6} {:>9}  Notes",
        "Stage", "Operation", "Bytes", "Lines", "~Tokens"
    );
    let _ = writeln!(out, "{}", "-".repeat(78));
    for s in &report.stages {
        let _ = writeln!(
            out,
            "{:<11} {:<26} {:>8} {:>6} {:>9}  {}",
            s.stage, s.operation, s.bytes, s.lines, s.est_tokens, s.note
        );
    }

    if let Some(a) = &report.snapshot_anatomy {
        let pct = |n: usize| {
            if a.total_bytes == 0 {
                0.0
            } else {
                (n as f64) * 100.0 / (a.total_bytes as f64)
            }
        };
        let _ = writeln!(out, "\n=== Snapshot Anatomy ===");
        let _ = writeln!(
            out,
            "total: {} B (~{} tok) · nodes: {}/{}{} · text_nodes={} · dup_child_text={} · show_menu={}",
            a.total_bytes,
            a.est_tokens,
            a.emitted_nodes,
            a.total_nodes,
            if a.truncated { " (truncated)" } else { "" },
            a.text_nodes,
            a.duplicate_child_text_lines,
            a.show_menu_lines
        );
        let _ = writeln!(
            out,
            "breakdown: header={} B ({:.1}%) · indent={} B ({:.1}%) · bounds={} B ({:.1}%) · actions={} B ({:.1}%)",
            a.header_bytes,
            pct(a.header_bytes),
            a.indent_bytes,
            pct(a.indent_bytes),
            a.bounds_bytes,
            pct(a.bounds_bytes),
            a.actions_bytes,
            pct(a.actions_bytes),
        );
        let mut role_pairs: Vec<(&String, &usize)> = a.roles.iter().collect();
        role_pairs.sort_by(|x, y| y.1.cmp(x.1).then_with(|| x.0.cmp(y.0)));
        let top_roles: Vec<String> = role_pairs
            .into_iter()
            .take(8)
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        if !top_roles.is_empty() {
            let _ = writeln!(out, "top roles: {}", top_roles.join(", "));
        }
    }

    out
}
