#![forbid(unsafe_code)]

pub mod atspi;
pub mod image_encode;
pub mod session;
pub mod wayland;
pub mod x11;
pub mod xvfb;

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use desktop_core::{AnnotateBox, PlatformBackend, RawScreenshot, ResolvedRef, StepExecInfo};
use desktop_proto::{
    AppInfo, ClickMode, DisplayInfo, ErrorCode, FocusMode, ImageFormat, KeyCode, KeyCombo,
    Modifiers, MouseButton, PermissionsStatus, Point, ReadTextMode, ReadTextResult, Rect, RpcError,
    ScreenState, ScreenshotTarget, ScrollDirection, Snapshot, SnapshotParams, TextMethod,
    WindowInfo,
};

use crate::atspi::{AtSpiSnapshotResult, LinuxRefStore};
use crate::session::{SessionKind, command_exists, detect_session, local_host_name};
use crate::x11::X11Session;
use crate::xvfb::{ManagedXvfb, XvfbConfig};

/// Configuration for starting [`LinuxBackend`].
#[derive(Debug, Clone, Default)]
pub struct LinuxConfig {
    /// Explicit X11 display override (e.g. `":0"` or `":99"`).
    pub display: Option<String>,
    /// Launch a private daemon-owned headless `Xvfb` display (24/7 server/VPS mode).
    pub xvfb: bool,
    /// Virtual screen geometry when `xvfb` is true.
    pub xvfb_config: XvfbConfig,
}

/// Pure-Rust Linux implementation of [`PlatformBackend`].
///
/// Supports:
/// - Interactive X11 sessions (`x11rb` RandR + EWMH + ZPixmap + XTest)
/// - Managed 24/7 headless `Xvfb` sessions (`desktopd --xvfb`, secured with `0600` Xauthority)
/// - Semantic accessibility & zero-focus actions via AT-SPI2 over D-Bus (`zbus`)
/// - Wayland sessions via `grim` / XDG Screenshot portal + `wl-clipboard` / `ydotool` / `wtype`
pub struct LinuxBackend {
    session: SessionKind,
    managed_xvfb: Option<ManagedXvfb>,
    stop_flag: Arc<AtomicBool>,
    refs: LinuxRefStore,
}

impl LinuxBackend {
    /// Connect to an existing X11/Wayland session or launch a managed headless `Xvfb` server.
    pub fn connect(config: LinuxConfig) -> Result<Self, RpcError> {
        let (session, managed_xvfb) = if config.xvfb {
            let xvfb = ManagedXvfb::start(&config.xvfb_config)?;
            let display = xvfb.display().to_string();
            (SessionKind::HeadlessXvfb { display }, Some(xvfb))
        } else if let Some(detected) = detect_session(config.display.as_deref()) {
            (detected, None)
        } else {
            return Err(RpcError::with_next(
                ErrorCode::BackendError,
                "no DISPLAY or WAYLAND_DISPLAY found in environment",
                "Run inside a graphical session, pass --display :0, or start headless with `desktopd --xvfb`.",
            ));
        };

        // Verify X11 connectivity up-front when running in X11 or Xvfb mode.
        if let Some(dpy) = session.x11_display()
            && !session.is_wayland()
        {
            let cookie = managed_xvfb.as_ref().map(|x| x.cookie().as_slice());
            let _ = X11Session::connect(dpy, cookie)?;
        }

        tracing::info!(session = ?session, "initialized LinuxBackend");
        Ok(Self {
            session,
            managed_xvfb,
            stop_flag: Arc::new(AtomicBool::new(false)),
            refs: LinuxRefStore::new(),
        })
    }

    #[must_use]
    pub fn session_kind(&self) -> &SessionKind {
        &self.session
    }

    fn xauth_cookie(&self) -> Option<&[u8]> {
        self.managed_xvfb.as_ref().map(|x| x.cookie().as_slice())
    }

    fn xauth_path(&self) -> Option<&Path> {
        self.managed_xvfb.as_ref().map(ManagedXvfb::xauth_path)
    }

    fn x11(&self) -> Result<(X11Session, &str), RpcError> {
        let dpy = self.session.x11_display().ok_or_else(|| {
            RpcError::with_next(
                ErrorCode::BackendError,
                "no X11/XWayland DISPLAY is active in this Wayland session",
                "Enable XWayland, use AT-SPI2 semantic actions, or run `desktopd --xvfb`.",
            )
        })?;
        let conn = X11Session::connect(dpy, self.xauth_cookie())?;
        Ok((conn, dpy))
    }

    fn launch_env_display(&self) -> Option<&str> {
        self.session.x11_display()
    }
}

impl PlatformBackend for LinuxBackend {
    fn os_name(&self) -> &'static str {
        "linux"
    }

    fn is_macos(&self) -> bool {
        false
    }

    fn host_name(&self) -> String {
        local_host_name()
    }

    fn permissions(&self) -> PermissionsStatus {
        let screen_ok = match &self.session {
            SessionKind::X11 { display } | SessionKind::HeadlessXvfb { display } => {
                X11Session::connect(display, self.xauth_cookie()).is_ok()
            }
            SessionKind::Wayland { .. } => {
                let caps = wayland::WaylandCapabilities::probe();
                caps.has_grim
                    || self.session.x11_display().is_some()
                    || wayland::is_screenshot_portal_available()
            }
        };
        // On Linux there is no TCC gate; accessibility is AT-SPI2 if running, or X11 window fallback.
        let ax_ok = atspi::is_atspi_available() || screen_ok;
        PermissionsStatus {
            accessibility: ax_ok,
            screen_recording: screen_ok,
        }
    }

    fn screen_state(&self) -> ScreenState {
        if let Ok((conn, _)) = self.x11() {
            return conn.screen_state();
        }
        ScreenState::Unlocked
    }

    fn wake_display(&self) -> Result<bool, RpcError> {
        if let Ok((conn, _)) = self.x11() {
            return conn.wake_display();
        }
        Ok(false)
    }

    fn set_display_awake(&self, _active: bool) {
        // Headless Xvfb never sleeps; desktop DPMS keep-awake is handled via wake_display.
    }

    fn set_overlay(&self, _text: Option<&str>) {
        // Headless/Linux daemon does not require a floating overlay window.
    }

    fn confirm_dialog(&self, title: &str, message: &str) -> Result<bool, RpcError> {
        if command_exists("zenity") {
            let mut cmd = Command::new("zenity");
            cmd.args(["--question", "--title", title, "--text", message]);
            if let Some(dpy) = self.launch_env_display() {
                cmd.env("DISPLAY", dpy);
            }
            if let Some(auth) = self.xauth_path() {
                cmd.env("XAUTHORITY", auth);
            }
            let status = cmd.status().map_err(|e| {
                RpcError::new(ErrorCode::BackendError, format!("zenity failed: {e}"))
            })?;
            return Ok(status.success());
        }
        Err(RpcError::with_next(
            ErrorCode::BackendError,
            "interactive confirm dialog requires zenity on Linux",
            "Install zenity or keep policy mode set to Open (the owner-first default).",
        ))
    }

    fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop_flag)
    }

    fn displays(&self) -> Result<Vec<DisplayInfo>, RpcError> {
        if let Ok((conn, _)) = self.x11() {
            return conn.displays();
        }
        // Pure Wayland fallback: report logical primary display until portal/output query runs.
        Ok(vec![DisplayInfo {
            index: 0,
            id: 0,
            bounds: Rect {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            },
            pixel_width: 1920,
            pixel_height: 1080,
            scale: 1.0,
            main: true,
        }])
    }

    fn windows(&self, app_filter: Option<&str>) -> Result<Vec<WindowInfo>, RpcError> {
        if let Ok((conn, _)) = self.x11() {
            return conn.windows(app_filter);
        }
        Ok(Vec::new())
    }

    fn focused_app(&self) -> Result<Option<AppInfo>, RpcError> {
        if let Ok((conn, _)) = self.x11() {
            return Ok(conn.focused_window()?.map(|w| w.app));
        }
        Ok(None)
    }

    fn app_at_point(&self, point: Point) -> Result<Option<AppInfo>, RpcError> {
        let wins = self.windows(None)?;
        Ok(wins
            .into_iter()
            .find(|w| w.bounds.contains(point))
            .map(|w| w.app))
    }

    fn find_app(&self, query: &str) -> Result<Option<AppInfo>, RpcError> {
        let wins = self.windows(Some(query))?;
        if let Some(w) = wins.into_iter().next() {
            return Ok(Some(w.app));
        }
        let bin = query.trim();
        if !bin.is_empty() && command_exists(bin) {
            return Ok(Some(AppInfo {
                name: bin.to_string(),
                bundle_id: Some(bin.to_string()),
                pid: None,
                path: Some(bin.to_string()),
                running: false,
            }));
        }
        Ok(None)
    }

    fn open_app(&self, app: &str, focus_mode: FocusMode) -> Result<AppInfo, RpcError> {
        let trimmed = app.trim();
        if trimmed.is_empty() {
            return Err(RpcError::new(
                ErrorCode::InvalidInput,
                "app name cannot be empty",
            ));
        }
        // If a window for this app already exists, only raise/focus it in Foreground mode.
        if let Ok(wins) = self.windows(Some(trimmed))
            && let Some(first) = wins.first()
        {
            if focus_mode == FocusMode::Foreground {
                let _ = self.focus_window(first.id);
            }
            return Ok(first.app.clone());
        }

        if focus_mode == FocusMode::Background
            && !matches!(self.session, SessionKind::HeadlessXvfb { .. })
        {
            return Err(RpcError::with_next(
                ErrorCode::RequiresFocus,
                format!(
                    "launching new app {trimmed:?} on an interactive Linux session may steal foreground focus"
                ),
                "Use focus_mode=\"auto\" or \"foreground\", or run desktopd with --xvfb.",
            ));
        }

        let mut cmd = if command_exists(trimmed) {
            Command::new(trimmed)
        } else if command_exists("gtk-launch") {
            let mut c = Command::new("gtk-launch");
            c.arg(trimmed);
            c
        } else {
            return Err(RpcError::with_next(
                ErrorCode::NotFound,
                format!("application executable {trimmed:?} not found on PATH"),
                "Pass a binary name on PATH (e.g. firefox, chromium, xterm) or full path.",
            ));
        };

        if let Some(dpy) = self.launch_env_display() {
            cmd.env("DISPLAY", dpy);
        }
        if let Some(auth) = self.xauth_path() {
            cmd.env("XAUTHORITY", auth);
        }
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                RpcError::new(
                    ErrorCode::BackendError,
                    format!("failed to launch {trimmed:?}: {e}"),
                )
            })?;

        let pid = child.id();
        std::thread::sleep(Duration::from_millis(120));
        Ok(AppInfo {
            name: trimmed.to_string(),
            bundle_id: Some(trimmed.to_string()),
            pid: Some(pid),
            path: None,
            running: true,
        })
    }

    fn focus_window(&self, window_id: u32) -> Result<WindowInfo, RpcError> {
        let (conn, _) = self.x11()?;
        conn.focus_window(window_id)
    }

    fn snapshot(
        &self,
        params: &SnapshotParams,
        redact_passwords: bool,
    ) -> Result<Snapshot, RpcError> {
        let wins = self.windows(None).unwrap_or_default();
        let fallback_win = params
            .window_id
            .and_then(|wid| wins.iter().find(|w| w.id == wid))
            .or_else(|| {
                params
                    .app
                    .as_deref()
                    .and_then(|q| wins.iter().find(|w| w.app.matches_query(q)))
            })
            .or_else(|| wins.iter().find(|w| w.focused))
            .or_else(|| wins.first());

        // Propagate real caller errors (`StaleRef`, explicit app `NotFound`); only fall back when
        // AT-SPI2 itself is unavailable (e.g. minimal Xvfb container without at-spi2-core).
        match atspi::take_atspi_snapshot(&self.refs, params, fallback_win, redact_passwords)? {
            AtSpiSnapshotResult::Snapshot(snap) => Ok(*snap),
            AtSpiSnapshotResult::Unavailable => {
                Ok(atspi::fallback_window_snapshot(&self.refs, &wins, params))
            }
        }
    }

    fn resolve_ref(&self, ref_id: &str) -> Result<ResolvedRef, RpcError> {
        self.refs.resolve(ref_id)
    }

    fn capture(
        &self,
        target: &ScreenshotTarget,
        format: ImageFormat,
        annotate: &[AnnotateBox],
        focus_mode: FocusMode,
    ) -> Result<RawScreenshot, RpcError> {
        if self.session.is_wayland() {
            return wayland::capture_wayland(target, format, annotate);
        }
        let (conn, _) = self.x11()?;
        conn.capture(target, format, annotate, focus_mode)
    }

    fn click_ref(
        &self,
        ref_id: &str,
        button: MouseButton,
        count: u8,
        mods: Modifiers,
        click_mode: ClickMode,
        focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError> {
        let entry = self.refs.get(ref_id)?;
        let try_semantic = click_mode != ClickMode::Pointer
            && button == MouseButton::Left
            && count <= 1
            && mods.is_empty()
            && focus_mode != FocusMode::Foreground
            && !entry.bus_name.is_empty();

        if try_semantic && matches!(atspi::do_action(&entry), Ok(true)) {
            return Ok(StepExecInfo {
                detail: format!("AT-SPI DoAction on {ref_id}"),
                used_background: true,
            });
        }

        if click_mode == ClickMode::Semantic || focus_mode == FocusMode::Background {
            return Err(RpcError::with_next(
                ErrorCode::RequiresFocus,
                format!("element {ref_id} does not support background AT-SPI Action"),
                "Use focus_mode=\"auto\" or click_mode=\"pointer\" to click its bounds.",
            ));
        }

        let bounds = entry.bounds.ok_or_else(|| {
            RpcError::with_next(
                ErrorCode::NotFound,
                format!("element {ref_id} has no bounding box to click"),
                "Take a screenshot and click explicit coordinates or pick a bounded element.",
            )
        })?;
        self.click_point(bounds.center(), button, count, mods, focus_mode)
    }

    fn click_point(
        &self,
        point: Point,
        button: MouseButton,
        count: u8,
        mods: Modifiers,
        _focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError> {
        if self.session.is_wayland() && self.session.x11_display().is_none() {
            return wayland::wayland_click_point(point, button, count);
        }
        let (conn, _) = self.x11()?;
        conn.click_point(&self.stop_flag, point, button, count, mods)
    }

    fn hover_point(&self, point: Point) -> Result<StepExecInfo, RpcError> {
        if self.session.is_wayland() && self.session.x11_display().is_none() {
            return wayland::wayland_hover_point(point);
        }
        let (conn, _) = self.x11()?;
        conn.hover_point(&self.stop_flag, point)
    }

    fn drag(
        &self,
        from: Point,
        to: Point,
        mods: Modifiers,
        _focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError> {
        let (conn, _) = self.x11()?;
        conn.drag(&self.stop_flag, from, to, mods)
    }

    fn scroll(
        &self,
        point: Point,
        direction: ScrollDirection,
        amount: u32,
        _focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError> {
        if self.session.is_wayland() && self.session.x11_display().is_none() {
            return wayland::wayland_scroll(point, direction, amount);
        }
        let (conn, _) = self.x11()?;
        conn.scroll(&self.stop_flag, point, direction, amount)
    }

    fn set_text(
        &self,
        ref_id: Option<&str>,
        text: &str,
        replace: bool,
        submit: bool,
        method: TextMethod,
        focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError> {
        // Tier 1: Background AT-SPI2 EditableText.SetTextContents (zero focus steal!).
        if let Some(rid) = ref_id
            && matches!(method, TextMethod::Auto | TextMethod::Ax)
        {
            let entry = self.refs.get(rid)?;
            if !entry.bus_name.is_empty()
                && !submit
                && replace
                && matches!(atspi::set_editable_text(&entry, text), Ok(true))
            {
                return Ok(StepExecInfo {
                    detail: format!(
                        "AT-SPI SetTextContents on {rid} ({} chars)",
                        text.chars().count()
                    ),
                    used_background: true,
                });
            }
            if focus_mode == FocusMode::Background || method == TextMethod::Ax {
                return Err(RpcError::with_next(
                    ErrorCode::RequiresFocus,
                    format!("background AT-SPI text entry unavailable for {rid}"),
                    "Use focus_mode=\"auto\" or method=\"auto\" to fall back to clipboard paste / XTest.",
                ));
            }
            if let Some(bounds) = entry.bounds {
                let _ = self.click_point(
                    bounds.center(),
                    MouseButton::Left,
                    1,
                    Modifiers::default(),
                    FocusMode::Foreground,
                )?;
                std::thread::sleep(Duration::from_millis(30));
            }
        }

        if self.session.is_wayland() && self.session.x11_display().is_none() {
            if replace {
                let _ = wayland::wayland_key_combo(
                    KeyCombo {
                        key: KeyCode::A,
                        modifiers: Modifiers {
                            ctrl: true,
                            ..Modifiers::default()
                        },
                    },
                    1,
                );
            }
            let info = wayland::wayland_type_text(text)?;
            if submit {
                let _ = wayland::wayland_key_combo(
                    KeyCombo {
                        key: KeyCode::Return,
                        modifiers: Modifiers::default(),
                    },
                    1,
                );
            }
            return Ok(info);
        }

        let (conn, dpy) = self.x11()?;
        if replace {
            let _ = conn.press_key_combo(
                &self.stop_flag,
                KeyCombo {
                    key: KeyCode::A,
                    modifiers: Modifiers {
                        ctrl: true,
                        ..Modifiers::default()
                    },
                },
                1,
            );
            std::thread::sleep(Duration::from_millis(15));
        }

        let xauth = self.xauth_path();
        let is_short_ascii = text.len() <= 16 && text.is_ascii();
        let detail = match method {
            TextMethod::Type => {
                let n = conn.type_text(&self.stop_flag, text)?;
                format!("xtest typed {n} chars")
            }
            TextMethod::Paste => {
                conn.paste_text_via_clipboard(&self.stop_flag, dpy, xauth, text)?;
                format!("clipboard pasted {} chars", text.chars().count())
            }
            TextMethod::Auto | TextMethod::Ax => {
                if is_short_ascii {
                    let n = conn.type_text(&self.stop_flag, text)?;
                    format!("xtest typed {n} chars")
                } else if conn
                    .paste_text_via_clipboard(&self.stop_flag, dpy, xauth, text)
                    .is_ok()
                {
                    format!("clipboard pasted {} chars", text.chars().count())
                } else {
                    let n = conn.type_text(&self.stop_flag, text)?;
                    format!("xtest typed {n} chars")
                }
            }
        };

        if submit {
            let _ = conn.press_key_combo(
                &self.stop_flag,
                KeyCombo {
                    key: KeyCode::Return,
                    modifiers: Modifiers::default(),
                },
                1,
            )?;
        }

        Ok(StepExecInfo {
            detail,
            used_background: false,
        })
    }

    fn key(
        &self,
        combo: KeyCombo,
        repeat: u8,
        _focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError> {
        if self.session.is_wayland() && self.session.x11_display().is_none() {
            return wayland::wayland_key_combo(combo, repeat);
        }
        let (conn, _) = self.x11()?;
        conn.press_key_combo(&self.stop_flag, combo, repeat)
    }

    fn menu(&self, app: &str, path: &[String]) -> Result<String, RpcError> {
        atspi::invoke_menu(app, path)
    }

    fn read_text(
        &self,
        ref_id: Option<&str>,
        _window_id: Option<u32>,
        mode: ReadTextMode,
        redact_passwords: bool,
    ) -> Result<ReadTextResult, RpcError> {
        if mode == ReadTextMode::Clipboard {
            let raw = if self.session.is_wayland()
                && wayland::WaylandCapabilities::probe().has_wl_clipboard
            {
                wayland::read_wayland_clipboard()?
            } else if let Some(dpy) = self.session.x11_display() {
                x11::read_x11_clipboard(dpy, self.xauth_path())?
            } else {
                wayland::read_wayland_clipboard()?
            };
            let sanitized = desktop_proto::sanitize_text(&raw, 16_384);
            return Ok(ReadTextResult {
                text: sanitized,
                source: "clipboard".into(),
            });
        }

        let rid = ref_id.ok_or_else(|| {
            RpcError::with_next(
                ErrorCode::InvalidInput,
                "read_text requires --ref for Value/Selection mode on Linux",
                "Pass ref_id from snapshot or use mode=\"clipboard\".",
            )
        })?;
        let entry = self.refs.get(rid)?;
        atspi::read_atspi_text(&entry, mode, redact_passwords)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_live_tests {
    use std::sync::Arc;

    use desktop_core::{AuditLog, DesktopEngine, PolicyStore, SystemClock};
    use desktop_proto::{
        ActParams, ActionStep, FocusMode, ImageFormat, PostAction, RpcMethod, RpcResult,
        ScreenshotParams, ScreenshotTarget, SnapshotParams, Target,
    };

    use super::*;

    /// Runs live against `$DISPLAY` (e.g. under `xvfb-run` in CI) and also tests managed `-auth` Xvfb!
    #[test]
    fn live_x11_capture_input_and_engine_roundtrip() {
        let cfg = if std::env::var("DISPLAY")
            .ok()
            .filter(|s| !s.is_empty())
            .is_some()
        {
            LinuxConfig::default()
        } else if session::command_exists("Xvfb") {
            LinuxConfig {
                xvfb: true,
                xvfb_config: XvfbConfig {
                    width: 1024,
                    height: 768,
                    depth: 24,
                    start_wm: false,
                    display_hint: None,
                },
                ..LinuxConfig::default()
            }
        } else {
            return;
        };

        let backend = Arc::new(LinuxBackend::connect(cfg).expect("connect LinuxBackend"));
        assert_eq!(backend.os_name(), "linux");
        assert!(!backend.is_macos());
        assert!(!backend.host_name().is_empty());

        let perms = backend.permissions();
        assert!(perms.screen_recording, "X11 capture should be ready");

        let displays = backend.displays().expect("query displays");
        assert!(!displays.is_empty());
        assert!(displays[0].pixel_width >= 320);

        let jpeg = backend
            .capture(
                &ScreenshotTarget::Display { index: Some(0) },
                ImageFormat::Jpeg,
                &[],
                FocusMode::Auto,
            )
            .expect("capture JPEG");
        assert_eq!(&jpeg.bytes[..3], &[0xFF, 0xD8, 0xFF]);

        let ann_png = backend
            .capture(
                &ScreenshotTarget::Display { index: Some(0) },
                ImageFormat::Png,
                &[AnnotateBox {
                    ref_id: "@e1".into(),
                    bounds: Rect {
                        x: 20.0,
                        y: 20.0,
                        width: 120.0,
                        height: 60.0,
                    },
                }],
                FocusMode::Auto,
            )
            .expect("capture annotated PNG");
        assert_eq!(
            &ann_png.bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );

        let tmp = tempfile::tempdir().unwrap();
        let engine = DesktopEngine::new(
            backend,
            Arc::new(SystemClock),
            PolicyStore::new(tmp.path().join("policy.toml")),
            AuditLog::new(tmp.path()),
        );

        let st = engine
            .handle(Some(1), None, Some("test"), RpcMethod::Status)
            .unwrap();
        assert!(matches!(st, RpcResult::Status(s) if s.os == "linux"));

        let snap = engine
            .handle(
                Some(1),
                None,
                Some("test"),
                RpcMethod::Snapshot(SnapshotParams::default()),
            )
            .unwrap();
        assert!(matches!(snap, RpcResult::Snapshot(_)));

        let shot = engine
            .handle(
                Some(1),
                None,
                Some("test"),
                RpcMethod::Screenshot(ScreenshotParams {
                    target: ScreenshotTarget::Display { index: None },
                    annotate: true,
                    format: ImageFormat::Jpeg,
                    if_changed_since: None,
                    focus_mode: FocusMode::Auto,
                }),
            )
            .unwrap();
        assert!(matches!(shot, RpcResult::Screenshot(s) if s.data_base64.is_some()));

        let act = engine
            .handle(
                Some(1),
                None,
                Some("test"),
                RpcMethod::Act(ActParams {
                    steps: vec![
                        ActionStep::Hover {
                            target: Target::Point {
                                x: 100.0,
                                y: 100.0,
                                display: None,
                            },
                        },
                        ActionStep::Key {
                            combo: "Shift+a".into(),
                            repeat: 1,
                        },
                    ],
                    after: PostAction::None,
                    expect_snapshot: None,
                    focus_mode: FocusMode::Auto,
                    release_after: false,
                }),
            )
            .unwrap();
        assert!(matches!(act, RpcResult::Act(r) if r.steps.len() == 2));
        engine.on_client_disconnect(1);
    }

    #[test]
    fn live_managed_authenticated_xvfb_boots_and_captures() {
        if !session::command_exists("Xvfb") {
            return;
        }
        let backend = LinuxBackend::connect(LinuxConfig {
            xvfb: true,
            xvfb_config: XvfbConfig {
                width: 800,
                height: 600,
                depth: 24,
                start_wm: false,
                display_hint: Some(120),
            },
            ..LinuxConfig::default()
        })
        .expect("start authenticated managed Xvfb");

        let displays = backend.displays().unwrap();
        assert_eq!(displays[0].pixel_width, 800);
        assert_eq!(displays[0].pixel_height, 600);

        let shot = backend
            .capture(
                &ScreenshotTarget::Display { index: Some(0) },
                ImageFormat::Png,
                &[],
                FocusMode::Auto,
            )
            .unwrap();
        assert_eq!(shot.width, 800);
        assert_eq!(shot.height, 600);
    }
}
