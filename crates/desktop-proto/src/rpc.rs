use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::{
    ActResult, ActionStep, DisplayInfo, FindMatch, FocusMode, HostStatus, ImageFormat, LeaseInfo,
    PolicyConfig, PostAction, ReadTextMode, ReadTextResult, ScreenshotResult, ScreenshotTarget,
    Snapshot, WaitCondition, WindowInfo,
};

pub const MAX_BATCH_STEPS: usize = 25;
pub const MAX_TEXT_CHARS: usize = 100_000;
pub const MAX_READ_TEXT_CHARS: usize = 16_384;
pub const MAX_SLEEP_MS: u64 = 5_000;
pub const MAX_WAIT_TIMEOUT_MS: u64 = 15_000;
pub const MAX_SNAPSHOT_DEPTH: usize = 32;
pub const MAX_SNAPSHOT_NODES: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidInput,
    NotFound,
    StaleRef,
    PermissionDenied,
    ScreenLocked,
    LeaseBusy,
    StoppedByUser,
    RequiresFocus,
    PolicyDenied,
    Timeout,
    BackendError,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
pub struct RpcError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:?}] {}", self.code, self.message)?;
        if let Some(next) = &self.next {
            write!(f, " Next: {next}")?;
        }
        Ok(())
    }
}

impl RpcError {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            next: None,
        }
    }

    #[must_use]
    pub fn with_next(code: ErrorCode, message: impl Into<String>, next: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            next: Some(next.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcRequest {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    pub method: RpcMethod,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcResponse {
    pub id: u64,
    #[serde(flatten)]
    pub payload: RpcPayload,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ok", rename_all = "snake_case")]
pub enum RpcPayload {
    Ok { result: Box<RpcResult> },
    Err { error: RpcError },
}

impl RpcResponse {
    #[must_use]
    pub fn ok(id: u64, result: RpcResult) -> Self {
        Self {
            id,
            payload: RpcPayload::Ok {
                result: Box::new(result),
            },
        }
    }

    #[must_use]
    pub fn err(id: u64, error: RpcError) -> Self {
        Self {
            id,
            payload: RpcPayload::Err { error },
        }
    }

    pub fn into_result(self) -> Result<RpcResult, RpcError> {
        match self.payload {
            RpcPayload::Ok { result } => Ok(*result),
            RpcPayload::Err { error } => Err(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum RpcMethod {
    Status,
    RequestPermissions,
    ListWindows(ListWindowsParams),
    SessionStart(SessionStartParams),
    SessionEnd,
    Snapshot(SnapshotParams),
    Find(FindParams),
    Screenshot(ScreenshotParams),
    Act(ActParams),
    ReadText(ReadTextParams),
    Menu(MenuParams),
    Wait(WaitParams),
    Stop,
    PolicyGet,
    PolicySet(PolicyConfig),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListWindowsParams {
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default = "default_true")]
    pub include_displays: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStartParams {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub apps: Option<Vec<String>>,
    #[serde(default)]
    pub wait_for_lease_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotParams {
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub window_id: Option<u32>,
    #[serde(default)]
    pub root_ref: Option<String>,
    #[serde(default)]
    pub max_depth: Option<usize>,
    #[serde(default)]
    pub max_nodes: Option<usize>,
    #[serde(default)]
    pub focus_mode: FocusMode,
    /// Include per-element `[x,y wxh]` pixel bounds in compact snapshot text (omitted by default).
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_bounds: bool,
}

impl SnapshotParams {
    pub fn validate(&self) -> Result<(), RpcError> {
        if let Some(d) = self.max_depth
            && (d == 0 || d > MAX_SNAPSHOT_DEPTH)
        {
            return Err(RpcError::new(
                ErrorCode::InvalidInput,
                format!("max_depth must be between 1 and {MAX_SNAPSHOT_DEPTH}"),
            ));
        }
        if let Some(n) = self.max_nodes
            && (n == 0 || n > MAX_SNAPSHOT_NODES)
        {
            return Err(RpcError::new(
                ErrorCode::InvalidInput,
                format!("max_nodes must be between 1 and {MAX_SNAPSHOT_NODES}"),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindParams {
    pub query: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub window_id: Option<u32>,
    #[serde(default = "default_find_limit")]
    pub limit: usize,
}

impl FindParams {
    pub fn validate(&self) -> Result<(), RpcError> {
        if self.query.trim().is_empty() && self.role.as_deref().unwrap_or("").trim().is_empty() {
            return Err(RpcError::with_next(
                ErrorCode::InvalidInput,
                "find requires a non-empty query or role",
                "Pass query (e.g. \"Save\") and/or role (e.g. \"button\").",
            ));
        }
        if self.limit == 0 || self.limit > 200 {
            return Err(RpcError::new(
                ErrorCode::InvalidInput,
                "limit must be between 1 and 200",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScreenshotParams {
    #[serde(default)]
    pub target: ScreenshotTarget,
    #[serde(default)]
    pub annotate: bool,
    #[serde(default)]
    pub format: ImageFormat,
    #[serde(default)]
    pub if_changed_since: Option<String>,
    #[serde(default)]
    pub focus_mode: FocusMode,
}

impl ScreenshotParams {
    pub fn validate(&self) -> Result<(), RpcError> {
        if let ScreenshotTarget::Region {
            x,
            y,
            width,
            height,
            ..
        } = self.target
            && (!x.is_finite()
                || !y.is_finite()
                || !width.is_finite()
                || !height.is_finite()
                || width <= 0.0
                || height <= 0.0)
        {
            return Err(RpcError::new(
                ErrorCode::InvalidInput,
                "region requires finite coordinates and positive width/height",
            ));
        }
        Ok(())
    }
}

fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActParams {
    pub steps: Vec<ActionStep>,
    #[serde(default)]
    pub after: PostAction,
    #[serde(default)]
    pub expect_snapshot: Option<String>,
    #[serde(default)]
    pub focus_mode: FocusMode,
    #[serde(default, skip_serializing_if = "is_false")]
    pub release_after: bool,
}

impl ActParams {
    pub fn validate(&self, is_macos: bool) -> Result<(), RpcError> {
        if self.steps.is_empty() || self.steps.len() > MAX_BATCH_STEPS {
            return Err(RpcError::with_next(
                ErrorCode::InvalidInput,
                format!("steps must contain 1 to {MAX_BATCH_STEPS} operations"),
                "Split larger sequences into multiple act calls.",
            ));
        }
        let total = self.steps.len();
        for (idx, step) in self.steps.iter().enumerate() {
            validate_step(idx, total, step, is_macos)?;
        }
        Ok(())
    }
}

fn validate_step(
    idx: usize,
    total: usize,
    step: &ActionStep,
    is_macos: bool,
) -> Result<(), RpcError> {
    match step {
        ActionStep::Click {
            count, modifiers, ..
        } => {
            if *count == 0 || *count > 3 {
                return Err(RpcError::new(
                    ErrorCode::InvalidInput,
                    format!("step {idx}: click count must be 1..=3"),
                ));
            }
            crate::keys::parse_modifiers(modifiers, is_macos)?;
        }
        ActionStep::Drag { modifiers, .. } => {
            crate::keys::parse_modifiers(modifiers, is_macos)?;
        }
        ActionStep::Scroll { amount, .. } => {
            if *amount == 0 || *amount > 100 {
                return Err(RpcError::new(
                    ErrorCode::InvalidInput,
                    format!("step {idx}: scroll amount must be 1..=100"),
                ));
            }
        }
        ActionStep::SetText { text, .. } => {
            if text.chars().count() > MAX_TEXT_CHARS {
                return Err(RpcError::new(
                    ErrorCode::InvalidInput,
                    format!("step {idx}: text exceeds {MAX_TEXT_CHARS} characters"),
                ));
            }
        }
        ActionStep::Key { combo, repeat } => {
            if *repeat == 0 || *repeat > 50 {
                return Err(RpcError::new(
                    ErrorCode::InvalidInput,
                    format!("step {idx}: key repeat must be 1..=50"),
                ));
            }
            crate::keys::parse_key_combo(combo, is_macos)?;
        }
        ActionStep::OpenApp { app } => {
            if app.trim().is_empty() {
                return Err(RpcError::new(
                    ErrorCode::InvalidInput,
                    format!("step {idx}: open_app requires non-empty app name or bundle id"),
                ));
            }
        }
        ActionStep::Menu { app, path } => {
            if app.trim().is_empty() || path.is_empty() {
                return Err(RpcError::new(
                    ErrorCode::InvalidInput,
                    format!("step {idx}: menu requires non-empty app and path"),
                ));
            }
        }
        ActionStep::Wait { timeout_ms, .. } => {
            if *timeout_ms == 0 || *timeout_ms > MAX_WAIT_TIMEOUT_MS {
                return Err(RpcError::new(
                    ErrorCode::InvalidInput,
                    format!("step {idx}: wait timeout_ms must be 1..={MAX_WAIT_TIMEOUT_MS}"),
                ));
            }
        }
        ActionStep::Sleep { ms } => {
            if *ms > MAX_SLEEP_MS {
                return Err(RpcError::new(
                    ErrorCode::InvalidInput,
                    format!("step {idx}: sleep ms must be 0..={MAX_SLEEP_MS}"),
                ));
            }
        }
        ActionStep::SessionEnd => {
            if idx + 1 != total {
                return Err(RpcError::with_next(
                    ErrorCode::InvalidInput,
                    format!("step {idx}: session_end must be the final step in an act batch"),
                    "Place session_end as the last step or set release_after: true.",
                ));
            }
        }
        ActionStep::Hover { .. } | ActionStep::FocusWindow { .. } | ActionStep::ReadText { .. } => {
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadTextParams {
    #[serde(default, rename = "ref")]
    pub ref_id: Option<String>,
    #[serde(default)]
    pub window_id: Option<u32>,
    #[serde(default)]
    pub mode: ReadTextMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MenuParams {
    pub app: String,
    pub path: Vec<String>,
}

impl MenuParams {
    pub fn validate(&self) -> Result<(), RpcError> {
        if self.app.trim().is_empty() || self.path.is_empty() {
            return Err(RpcError::with_next(
                ErrorCode::InvalidInput,
                "menu requires non-empty app and path",
                "Example: app=\"Notes\", path=[\"File\", \"New Note\"].",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitParams {
    pub condition: WaitCondition,
    #[serde(default = "default_wait_ms")]
    pub timeout_ms: u64,
}

impl WaitParams {
    pub fn validate(&self) -> Result<(), RpcError> {
        if self.timeout_ms == 0 || self.timeout_ms > MAX_WAIT_TIMEOUT_MS {
            return Err(RpcError::new(
                ErrorCode::InvalidInput,
                format!("timeout_ms must be 1..={MAX_WAIT_TIMEOUT_MS}"),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RpcResult {
    Status(HostStatus),
    Windows {
        displays: Vec<DisplayInfo>,
        windows: Vec<WindowInfo>,
    },
    Session {
        lease: LeaseInfo,
        woke_display: bool,
    },
    Released {
        message: String,
    },
    Snapshot(Box<Snapshot>),
    Find {
        snapshot_id: String,
        matches: Vec<FindMatch>,
    },
    Screenshot(Box<ScreenshotResult>),
    Act(Box<ActResult>),
    ReadText(ReadTextResult),
    Menu {
        detail: String,
    },
    Wait {
        matched: bool,
        elapsed_ms: u64,
        detail: String,
    },
    Stopped {
        had_active_session: bool,
    },
    Policy(PolicyConfig),
}

fn default_true() -> bool {
    true
}
fn default_find_limit() -> usize {
    25
}
fn default_wait_ms() -> u64 {
    5000
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ClickMode, MouseButton, Target};

    #[test]
    fn validates_act_bounds() {
        let empty = ActParams {
            steps: vec![],
            after: PostAction::None,
            expect_snapshot: None,
            focus_mode: FocusMode::Auto,
            release_after: false,
        };
        assert!(empty.validate(true).is_err());

        let ok = ActParams {
            steps: vec![ActionStep::Click {
                target: Target::Ref {
                    ref_id: "@e1".into(),
                },
                button: MouseButton::Left,
                count: 1,
                modifiers: vec!["mod".into()],
                mode: ClickMode::Auto,
            }],
            after: PostAction::Snapshot,
            expect_snapshot: None,
            focus_mode: FocusMode::Background,
            release_after: false,
        };
        assert!(ok.validate(true).is_ok());

        let too_long_sleep = ActParams {
            steps: vec![ActionStep::Sleep {
                ms: MAX_SLEEP_MS + 1,
            }],
            after: PostAction::None,
            expect_snapshot: None,
            focus_mode: FocusMode::Auto,
            release_after: false,
        };
        assert!(too_long_sleep.validate(true).is_err());
    }

    #[test]
    fn validates_compound_act_steps_and_v1_backward_compat() {
        // Legacy /v1 ActParams without release_after still deserializes cleanly.
        let legacy_act: ActParams =
            serde_json::from_str(r#"{"steps":[{"op":"open_app","app":"Notes"}]}"#).unwrap();
        assert!(!legacy_act.release_after);
        assert_eq!(legacy_act.focus_mode, FocusMode::Auto);
        assert!(legacy_act.validate(true).is_ok());
        // Default release_after=false is omitted on the wire for older daemons.
        let encoded_legacy = serde_json::to_value(&legacy_act).unwrap();
        assert!(encoded_legacy.get("release_after").is_none());

        // Compound batch: read_text (default + explicit) + session_end at the end.
        let compound: ActParams = serde_json::from_str(
            r#"{
                "steps": [
                    {"op": "read_text"},
                    {"op": "read_text", "mode": "clipboard"},
                    {"op": "read_text", "mode": "selection", "ref": "@e2", "window_id": 42},
                    {"op": "session_end"}
                ],
                "release_after": true
            }"#,
        )
        .unwrap();
        assert!(compound.release_after);
        assert!(compound.validate(true).is_ok());
        let roundtrip: ActParams =
            serde_json::from_str(&serde_json::to_string(&compound).unwrap()).unwrap();
        assert_eq!(roundtrip, compound);

        // session_end must be the final step in a batch.
        let mid_end = ActParams {
            steps: vec![ActionStep::SessionEnd, ActionStep::Sleep { ms: 10 }],
            after: PostAction::None,
            expect_snapshot: None,
            focus_mode: FocusMode::Auto,
            release_after: false,
        };
        let err = mid_end.validate(true).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.next.is_some());

        // Legacy /v1 ActResult & StepOutcome without text/released still deserialize cleanly.
        let legacy_result: ActResult = serde_json::from_str(
            r#"{
                "completed": 1,
                "total": 1,
                "stopped_early": false,
                "duration_ms": 5,
                "steps": [{
                    "index": 0,
                    "op": "click",
                    "ok": true,
                    "detail": "pressed @e1",
                    "used_background": true,
                    "duration_ms": 5
                }]
            }"#,
        )
        .unwrap();
        assert!(!legacy_result.released);
        assert!(legacy_result.steps[0].text.is_none());
        let encoded_res = serde_json::to_value(&legacy_result).unwrap();
        assert!(encoded_res.get("released").is_none());
        assert!(encoded_res["steps"][0].get("text").is_none());

        // Legacy /v1 SnapshotParams without include_bounds still deserializes cleanly,
        // and default false is omitted on the wire for older daemons.
        let legacy_snap: SnapshotParams = serde_json::from_str(r#"{"app":"Notes"}"#).unwrap();
        assert!(!legacy_snap.include_bounds);
        let encoded_snap = serde_json::to_value(&legacy_snap).unwrap();
        assert!(encoded_snap.get("include_bounds").is_none());

        let with_bounds: SnapshotParams =
            serde_json::from_str(r#"{"app":"Notes","include_bounds":true}"#).unwrap();
        assert!(with_bounds.include_bounds);
        assert_eq!(
            serde_json::to_value(&with_bounds).unwrap()["include_bounds"],
            serde_json::Value::Bool(true)
        );
    }
}
