#![forbid(unsafe_code)]

pub mod fleet;
pub mod keys;
pub mod rpc;
pub mod types;

pub const PROTOCOL_VERSION: u32 = 1;

pub use fleet::{
    DEFAULT_CONNECT_TIMEOUT_MS, DEFAULT_REMOTE_PORT, FleetConfig, HostEntryStatus, ListenConfig,
    PeerHostConfig, expand_tilde, is_tailscale_ip, resolve_token, validate_bind_ip,
};

pub use keys::{KeyCode, KeyCombo, Modifiers, parse_key_combo, parse_modifiers};
pub use rpc::{
    ActParams, ErrorCode, FindParams, ListWindowsParams, MenuParams, ReadTextParams, RpcError,
    RpcMethod, RpcRequest, RpcResponse, RpcResult, ScreenshotParams, SessionStartParams,
    SnapshotParams, WaitParams,
};
pub use types::{
    ActResult, ActionStep, AppInfo, ClickMode, DisplayInfo, ElementAction, FindMatch, FocusMode,
    HostStatus, ImageFormat, LeaseInfo, MouseButton, PermissionsStatus, Point, PolicyConfig,
    PolicyMode, PostAction, ReadTextMode, ReadTextResult, Rect, ScreenState, ScreenshotResult,
    ScreenshotTarget, ScrollDirection, Snapshot, StepOutcome, Target, TextMethod, UiNode,
    WaitCondition, WindowInfo, fit_screenshot, screenshot_to_global,
};
