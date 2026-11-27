use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use chrono::{DateTime, Utc};

/// UTC timestamp type used by `PlatformBackend` and `Clock`.
pub type Timestamp = DateTime<Utc>;

use desktop_proto::{
    AppInfo, ClickMode, DisplayInfo, ElementAction, FocusMode, ImageFormat, KeyCombo, Modifiers,
    MouseButton, PermissionsStatus, Point, ReadTextMode, ReadTextResult, Rect, RpcError,
    ScreenState, ScreenshotTarget, ScrollDirection, Snapshot, SnapshotParams, TextMethod,
    WindowInfo,
};

#[derive(Debug, Clone)]
pub struct AnnotateBox {
    pub ref_id: String,
    pub bounds: Rect,
}

#[derive(Debug, Clone)]
pub struct RawScreenshot {
    pub width: u32,
    pub height: u32,
    pub display_bounds: Rect,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    /// Optional 8x8 area-averaged luma thumbnail computed from raw pixels before encoding.
    pub visual_sig: Option<[u8; 64]>,
}

#[derive(Debug, Clone)]
pub struct ResolvedRef {
    pub ref_id: String,
    pub app: AppInfo,
    pub bounds: Option<Rect>,
    pub actions: Vec<ElementAction>,
}

#[derive(Debug, Clone)]
pub struct StepExecInfo {
    pub detail: String,
    pub used_background: bool,
}

/// Platform abstraction implemented by `desktop-macos`, future `desktop-linux`, and `MockBackend`.
pub trait PlatformBackend: Send + Sync {
    fn os_name(&self) -> &'static str;
    fn is_macos(&self) -> bool;
    fn host_name(&self) -> String;

    fn permissions(&self) -> PermissionsStatus;
    /// Prompt macOS TCC to register/request Accessibility and Screen Recording for this daemon.
    fn request_permissions(&self) -> PermissionsStatus {
        self.permissions()
    }
    fn screen_state(&self) -> ScreenState;
    /// Wake an unlocked sleeping display. Returns `true` if a wake activity was sent.
    fn wake_display(&self) -> Result<bool, RpcError>;
    /// Hold or release a `PreventUserIdleDisplaySleep` assertion during an active session.
    fn set_display_awake(&self, active: bool);
    /// Invalidate any backend-side warm capture/window caches after a mutating UI action.
    fn invalidate_caches(&self) {}

    /// Show, update, or hide the floating status + Stop pill.
    fn set_overlay(&self, text: Option<&str>);
    /// Notify the backend whether an active mutating/control lease is currently held.
    ///
    /// Backends with global physical kill-switches (e.g. macOS double-Esc monitor) must only
    /// arm the kill-switch while `active` is `true`, so normal user Esc presses when idle or
    /// during read-only observation never latch `stop_flag`.
    fn set_control_active(&self, _active: bool) {}
    /// Show a native Allow/Deny confirmation dialog (only used if user opts into Confirm policy).
    fn confirm_dialog(&self, title: &str, message: &str) -> Result<bool, RpcError>;
    /// Shared in-process kill switch set immediately by physical double-Esc, Stop button, or `desktop stop`.
    fn stop_flag(&self) -> Arc<AtomicBool>;
    /// Optional timestamp recorded when the physical kill-switch latched `stop_flag`.
    /// Consuming this lets the engine measure the explicit-stop cooldown from when the user
    /// actually pressed double-Esc or clicked Stop rather than when a later RPC lazily reaps it.
    fn take_stop_triggered_at(&self) -> Option<Timestamp> {
        None
    }

    fn displays(&self) -> Result<Vec<DisplayInfo>, RpcError>;
    fn windows(&self, app_filter: Option<&str>) -> Result<Vec<WindowInfo>, RpcError>;
    fn focused_app(&self) -> Result<Option<AppInfo>, RpcError>;
    fn app_at_point(&self, point: Point) -> Result<Option<AppInfo>, RpcError>;
    fn find_app(&self, query: &str) -> Result<Option<AppInfo>, RpcError>;
    fn open_app(&self, app: &str, focus_mode: FocusMode) -> Result<AppInfo, RpcError>;
    fn focus_window(&self, window_id: u32) -> Result<WindowInfo, RpcError>;

    fn snapshot(
        &self,
        params: &SnapshotParams,
        redact_passwords: bool,
    ) -> Result<Snapshot, RpcError>;
    fn resolve_ref(&self, ref_id: &str) -> Result<ResolvedRef, RpcError>;

    fn capture(
        &self,
        target: &ScreenshotTarget,
        format: ImageFormat,
        annotate: &[AnnotateBox],
        focus_mode: FocusMode,
    ) -> Result<RawScreenshot, RpcError>;

    fn click_ref(
        &self,
        ref_id: &str,
        button: MouseButton,
        count: u8,
        mods: Modifiers,
        click_mode: ClickMode,
        focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError>;

    fn click_point(
        &self,
        point: Point,
        button: MouseButton,
        count: u8,
        mods: Modifiers,
        focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError>;

    fn hover_point(&self, point: Point) -> Result<StepExecInfo, RpcError>;

    fn drag(
        &self,
        from: Point,
        to: Point,
        mods: Modifiers,
        focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError>;

    fn scroll(
        &self,
        point: Point,
        direction: ScrollDirection,
        amount: u32,
        focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError>;

    fn set_text(
        &self,
        ref_id: Option<&str>,
        text: &str,
        replace: bool,
        submit: bool,
        method: TextMethod,
        focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError>;

    fn key(
        &self,
        combo: KeyCombo,
        repeat: u8,
        focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError>;

    fn menu(&self, app: &str, path: &[String]) -> Result<String, RpcError>;

    fn read_text(
        &self,
        ref_id: Option<&str>,
        window_id: Option<u32>,
        mode: ReadTextMode,
        redact_passwords: bool,
    ) -> Result<ReadTextResult, RpcError>;
}
