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

impl AppInfo {
    #[must_use]
    pub fn canonical_key(&self) -> String {
        self.bundle_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.name)
            .to_ascii_lowercase()
    }

    #[must_use]
    pub fn matches_query(&self, query: &str) -> bool {
        let q = query.trim().to_ascii_lowercase();
        if q.is_empty() {
            return false;
        }
        let q_no_ext = q.strip_suffix(".app").unwrap_or(&q);
        if self.name.to_ascii_lowercase() == q || self.name.to_ascii_lowercase() == q_no_ext {
            return true;
        }
        if let Some(bid) = &self.bundle_id {
            let b = bid.to_ascii_lowercase();
            if q.ends_with('.') {
                return b.starts_with(&q);
            }
            if b == q {
                return true;
            }
        }
        // Match the non-localized `.app` bundle stem (e.g. "Finder" when localizedName is "访达").
        if let Some(path) = &self.path
            && let Some(file_name) = std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
            && file_name.to_ascii_lowercase() == q_no_ext
        {
            return true;
        }
        false
    }
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

impl Snapshot {
    /// Render a compact, token-efficient indented representation of the UI tree.
    #[must_use]
    pub fn render_compact(&self) -> String {
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
        render_node(&self.root, 0, &mut out);
        out
    }
}

fn render_node(node: &UiNode, depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    let _ = write!(out, "{} {}", node.ref_id, node.role);
    if let Some(sub) = &node.subrole
        && !sub.is_empty()
    {
        let _ = write!(out, ":{sub}");
    }
    let display_name = node
        .title
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| node.label.as_deref().filter(|s| !s.is_empty()));
    if let Some(name) = display_name {
        let _ = write!(out, " {:?}", sanitize_text(name, 80));
    }
    if let Some(b) = node.bounds
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
    if let Some(val) = &node.value
        && !val.is_empty()
    {
        let _ = write!(out, " value={:?}", sanitize_text(val, 120));
    }
    if !node.actions.is_empty() {
        out.push_str(" actions=[");
        for (i, a) in node.actions.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(a.as_str());
        }
        out.push(']');
    }
    out.push('\n');
    for child in &node.children {
        render_node(child, depth + 1, out);
    }
}

/// Strip control and bidirectional override characters and cap text length.
#[must_use]
pub fn sanitize_text(raw: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    for ch in raw.chars() {
        if count >= max_chars {
            out.push('…');
            break;
        }
        if matches!(
            ch,
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}'
        ) {
            continue;
        }
        if ch.is_control() {
            if ch == '\n' || ch == '\t' || ch == '\r' {
                out.push(' ');
                count += 1;
            }
            continue;
        }
        out.push(ch);
        count += 1;
    }
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
    fn sanitize_text_strips_bidi_and_truncates() {
        let dirty = "Hello\u{202E}World\nNext\x07";
        assert_eq!(sanitize_text(dirty, 50), "HelloWorld Next");
        assert_eq!(sanitize_text("abcdef", 4), "abcd…");
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
    }

    #[test]
    fn snapshot_compact_render_includes_refs_and_actions() {
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
            total_nodes: 2,
            emitted_nodes: 2,
            elapsed_ms: 12,
            root: UiNode {
                ref_id: "@e1".into(),
                role: "window".into(),
                subrole: None,
                title: Some("Ideas".into()),
                label: None,
                value: None,
                bounds: None,
                enabled: true,
                focused: false,
                selected: false,
                actions: vec![],
                children: vec![UiNode {
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
                    actions: vec![ElementAction::Press],
                    children: vec![],
                }],
            },
        };
        let text = snap.render_compact();
        assert!(text.contains("snapshot_id: s1"));
        assert!(text.contains("@e2 button \"New\" [20,30 40x24] actions=[press]"));
    }
}
