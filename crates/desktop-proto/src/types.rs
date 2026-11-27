use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::rpc::{ErrorCode, RpcError};

/// Screenshot sizing bounds tailored for multimodal vision models:
/// 1568 px long edge and ~1.15 megapixels.
pub const MAX_SCREENSHOT_EDGE: u32 = 1568;
pub const MAX_SCREENSHOT_PIXELS: u64 = 1_150_000;

/// Compute scaled screenshot dimensions preserving aspect ratio within vision-model limits.
#[must_use]
pub fn fit_screenshot(width: u32, height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (1, 1);
    }
    let w = f64::from(width);
    let h = f64::from(height);
    let edge_scale = f64::from(MAX_SCREENSHOT_EDGE) / w.max(h);
    let pixel_scale = (MAX_SCREENSHOT_PIXELS as f64 / (w * h)).sqrt();
    let scale = 1.0_f64.min(edge_scale).min(pixel_scale);
    let out_w = (w * scale).round().max(1.0) as u32;
    let out_h = (h * scale).round().max(1.0) as u32;
    (out_w, out_h)
}

/// Map a pixel coordinate in a captured image back to global display points.
pub fn screenshot_to_global(
    display_bounds: Rect,
    shot_w: u32,
    shot_h: u32,
    x: f64,
    y: f64,
) -> Result<Point, RpcError> {
    if shot_w == 0 || shot_h == 0 {
        return Err(RpcError::new(
            ErrorCode::InvalidInput,
            "screenshot has zero dimensions",
        ));
    }
    let sw = f64::from(shot_w);
    let sh = f64::from(shot_h);
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 || x >= sw || y >= sh {
        return Err(RpcError::with_next(
            ErrorCode::InvalidInput,
            format!("({x}, {y}) is outside the {shot_w}x{shot_h} screenshot"),
            "Give coordinates within [0, width) x [0, height) of the screenshot, or use an @eN ref from snapshot.",
        ));
    }
    Ok(Point {
        x: display_bounds.x + (x * display_bounds.width) / sw,
        y: display_bounds.y + (y * display_bounds.height) / sh,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    #[must_use]
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.y >= self.y && p.x < self.x + self.width && p.y < self.y + self.height
    }

    #[must_use]
    pub fn covers(&self, inner: Rect) -> bool {
        self.x <= inner.x
            && self.y <= inner.y
            && self.x + self.width >= inner.x + inner.width
            && self.y + self.height >= inner.y + inner.height
    }

    #[must_use]
    pub fn center(&self) -> Point {
        Point {
            x: self.x + self.width * 0.5,
            y: self.y + self.height * 0.5,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub index: u32,
    pub id: u32,
    pub main: bool,
    pub bounds: Rect,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub scale: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub running: bool,
}

/// Specificity rank returned by [`AppInfo::match_rank`].
///
/// Higher variants represent tighter matches so lookup helpers such as `find_app` and
/// `list_windows` prefer exact matches over word-prefix matches and general substrings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AppMatchRank {
    /// Case-insensitive substring match against the app name or `.app` bundle stem.
    Substring = 1,
    /// Case-insensitive word-boundary prefix match against the app name or `.app` bundle stem
    /// (e.g. `"T3 Code"` matching `"T3 Code (Beta)"`).
    WordPrefix = 2,
    /// Exact case-insensitive match on app name, `.app` bundle stem, exact bundle ID,
    /// or explicit trailing-dot bundle ID prefix (e.g. `"com.apple."`).
    Exact = 3,
}

fn is_word_prefix(haystack: &str, needle: &str) -> bool {
    let Some(rest) = haystack.strip_prefix(needle) else {
        return false;
    };
    match rest.chars().next() {
        None => true,
        Some(next) => !next.is_alphanumeric(),
    }
}

impl AppInfo {
    #[must_use]
    pub fn canonical_key(&self) -> String {
        self.bundle_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.name)
            .to_ascii_lowercase()
    }

    /// Rank how closely this app matches a caller-supplied lookup query.
    ///
    /// Returns `None` for empty, whitespace-only, bare `.app`, or dot-only queries.
    /// Trailing-dot queries (e.g. `"com.apple."`) match `bundle_id` prefixes only.
    #[must_use]
    pub fn match_rank(&self, query: &str) -> Option<AppMatchRank> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return None;
        }
        if q.ends_with('.') {
            if q.trim_end_matches('.').is_empty() {
                return None;
            }
            return self
                .bundle_id
                .as_deref()
                .is_some_and(|bid| bid.to_lowercase().starts_with(&q))
                .then_some(AppMatchRank::Exact);
        }

        let q_no_ext = q.strip_suffix(".app").unwrap_or(&q).trim();
        if q_no_ext.is_empty() {
            return None;
        }

        let name_lc = self.name.to_lowercase();
        let stem_lc = self
            .path
            .as_deref()
            .and_then(|path| std::path::Path::new(path).file_stem())
            .and_then(|s| s.to_str())
            .map(str::to_lowercase);

        // 1. Exact match: name, bundle_id, or non-localized `.app` bundle stem.
        if name_lc == q || name_lc == q_no_ext {
            return Some(AppMatchRank::Exact);
        }
        if self
            .bundle_id
            .as_deref()
            .is_some_and(|bid| bid.to_lowercase() == q)
        {
            return Some(AppMatchRank::Exact);
        }
        if stem_lc.as_deref() == Some(q_no_ext) {
            return Some(AppMatchRank::Exact);
        }

        // 2. Word-boundary prefix match (e.g. "T3 Code" -> "T3 Code (Beta)").
        if is_word_prefix(&name_lc, q_no_ext)
            || stem_lc
                .as_deref()
                .is_some_and(|stem| is_word_prefix(stem, q_no_ext))
        {
            return Some(AppMatchRank::WordPrefix);
        }

        // 3. Case-insensitive substring match on name or `.app` bundle stem.
        if name_lc.contains(q_no_ext)
            || stem_lc
                .as_deref()
                .is_some_and(|stem| stem.contains(q_no_ext))
        {
            return Some(AppMatchRank::Substring);
        }

        None
    }

    /// Ergonomic lookup matching for `list_windows`, `find_app`, `snapshot(app=...)`, etc.
    ///
    /// Matches exact name, bundle ID, `.app` stem, trailing-dot bundle prefix,
    /// word-prefix, and case-insensitive substring while rejecting empty/whitespace queries.
    #[must_use]
    pub fn matches_query(&self, query: &str) -> bool {
        self.match_rank(query).is_some()
    }

    /// Strict policy/scope matching for `deny_apps`, `confirm_apps`, and `session_start(apps=...)`.
    ///
    /// Only matches exact app name, exact `.app` bundle stem, exact bundle ID, or an explicit
    /// trailing-dot bundle ID prefix (e.g. `"com.apple."`), avoiding unintended substring matches.
    #[must_use]
    pub fn matches_policy_rule(&self, rule: &str) -> bool {
        self.match_rank(rule) == Some(AppMatchRank::Exact)
    }
}

/// Filter and rank windows against an app query.
///
/// If any window matches with [`AppMatchRank::Exact`], only exact matches are returned
/// (preserving original z-order). Otherwise all matching windows are returned ordered by
/// specificity (`WordPrefix` before `Substring`), preserving original z-order within each rank.
#[must_use]
pub fn filter_windows_by_app(windows: Vec<WindowInfo>, query: &str) -> Vec<WindowInfo> {
    let mut ranked: Vec<(AppMatchRank, WindowInfo)> = windows
        .into_iter()
        .filter_map(|w| w.app.match_rank(query).map(|rank| (rank, w)))
        .collect();
    if ranked.iter().any(|(rank, _)| *rank == AppMatchRank::Exact) {
        ranked.retain(|(rank, _)| *rank == AppMatchRank::Exact);
    } else {
        ranked.sort_by(|(a, _), (b, _)| b.cmp(a));
    }
    ranked.into_iter().map(|(_, w)| w).collect()
}

/// Pick the best single window matching an app query, preferring higher [`AppMatchRank`],
/// normal `layer == 0` windows, and earlier z-order on ties.
#[must_use]
pub fn best_matching_window<'a>(windows: &'a [WindowInfo], query: &str) -> Option<&'a WindowInfo> {
    let mut best: Option<(AppMatchRank, bool, &'a WindowInfo)> = None;
    for w in windows {
        let Some(rank) = w.app.match_rank(query) else {
            continue;
        };
        let is_normal_layer = w.layer == 0;
        let better = match best {
            None => true,
            Some((best_rank, best_normal, _)) => (rank, is_normal_layer) > (best_rank, best_normal),
        };
        if better {
            best = Some((rank, is_normal_layer, w));
        }
    }
    best.map(|(_, _, w)| w)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: u32,
    pub title: String,
    pub app: AppInfo,
    pub bounds: Rect,
    pub layer: i32,
    pub alpha: f64,
    pub focused: bool,
    pub minimized: bool,
    pub on_screen: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_index: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusMode {
    /// Try zero-focus background AX/window capture first; fall back to brief focus-and-restore only when needed.
    #[default]
    Auto,
    /// Strictly never focus the app or warp the physical cursor; return `requires_focus` if HID focus is needed.
    Background,
    /// Explicitly activate and raise the target window before acting.
    Foreground,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenState {
    Unlocked,
    DisplayAsleep,
    ScreenLocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionsStatus {
    pub accessibility: bool,
    pub screen_recording: bool,
}

impl PermissionsStatus {
    #[must_use]
    pub fn all_granted(&self) -> bool {
        self.accessibility && self.screen_recording
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    /// Owner-first default: every app is controllable immediately with zero modal dialogs.
    #[default]
    Open,
    /// Require an explicit confirmation dialog for new sessions or listed apps.
    Confirm,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyConfig {
    #[serde(default)]
    pub mode: PolicyMode,
    /// Optional user-configured list of app names or bundle IDs to block. Empty by default.
    #[serde(default)]
    pub deny_apps: Vec<String>,
    /// Optional user-configured list of app names or bundle IDs that require a dialog. Empty by default.
    #[serde(default)]
    pub confirm_apps: Vec<String>,
    /// Optional password-field masking in AX snapshots (`AXSecureTextField`). False by default.
    #[serde(default)]
    pub redact_passwords: bool,
    /// Show the subtle floating `Double-Esc or Stop` status pill while an agent holds an active
    /// control lease. True by default.
    #[serde(default = "default_true")]
    pub overlay: bool,
}

fn default_true() -> bool {
    true
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            mode: PolicyMode::Open,
            deny_apps: Vec::new(),
            confirm_apps: Vec::new(),
            redact_passwords: false,
            overlay: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseInfo {
    pub session_id: String,
    pub owner: String,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStatus {
    pub host: String,
    pub os: String,
    pub version: String,
    pub screen_state: ScreenState,
    pub permissions: PermissionsStatus,
    pub policy_mode: PolicyMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_lease: Option<LeaseInfo>,
    pub displays: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElementAction {
    Press,
    Confirm,
    ShowMenu,
    Raise,
    Focus,
    SetValue,
    Increment,
    Decrement,
}

impl ElementAction {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Press => "press",
            Self::Confirm => "confirm",
            Self::ShowMenu => "show_menu",
            Self::Raise => "raise",
            Self::Focus => "focus",
            Self::SetValue => "set_value",
            Self::Increment => "increment",
            Self::Decrement => "decrement",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiNode {
    pub ref_id: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subrole: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Rect>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub selected: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ElementAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<UiNode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub snapshot_id: String,
    pub app: AppInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_bounds: Option<Rect>,
    pub truncated: bool,
    pub total_nodes: usize,
    pub emitted_nodes: usize,
    pub elapsed_ms: u64,
    pub root: UiNode,
}

/// Rendering options for [`Snapshot::render_compact_with_options`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnapshotRenderOptions {
    /// Emit per-element `[x,y wxh]` pixel bounds (default: `false`; header still includes window bounds).
    pub include_bounds: bool,
}

/// Filter raw element actions down to only non-obvious actionable capabilities for compact output.
///
/// Omits ubiquitous `show_menu` / `raise`, implied `press` on canonical clickable controls,
/// Chromium dummy `press` on structural containers and static text/headings, and implied
/// `set_value` / `focus` on text input roles.
#[must_use]
pub fn filter_render_actions(role: &str, actions: &[ElementAction]) -> Vec<ElementAction> {
    let is_implied_press_role = matches!(
        role,
        "button"
            | "link"
            | "menuitem"
            | "tab"
            | "tabbutton"
            | "checkbox"
            | "radio"
            | "switch"
            | "popup"
            | "combobox"
            | "disclosuretriangle"
            | "segmentedcontrol"
    );
    let is_noisy_container_or_text = matches!(
        role,
        "window"
            | "application"
            | "webarea"
            | "scrollarea"
            | "splitgroup"
            | "toolbar"
            | "menubar"
            | "menu"
            | "tabgroup"
            | "list"
            | "table"
            | "tree"
            | "dialog"
            | "panel"
            | "filler"
            | "unknown"
            | "text"
            | "heading"
    );
    let is_text_input = matches!(
        role,
        "textfield" | "textfield:secure" | "textarea" | "searchfield" | "combobox"
    );

    actions
        .iter()
        .copied()
        .filter(|a| match a {
            ElementAction::ShowMenu | ElementAction::Raise => false,
            ElementAction::Press => !is_implied_press_role && !is_noisy_container_or_text,
            ElementAction::SetValue | ElementAction::Focus => !is_text_input,
            ElementAction::Confirm | ElementAction::Increment | ElementAction::Decrement => true,
        })
        .collect()
}

/// Returns true for roles whose `value` carries editable/control state that must stay explicit
/// even when equal to the control's title or label.
#[must_use]
pub fn is_input_value_role(role: &str) -> bool {
    matches!(
        role,
        "textfield"
            | "textfield:secure"
            | "textarea"
            | "searchfield"
            | "combobox"
            | "slider"
            | "incrementor"
            | "stepper"
    )
}

/// Extract the sanitized display name (`title`, falling back to `label`, capped to 80 chars).
#[must_use]
pub fn rendered_display_name(title: Option<&str>, label: Option<&str>) -> Option<String> {
    title
        .map(|s| sanitize_text(s, 80))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            label
                .map(|s| sanitize_text(s, 80))
                .filter(|s| !s.is_empty())
        })
}

/// Append ` actions=[...]` for any non-obvious visible actions after filtering noise.
pub fn write_visible_actions(out: &mut String, role: &str, actions: &[ElementAction]) {
    let visible = filter_render_actions(role, actions);
    if visible.is_empty() {
        return;
    }
    out.push_str(" actions=[");
    for (i, a) in visible.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(a.as_str());
    }
    out.push(']');
}

impl Snapshot {
    /// Render a compact, token-efficient indented representation of the UI tree.
    #[must_use]
    pub fn render_compact(&self) -> String {
        self.render_compact_with_options(SnapshotRenderOptions::default())
    }

    /// Render a compact representation of the UI tree with explicit display options.
    #[must_use]
    pub fn render_compact_with_options(&self, opts: SnapshotRenderOptions) -> String {
        let mut out = String::with_capacity(1024);
        let _ = write!(
            out,
            "snapshot_id: {} · app: {}",
            self.snapshot_id, self.app.name
        );
        if let Some(bid) = &self.app.bundle_id {
            let _ = write!(out, " ({bid})");
        }
        if let Some(wid) = self.window_id {
            let _ = write!(out, " · window: w{wid}");
        }
        if let Some(title) = &self.window_title
            && !title.is_empty()
        {
            let _ = write!(out, " {:?}", sanitize_text(title, 80));
        }
        if let Some(b) = self.window_bounds {
            let _ = write!(
                out,
                " [{:.0},{:.0} {:.0}x{:.0}]",
                b.x, b.y, b.width, b.height
            );
        }
        if self.truncated {
            let _ = write!(
                out,
                " · truncated ({}/{} nodes)",
                self.emitted_nodes, self.total_nodes
            );
        }
        out.push('\n');
        render_node(&self.root, 0, opts, &mut out);
        out
    }
}

fn render_node(node: &UiNode, depth: usize, opts: SnapshotRenderOptions, out: &mut String) {
    for _ in 0..depth {
        out.push(' ');
    }
    let _ = write!(out, "{} {}", node.ref_id, node.role);
    if let Some(sub) = &node.subrole
        && !sub.is_empty()
    {
        let _ = write!(out, ":{sub}");
    }
    let display_name = rendered_display_name(node.title.as_deref(), node.label.as_deref());
    let sanitized_val = node
        .value
        .as_deref()
        .map(|s| sanitize_text(s, 120))
        .filter(|s| !s.is_empty());

    // Fold plain `text`/`heading` value into positional `"..."` when title/label is absent,
    // unifying macOS (AXValue) and Linux (Name) while keeping `value=` explicit on inputs.
    let folded_text_value = display_name.is_none()
        && matches!(node.role.as_str(), "text" | "heading")
        && sanitized_val.is_some();

    if let Some(name) = &display_name {
        let _ = write!(out, " {name:?}");
    } else if folded_text_value && let Some(val) = &sanitized_val {
        let _ = write!(out, " {val:?}");
    }

    if opts.include_bounds
        && let Some(b) = node.bounds
        && !b.is_empty()
    {
        let _ = write!(
            out,
            " [{:.0},{:.0} {:.0}x{:.0}]",
            b.x, b.y, b.width, b.height
        );
    }
    if !node.enabled {
        out.push_str(" disabled");
    }
    if node.focused {
        out.push_str(" focused");
    }
    if node.selected {
        out.push_str(" selected");
    }
    if !folded_text_value && let Some(val) = &sanitized_val {
        let duplicates_name = !is_input_value_role(&node.role)
            && display_name.as_deref().is_some_and(|n| n == val.as_str());
        if !duplicates_name {
            let _ = write!(out, " value={val:?}");
        }
    }
    write_visible_actions(out, &node.role, &node.actions);
    out.push('\n');
    for child in &node.children {
        render_node(child, depth + 1, opts, out);
    }
}

/// Strip control and bidirectional override characters, collapse whitespace runs, and cap length.
#[must_use]
pub fn sanitize_text(raw: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    let mut pending_space = false;
    for ch in raw.chars() {
        if matches!(
            ch,
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}'
        ) {
            continue;
        }
        if ch == ' ' || ch == '\n' || ch == '\t' || ch == '\r' {
            if !out.is_empty() {
                pending_space = true;
            }
            continue;
        }
        if ch.is_control() {
            continue;
        }
        if pending_space {
            if count >= max_chars {
                out.push('…');
                break;
            }
            out.push(' ');
            count += 1;
            pending_space = false;
        }
        if count >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
        count += 1;
    }
    out
}

/// Cap `read_text` output to `max_chars` Unicode scalar values, appending a truncation note when trimmed.
#[must_use]
pub fn truncate_read_text(text: String, max_chars: usize) -> String {
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return text;
    }
    let split_byte = text
        .char_indices()
        .nth(max_chars)
        .map_or(text.len(), |(idx, _)| idx);
    let dropped = total_chars - max_chars;
    let mut out = String::with_capacity(split_byte + 32);
    out.push_str(&text[..split_byte]);
    let _ = write!(out, "\n... [truncated {dropped} chars]");
    out
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FindMatch {
    pub ref_id: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Rect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ElementAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    #[default]
    Jpeg,
    Png,
}

impl ImageFormat {
    #[must_use]
    pub fn mime_type(&self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScreenshotTarget {
    Display {
        #[serde(default)]
        index: Option<u32>,
    },
    Window {
        window_id: u32,
    },
    App {
        app: String,
    },
    Region {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        #[serde(default)]
        display: Option<u32>,
    },
}

impl Default for ScreenshotTarget {
    fn default() -> Self {
        Self::Display { index: None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenshotResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    pub width: u32,
    pub height: u32,
    pub display_bounds: Rect,
    pub mime_type: String,
    pub hash: String,
    pub unchanged: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotated_refs: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClickMode {
    #[default]
    Auto,
    Semantic,
    Pointer,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextMethod {
    #[default]
    Auto,
    Ax,
    Paste,
    Type,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Target {
    Ref {
        #[serde(rename = "ref")]
        ref_id: String,
    },
    Point {
        x: f64,
        y: f64,
        #[serde(default)]
        display: Option<u32>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ActionStep {
    Click {
        #[serde(flatten)]
        target: Target,
        #[serde(default)]
        button: MouseButton,
        #[serde(default = "default_one_u8")]
        count: u8,
        #[serde(default)]
        modifiers: Vec<String>,
        #[serde(default)]
        mode: ClickMode,
    },
    Hover {
        #[serde(flatten)]
        target: Target,
    },
    Drag {
        from: Target,
        to: Target,
        #[serde(default)]
        modifiers: Vec<String>,
    },
    Scroll {
        #[serde(flatten)]
        target: Target,
        direction: ScrollDirection,
        #[serde(default = "default_three_u32")]
        amount: u32,
    },
    SetText {
        #[serde(default, rename = "ref")]
        ref_id: Option<String>,
        text: String,
        #[serde(default)]
        replace: bool,
        #[serde(default)]
        submit: bool,
        #[serde(default)]
        method: TextMethod,
    },
    Key {
        combo: String,
        #[serde(default = "default_one_u8")]
        repeat: u8,
    },
    OpenApp {
        app: String,
    },
    FocusWindow {
        window_id: u32,
    },
    Menu {
        app: String,
        path: Vec<String>,
    },
    ReadText {
        #[serde(default)]
        mode: ReadTextMode,
        #[serde(default, rename = "ref")]
        ref_id: Option<String>,
        #[serde(default)]
        window_id: Option<u32>,
    },
    Wait {
        until: WaitCondition,
        #[serde(default = "default_wait_ms")]
        timeout_ms: u64,
    },
    Sleep {
        ms: u64,
    },
    SessionEnd,
}

fn default_one_u8() -> u8 {
    1
}
fn default_three_u32() -> u32 {
    3
}
fn default_wait_ms() -> u64 {
    5000
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitCondition {
    Exists {
        query: String,
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        app: Option<String>,
    },
    Gone {
        #[serde(default, rename = "ref")]
        ref_id: Option<String>,
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        app: Option<String>,
    },
    FocusedApp {
        app: String,
    },
    WindowTitleContains {
        text: String,
        #[serde(default)]
        app: Option<String>,
    },
    VisualChange {
        #[serde(default)]
        target: ScreenshotTarget,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostAction {
    #[default]
    None,
    Snapshot,
    Screenshot,
    Annotated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepOutcome {
    pub index: usize,
    pub op: String,
    pub ok: bool,
    pub detail: String,
    pub used_background: bool,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActResult {
    pub completed: usize,
    pub total: usize,
    pub stopped_early: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
    pub duration_ms: u64,
    pub steps: Vec<StepOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Snapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<ScreenshotResult>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub released: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadTextMode {
    #[default]
    Value,
    Selection,
    Clipboard,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadTextResult {
    pub text: String,
    pub source: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_screenshot_preserves_small_and_caps_retina() {
        assert_eq!(fit_screenshot(800, 600), (800, 600));
        let (w, h) = fit_screenshot(2560, 1664);
        assert!(w <= MAX_SCREENSHOT_EDGE);
        assert!(u64::from(w) * u64::from(h) <= MAX_SCREENSHOT_PIXELS + 2000);
        // Aspect ratio preserved closely.
        let orig_ratio = 2560.0 / 1664.0;
        let new_ratio = f64::from(w) / f64::from(h);
        assert!((orig_ratio - new_ratio).abs() < 0.01);
    }

    #[test]
    fn screenshot_to_global_maps_and_rejects_oob() {
        let display = Rect {
            x: 100.0,
            y: 50.0,
            width: 1440.0,
            height: 900.0,
        };
        let p = screenshot_to_global(display, 720, 450, 360.0, 225.0).unwrap();
        assert!((p.x - 820.0).abs() < 1e-6);
        assert!((p.y - 500.0).abs() < 1e-6);

        assert!(screenshot_to_global(display, 720, 450, -1.0, 10.0).is_err());
        assert!(screenshot_to_global(display, 720, 450, 720.0, 10.0).is_err());
        assert!(screenshot_to_global(display, 720, 450, f64::NAN, 10.0).is_err());
    }

    #[test]
    fn sanitize_text_strips_bidi_collapses_whitespace_and_truncates() {
        let dirty = "Hello\u{202E}World\nNext\x07";
        assert_eq!(sanitize_text(dirty, 50), "HelloWorld Next");
        assert_eq!(sanitize_text("  foo  \n\t  bar  ", 50), "foo bar");
        assert_eq!(sanitize_text("abcdef", 4), "abcd…");
    }

    #[test]
    fn truncate_read_text_caps_unicode_chars() {
        assert_eq!(truncate_read_text("short".into(), 10), "short");
        let capped = truncate_read_text("你好世界abc".into(), 4);
        assert!(capped.starts_with("你好世界\n... [truncated 3 chars]"));
    }

    #[test]
    fn app_info_matches_localized_and_english_bundle_stem() {
        let finder = AppInfo {
            name: "访达".into(),
            bundle_id: Some("com.apple.finder".into()),
            pid: Some(500),
            path: Some("/System/Library/CoreServices/Finder.app".into()),
            running: true,
        };
        assert!(finder.matches_query("访达"));
        assert!(finder.matches_query("Finder"));
        assert!(finder.matches_query("Finder.app"));
        assert!(finder.matches_query("com.apple.finder"));
        assert!(finder.matches_query("com.apple."));
        assert!(!finder.matches_query("Safari"));
        assert!(finder.matches_policy_rule("Finder"));
        assert!(finder.matches_policy_rule("com.apple.finder"));
        assert!(finder.matches_policy_rule("com.apple."));
    }

    #[test]
    fn app_info_ranks_exact_prefix_substring_and_keeps_policy_strict() {
        let t3_beta = AppInfo {
            name: "T3 Code (Beta)".into(),
            bundle_id: Some("com.t3tools.t3code".into()),
            pid: Some(42),
            path: Some("/Applications/T3 Code (Beta).app".into()),
            running: true,
        };
        let t3_exact = AppInfo {
            name: "T3 Code".into(),
            bundle_id: Some("com.t3tools.t3code.release".into()),
            pid: Some(43),
            path: Some("/Applications/T3 Code.app".into()),
            running: true,
        };
        let other_code = AppInfo {
            name: "My T3 Code Helper".into(),
            bundle_id: Some("com.example.helper".into()),
            pid: Some(44),
            path: None,
            running: true,
        };

        // Lookup matching: exact > word-prefix > substring.
        assert_eq!(t3_exact.match_rank("T3 Code"), Some(AppMatchRank::Exact));
        assert_eq!(
            t3_beta.match_rank("T3 Code"),
            Some(AppMatchRank::WordPrefix)
        );
        assert_eq!(
            t3_beta.match_rank("t3 code"),
            Some(AppMatchRank::WordPrefix)
        );
        assert_eq!(
            other_code.match_rank("T3 Code"),
            Some(AppMatchRank::Substring)
        );
        assert_eq!(t3_beta.match_rank("Code"), Some(AppMatchRank::Substring));
        assert_eq!(
            t3_beta.match_rank("com.t3tools.t3code"),
            Some(AppMatchRank::Exact)
        );
        assert!(t3_beta.matches_query("T3 Code"));
        assert!(t3_beta.matches_query("Code"));

        // Policy rules stay exact-only (no unintended substring denials or scope leaks).
        assert!(t3_beta.matches_policy_rule("T3 Code (Beta)"));
        assert!(t3_beta.matches_policy_rule("com.t3tools.t3code"));
        assert!(t3_beta.matches_policy_rule("com.t3tools."));
        assert!(!t3_beta.matches_policy_rule("T3 Code"));
        assert!(!t3_beta.matches_policy_rule("Code"));

        // Empty, whitespace, bare `.app`, and dot-only queries must never match.
        for bad in ["", "   ", ".app", "  .app ", ".", "..."] {
            assert_eq!(
                t3_beta.match_rank(bad),
                None,
                "query {bad:?} must not match"
            );
            assert!(!t3_beta.matches_query(bad));
            assert!(!t3_beta.matches_policy_rule(bad));
        }
        // Trailing-dot bundle prefix queries do not fall back to substring name matching.
        assert_eq!(t3_beta.match_rank("com.apple."), None);

        // Window filtering prefers exact over word-prefix over substring.
        let make_win = |id: u32, app: AppInfo, layer: i32| WindowInfo {
            id,
            title: app.name.clone(),
            app,
            bounds: Rect {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
            },
            layer,
            alpha: 1.0,
            focused: false,
            minimized: false,
            on_screen: true,
            display_index: Some(0),
        };

        // When no exact match exists, WordPrefix comes before Substring.
        let no_exact = filter_windows_by_app(
            vec![
                make_win(1, other_code.clone(), 0),
                make_win(2, t3_beta.clone(), 0),
            ],
            "T3 Code",
        );
        assert_eq!(
            no_exact.iter().map(|w| w.id).collect::<Vec<_>>(),
            vec![2, 1]
        );

        // When an exact match exists, exact wins exclusively.
        let with_exact = filter_windows_by_app(
            vec![
                make_win(1, other_code.clone(), 0),
                make_win(2, t3_beta.clone(), 0),
                make_win(3, t3_exact.clone(), 0),
            ],
            "T3 Code",
        );
        assert_eq!(with_exact.iter().map(|w| w.id).collect::<Vec<_>>(), vec![3]);

        // Empty query returns no windows.
        assert!(filter_windows_by_app(vec![make_win(2, t3_beta.clone(), 0)], "   ").is_empty());

        // `best_matching_window` prefers higher rank and normal `layer == 0` windows.
        let wins = vec![
            make_win(10, t3_beta.clone(), 25),
            make_win(11, t3_beta.clone(), 0),
            make_win(12, other_code, 0),
        ];
        assert_eq!(
            best_matching_window(&wins, "T3 Code").map(|w| w.id),
            Some(11)
        );
        assert_eq!(best_matching_window(&wins, ""), None);
    }

    #[test]
    fn snapshot_compact_render_omits_noise_and_supports_opt_in_bounds() {
        let snap = Snapshot {
            snapshot_id: "s1".into(),
            app: AppInfo {
                name: "Notes".into(),
                bundle_id: Some("com.apple.Notes".into()),
                pid: Some(42),
                path: None,
                running: true,
            },
            window_id: Some(7),
            window_title: Some("Ideas".into()),
            window_bounds: Some(Rect {
                x: 10.0,
                y: 20.0,
                width: 800.0,
                height: 600.0,
            }),
            truncated: false,
            total_nodes: 5,
            emitted_nodes: 5,
            elapsed_ms: 12,
            root: UiNode {
                ref_id: "@e1".into(),
                role: "window".into(),
                subrole: None,
                title: Some("Ideas".into()),
                label: None,
                value: None,
                bounds: Some(Rect {
                    x: 10.0,
                    y: 20.0,
                    width: 800.0,
                    height: 600.0,
                }),
                enabled: true,
                focused: false,
                selected: false,
                actions: vec![ElementAction::Raise, ElementAction::ShowMenu],
                children: vec![
                    UiNode {
                        ref_id: "@e2".into(),
                        role: "button".into(),
                        subrole: None,
                        title: Some("New".into()),
                        label: None,
                        value: None,
                        bounds: Some(Rect {
                            x: 20.0,
                            y: 30.0,
                            width: 40.0,
                            height: 24.0,
                        }),
                        enabled: true,
                        focused: false,
                        selected: false,
                        actions: vec![ElementAction::Press, ElementAction::ShowMenu],
                        children: vec![],
                    },
                    UiNode {
                        ref_id: "@e3".into(),
                        role: "text".into(),
                        subrole: None,
                        title: None,
                        label: None,
                        value: Some("  Hello   world  ".into()),
                        bounds: Some(Rect {
                            x: 20.0,
                            y: 60.0,
                            width: 100.0,
                            height: 16.0,
                        }),
                        enabled: true,
                        focused: false,
                        selected: false,
                        actions: vec![ElementAction::Press, ElementAction::ShowMenu],
                        children: vec![],
                    },
                    UiNode {
                        ref_id: "@e4".into(),
                        role: "textfield".into(),
                        subrole: None,
                        title: Some("Search".into()),
                        label: None,
                        value: Some("Search".into()),
                        bounds: None,
                        enabled: true,
                        focused: false,
                        selected: false,
                        actions: vec![ElementAction::SetValue, ElementAction::ShowMenu],
                        children: vec![],
                    },
                    UiNode {
                        ref_id: "@e5".into(),
                        role: "group".into(),
                        subrole: None,
                        title: Some("Card".into()),
                        label: None,
                        value: None,
                        bounds: None,
                        enabled: true,
                        focused: false,
                        selected: false,
                        actions: vec![ElementAction::Press, ElementAction::ShowMenu],
                        children: vec![],
                    },
                ],
            },
        };

        let compact = snap.render_compact();
        assert!(compact.contains("snapshot_id: s1"));
        assert!(compact.contains("[10,20 800x600]")); // header keeps window bounds
        assert!(compact.contains("\n@e1 window \"Ideas\"\n"));
        assert!(compact.contains("\n @e2 button \"New\"\n"));
        assert!(compact.contains("\n @e3 text \"Hello world\"\n"));
        // Input keeps explicit value= even when matching title so caller knows field is non-empty.
        assert!(compact.contains("\n @e4 textfield \"Search\" value=\"Search\"\n"));
        // Non-obvious clickable group keeps actions=[press] (without show_menu).
        assert!(compact.contains("\n @e5 group \"Card\" actions=[press]\n"));
        assert!(!compact.contains("show_menu"));
        assert!(!compact.contains("raise"));
        assert!(!compact.contains("[20,30 40x24]"));

        let with_bounds = snap.render_compact_with_options(SnapshotRenderOptions {
            include_bounds: true,
        });
        assert!(with_bounds.contains("@e2 button \"New\" [20,30 40x24]\n"));
        assert!(with_bounds.contains("@e3 text \"Hello world\" [20,60 100x16]\n"));
    }
}
