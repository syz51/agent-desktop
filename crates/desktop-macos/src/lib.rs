#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(target_os = "macos")]
mod ax;
#[cfg(target_os = "macos")]
mod capture;
#[cfg(target_os = "macos")]
mod ffi;
#[cfg(target_os = "macos")]
mod input;
#[cfg(target_os = "macos")]
mod power;
#[cfg(target_os = "macos")]
mod stream;
#[cfg(target_os = "macos")]
mod ui;
#[cfg(target_os = "macos")]
mod windows;

#[cfg(target_os = "macos")]
mod macos_backend {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use desktop_core::{AnnotateBox, PlatformBackend, RawScreenshot, ResolvedRef, StepExecInfo};
    use desktop_proto::{
        AppInfo, ClickMode, DisplayInfo, FocusMode, ImageFormat, KeyCombo, Modifiers, MouseButton,
        PermissionsStatus, Point, ReadTextMode, ReadTextResult, RpcError, ScreenState,
        ScreenshotTarget, ScrollDirection, Snapshot, SnapshotParams, TextMethod, WindowInfo,
    };

    use crate::{ax, capture, input, power, stream, ui, windows};

    pub use crate::ui::{UiHandle, run_main_loop};

    /// Native macOS implementation of [`PlatformBackend`].
    pub struct MacOsBackend {
        stop_flag: Arc<AtomicBool>,
        refs: ax::AxRefStore,
        awake: power::DisplayAwakeGuard,
        stream: stream::WarmDisplayStream,
        ui: Option<UiHandle>,
    }

    impl MacOsBackend {
        /// Create a backend wired to a running AppKit main-thread UI handle (daemon mode).
        #[must_use]
        pub fn with_ui(stop_flag: Arc<AtomicBool>, ui: UiHandle) -> Self {
            Self {
                stop_flag,
                refs: ax::AxRefStore::new(),
                awake: power::DisplayAwakeGuard::new(),
                stream: stream::WarmDisplayStream::new(),
                ui: Some(ui),
            }
        }

        /// Create a headless backend without an AppKit runloop (suitable for CLI doctor / live tests).
        #[must_use]
        pub fn headless() -> Self {
            Self {
                stop_flag: Arc::new(AtomicBool::new(false)),
                refs: ax::AxRefStore::new(),
                awake: power::DisplayAwakeGuard::new(),
                stream: stream::WarmDisplayStream::new(),
                ui: None,
            }
        }
    }

    impl PlatformBackend for MacOsBackend {
        fn os_name(&self) -> &'static str {
            "macos"
        }

        fn is_macos(&self) -> bool {
            true
        }

        fn host_name(&self) -> String {
            windows::local_host_name()
        }

        fn permissions(&self) -> PermissionsStatus {
            power::permissions()
        }

        fn request_permissions(&self) -> PermissionsStatus {
            power::request_permissions()
        }

        fn screen_state(&self) -> ScreenState {
            power::screen_state()
        }

        fn wake_display(&self) -> Result<bool, RpcError> {
            power::wake_display()
        }

        fn set_display_awake(&self, active: bool) {
            self.awake.set_active(active);
            if !active {
                self.stream.stop();
            }
        }

        fn invalidate_caches(&self) {
            self.stream.mark_mutation();
            capture::invalidate_shareable_content_cache();
        }

        fn set_overlay(&self, text: Option<&str>) {
            if let Some(ui) = &self.ui {
                ui.set_overlay(text);
            }
        }

        fn confirm_dialog(&self, title: &str, message: &str) -> Result<bool, RpcError> {
            if let Some(ui) = &self.ui {
                ui.confirm_dialog(title, message)
            } else {
                ui::confirm_dialog_fallback(title, message)
            }
        }

        fn stop_flag(&self) -> Arc<AtomicBool> {
            Arc::clone(&self.stop_flag)
        }

        fn displays(&self) -> Result<Vec<DisplayInfo>, RpcError> {
            windows::displays()
        }

        fn windows(&self, app_filter: Option<&str>) -> Result<Vec<WindowInfo>, RpcError> {
            windows::windows(app_filter)
        }

        fn focused_app(&self) -> Result<Option<AppInfo>, RpcError> {
            ax::focused_app()
        }

        fn app_at_point(&self, point: Point) -> Result<Option<AppInfo>, RpcError> {
            windows::app_at_point(point)
        }

        fn find_app(&self, query: &str) -> Result<Option<AppInfo>, RpcError> {
            Ok(windows::find_app(query))
        }

        fn open_app(&self, app: &str, focus_mode: FocusMode) -> Result<AppInfo, RpcError> {
            windows::open_app(app, focus_mode)
        }

        fn focus_window(&self, window_id: u32) -> Result<WindowInfo, RpcError> {
            windows::focus_window(window_id)
        }

        fn snapshot(
            &self,
            params: &SnapshotParams,
            redact_passwords: bool,
        ) -> Result<Snapshot, RpcError> {
            ax::take_snapshot(&self.refs, params, redact_passwords)
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
            capture::capture(&self.stream, target, format, annotate, focus_mode)
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
            input::click_ref(
                &self.refs,
                &self.stop_flag,
                ref_id,
                button,
                count,
                mods,
                click_mode,
                focus_mode,
            )
        }

        fn click_point(
            &self,
            point: Point,
            button: MouseButton,
            count: u8,
            mods: Modifiers,
            focus_mode: FocusMode,
        ) -> Result<StepExecInfo, RpcError> {
            input::click_point(&self.stop_flag, point, button, count, mods, focus_mode)
        }

        fn hover_point(&self, point: Point) -> Result<StepExecInfo, RpcError> {
            input::hover_point(&self.stop_flag, point)
        }

        fn drag(
            &self,
            from: Point,
            to: Point,
            mods: Modifiers,
            focus_mode: FocusMode,
        ) -> Result<StepExecInfo, RpcError> {
            input::drag(&self.stop_flag, from, to, mods, focus_mode)
        }

        fn scroll(
            &self,
            point: Point,
            direction: ScrollDirection,
            amount: u32,
            focus_mode: FocusMode,
        ) -> Result<StepExecInfo, RpcError> {
            input::scroll(&self.stop_flag, point, direction, amount, focus_mode)
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
            input::set_text(
                &self.refs,
                &self.stop_flag,
                ref_id,
                text,
                replace,
                submit,
                method,
                focus_mode,
            )
        }

        fn key(
            &self,
            combo: KeyCombo,
            repeat: u8,
            focus_mode: FocusMode,
        ) -> Result<StepExecInfo, RpcError> {
            input::press_key_combo(&self.stop_flag, combo, repeat, focus_mode)
        }

        fn menu(&self, app: &str, path: &[String]) -> Result<String, RpcError> {
            ax::invoke_menu(app, path)
        }

        fn read_text(
            &self,
            ref_id: Option<&str>,
            window_id: Option<u32>,
            mode: ReadTextMode,
            redact_passwords: bool,
        ) -> Result<ReadTextResult, RpcError> {
            ax::read_text(&self.refs, ref_id, window_id, mode, redact_passwords)
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos_backend::{MacOsBackend, UiHandle, run_main_loop};

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use desktop_core::PlatformBackend as _;

    use super::*;

    #[test]
    fn cf_string_roundtrips_unicode_and_emoji() {
        let sample = "你好 👋 agent-desktop — café";
        let cf = ffi::cf_string(sample);
        // SAFETY: `cf` is a valid owned CFString.
        let back = unsafe { ffi::cf_to_string(cf.as_ptr()) }.unwrap();
        assert_eq!(back, sample);
    }

    #[test]
    fn queries_live_macos_displays_windows_and_finder() {
        let backend = MacOsBackend::headless();
        assert_eq!(backend.os_name(), "macos");
        assert!(backend.is_macos());
        assert!(!backend.host_name().is_empty());

        let displays = backend.displays().unwrap();
        assert!(
            !displays.is_empty(),
            "macOS should report at least 1 display"
        );
        assert!(displays[0].bounds.width > 0.0);

        let _wins = backend.windows(None).unwrap();
        let finder = backend.find_app("Finder").unwrap();
        assert!(finder.is_some(), "Finder should be discoverable on macOS");
        assert_eq!(
            finder.unwrap().bundle_id.as_deref(),
            Some("com.apple.finder")
        );
    }
}
