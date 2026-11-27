use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use desktop_proto::{
    AppInfo, ClickMode, DisplayInfo, ElementAction, ErrorCode, FocusMode, ImageFormat, KeyCombo,
    Modifiers, MouseButton, PermissionsStatus, Point, ReadTextMode, ReadTextResult, Rect, RpcError,
    ScreenState, ScreenshotTarget, ScrollDirection, Snapshot, SnapshotParams, TextMethod, UiNode,
    WindowInfo,
};

use crate::backend::{AnnotateBox, PlatformBackend, RawScreenshot, ResolvedRef, StepExecInfo};

#[derive(Debug, Default)]
pub struct MockState {
    pub screen_state: Option<ScreenState>,
    pub permissions: Option<PermissionsStatus>,
    pub overlay_history: Vec<Option<String>>,
    pub display_awake: bool,
    pub woke_count: usize,
    pub confirm_result: bool,
    pub actions_log: Vec<String>,
    pub open_app_calls: Vec<(String, FocusMode)>,
    pub screenshot_bytes: Vec<u8>,
    pub snapshot_seq: u64,
    pub focused: Option<AppInfo>,
    pub ref_requires_focus: bool,
    pub ref_gone: bool,
}

pub struct MockBackend {
    pub stop: Arc<AtomicBool>,
    pub state: Mutex<MockState>,
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MockBackend {
    #[must_use]
    pub fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            state: Mutex::new(MockState {
                screen_state: Some(ScreenState::Unlocked),
                permissions: Some(PermissionsStatus {
                    accessibility: true,
                    screen_recording: true,
                }),
                confirm_result: true,
                screenshot_bytes: b"mock-jpeg-v1".to_vec(),
                focused: Some(sample_app()),
                ..MockState::default()
            }),
        }
    }
}

fn sample_app() -> AppInfo {
    AppInfo {
        name: "Notes".into(),
        bundle_id: Some("com.apple.Notes".into()),
        pid: Some(1234),
        path: Some("/System/Applications/Notes.app".into()),
        running: true,
    }
}

impl PlatformBackend for MockBackend {
    fn os_name(&self) -> &'static str {
        "macos"
    }

    fn is_macos(&self) -> bool {
        true
    }

    fn host_name(&self) -> String {
        "mock-mac".into()
    }

    fn permissions(&self) -> PermissionsStatus {
        self.state
            .lock()
            .unwrap()
            .permissions
            .unwrap_or(PermissionsStatus {
                accessibility: true,
                screen_recording: true,
            })
    }

    fn screen_state(&self) -> ScreenState {
        self.state
            .lock()
            .unwrap()
            .screen_state
            .unwrap_or(ScreenState::Unlocked)
    }

    fn wake_display(&self) -> Result<bool, RpcError> {
        let mut st = self.state.lock().unwrap();
        st.woke_count += 1;
        st.screen_state = Some(ScreenState::Unlocked);
        Ok(true)
    }

    fn set_display_awake(&self, active: bool) {
        self.state.lock().unwrap().display_awake = active;
    }

    fn set_overlay(&self, text: Option<&str>) {
        self.state
            .lock()
            .unwrap()
            .overlay_history
            .push(text.map(str::to_string));
    }

    fn confirm_dialog(&self, _title: &str, _message: &str) -> Result<bool, RpcError> {
        Ok(self.state.lock().unwrap().confirm_result)
    }

    fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    fn displays(&self) -> Result<Vec<DisplayInfo>, RpcError> {
        Ok(vec![DisplayInfo {
            index: 0,
            id: 1,
            main: true,
            bounds: Rect {
                x: 0.0,
                y: 0.0,
                width: 1440.0,
                height: 900.0,
            },
            pixel_width: 2880,
            pixel_height: 1800,
            scale: 2.0,
        }])
    }

    fn windows(&self, app_filter: Option<&str>) -> Result<Vec<WindowInfo>, RpcError> {
        let app = sample_app();
        if let Some(q) = app_filter
            && !app.matches_query(q)
        {
            return Ok(vec![]);
        }
        Ok(vec![WindowInfo {
            id: 101,
            title: "Project Plan".into(),
            app,
            bounds: Rect {
                x: 100.0,
                y: 80.0,
                width: 800.0,
                height: 600.0,
            },
            layer: 0,
            alpha: 1.0,
            focused: true,
            minimized: false,
            on_screen: true,
            display_index: Some(0),
        }])
    }

    fn focused_app(&self) -> Result<Option<AppInfo>, RpcError> {
        Ok(self.state.lock().unwrap().focused.clone())
    }

    fn app_at_point(&self, _point: Point) -> Result<Option<AppInfo>, RpcError> {
        Ok(Some(sample_app()))
    }

    fn find_app(&self, query: &str) -> Result<Option<AppInfo>, RpcError> {
        let app = sample_app();
        if app.matches_query(query) {
            return Ok(Some(app));
        }
        Ok(Some(AppInfo {
            name: query.into(),
            bundle_id: Some(format!("com.example.{}", query.to_ascii_lowercase())),
            pid: None,
            path: None,
            running: false,
        }))
    }

    fn open_app(&self, app: &str, focus_mode: FocusMode) -> Result<AppInfo, RpcError> {
        let mut st = self.state.lock().unwrap();
        st.open_app_calls.push((app.to_string(), focus_mode));
        st.actions_log.push(format!("open:{app}:{focus_mode:?}"));
        Ok(AppInfo {
            name: app.into(),
            bundle_id: None,
            pid: Some(999),
            path: None,
            running: true,
        })
    }

    fn focus_window(&self, window_id: u32) -> Result<WindowInfo, RpcError> {
        self.state
            .lock()
            .unwrap()
            .actions_log
            .push(format!("focus:{window_id}"));
        Ok(WindowInfo {
            id: window_id,
            title: "Project Plan".into(),
            app: sample_app(),
            bounds: Rect {
                x: 100.0,
                y: 80.0,
                width: 800.0,
                height: 600.0,
            },
            layer: 0,
            alpha: 1.0,
            focused: true,
            minimized: false,
            on_screen: true,
            display_index: Some(0),
        })
    }

    fn snapshot(
        &self,
        _params: &SnapshotParams,
        redact_passwords: bool,
    ) -> Result<Snapshot, RpcError> {
        let mut st = self.state.lock().unwrap();
        st.snapshot_seq += 1;
        let seq = st.snapshot_seq;
        let gone = st.ref_gone;
        drop(st);

        let mut children = vec![UiNode {
            ref_id: "@e2".into(),
            role: "textfield".into(),
            subrole: Some("secure".into()),
            title: Some("Secret".into()),
            label: None,
            value: Some(if redact_passwords {
                "[REDACTED]".into()
            } else {
                "hunter2".into()
            }),
            bounds: Some(Rect {
                x: 120.0,
                y: 120.0,
                width: 200.0,
                height: 28.0,
            }),
            enabled: true,
            focused: true,
            selected: false,
            actions: vec![ElementAction::SetValue, ElementAction::Focus],
            children: vec![],
        }];
        if !gone {
            children.push(UiNode {
                ref_id: "@e3".into(),
                role: "button".into(),
                subrole: None,
                title: Some("Save".into()),
                label: None,
                value: None,
                bounds: Some(Rect {
                    x: 340.0,
                    y: 120.0,
                    width: 80.0,
                    height: 28.0,
                }),
                enabled: true,
                focused: false,
                selected: false,
                actions: vec![ElementAction::Press],
                children: vec![],
            });
        }

        Ok(Snapshot {
            snapshot_id: format!("s{seq}"),
            app: sample_app(),
            window_id: Some(101),
            window_title: Some("Project Plan".into()),
            window_bounds: Some(Rect {
                x: 100.0,
                y: 80.0,
                width: 800.0,
                height: 600.0,
            }),
            truncated: false,
            total_nodes: children.len() + 1,
            emitted_nodes: children.len() + 1,
            elapsed_ms: 4,
            root: UiNode {
                ref_id: "@e1".into(),
                role: "window".into(),
                subrole: None,
                title: Some("Project Plan".into()),
                label: None,
                value: None,
                bounds: Some(Rect {
                    x: 100.0,
                    y: 80.0,
                    width: 800.0,
                    height: 600.0,
                }),
                enabled: true,
                focused: true,
                selected: false,
                actions: vec![],
                children,
            },
        })
    }

    fn resolve_ref(&self, ref_id: &str) -> Result<ResolvedRef, RpcError> {
        let st = self.state.lock().unwrap();
        if ref_id == "@e3" && st.ref_gone {
            return Err(RpcError::new(
                ErrorCode::StaleRef,
                "@e3 is no longer present",
            ));
        }
        Ok(ResolvedRef {
            ref_id: ref_id.into(),
            app: sample_app(),
            bounds: Some(Rect {
                x: 340.0,
                y: 120.0,
                width: 80.0,
                height: 28.0,
            }),
            actions: vec![ElementAction::Press],
        })
    }

    fn capture(
        &self,
        _target: &ScreenshotTarget,
        format: ImageFormat,
        _annotate: &[AnnotateBox],
        _focus_mode: FocusMode,
    ) -> Result<RawScreenshot, RpcError> {
        let bytes = self.state.lock().unwrap().screenshot_bytes.clone();
        Ok(RawScreenshot {
            width: 1440,
            height: 900,
            display_bounds: Rect {
                x: 0.0,
                y: 0.0,
                width: 1440.0,
                height: 900.0,
            },
            mime_type: format.mime_type().into(),
            bytes,
            visual_sig: None,
        })
    }

    fn click_ref(
        &self,
        ref_id: &str,
        _button: MouseButton,
        _count: u8,
        _mods: Modifiers,
        _click_mode: ClickMode,
        focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError> {
        let mut st = self.state.lock().unwrap();
        if st.ref_requires_focus && focus_mode == FocusMode::Background {
            return Err(RpcError::with_next(
                ErrorCode::RequiresFocus,
                format!("{ref_id} does not support background AXPress"),
                "Retry with focus_mode=\"auto\" or \"foreground\".",
            ));
        }
        st.actions_log.push(format!("click_ref:{ref_id}"));
        Ok(StepExecInfo {
            detail: format!("pressed {ref_id} via AXPress"),
            used_background: !st.ref_requires_focus,
        })
    }

    fn click_point(
        &self,
        point: Point,
        _button: MouseButton,
        _count: u8,
        _mods: Modifiers,
        _focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError> {
        self.state
            .lock()
            .unwrap()
            .actions_log
            .push(format!("click_pt:{:.0},{:.0}", point.x, point.y));
        Ok(StepExecInfo {
            detail: format!("clicked ({:.0}, {:.0})", point.x, point.y),
            used_background: false,
        })
    }

    fn hover_point(&self, point: Point) -> Result<StepExecInfo, RpcError> {
        self.state
            .lock()
            .unwrap()
            .actions_log
            .push(format!("hover:{:.0},{:.0}", point.x, point.y));
        Ok(StepExecInfo {
            detail: "hovered".into(),
            used_background: false,
        })
    }

    fn drag(
        &self,
        from: Point,
        to: Point,
        _mods: Modifiers,
        _focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError> {
        if self.stop.load(Ordering::Acquire) {
            return Err(RpcError::new(ErrorCode::StoppedByUser, "stopped"));
        }
        self.state.lock().unwrap().actions_log.push(format!(
            "drag:{:.0},{:.0}->{:.0},{:.0}",
            from.x, from.y, to.x, to.y
        ));
        Ok(StepExecInfo {
            detail: "dragged".into(),
            used_background: false,
        })
    }

    fn scroll(
        &self,
        _point: Point,
        direction: ScrollDirection,
        amount: u32,
        _focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError> {
        self.state
            .lock()
            .unwrap()
            .actions_log
            .push(format!("scroll:{direction:?}:{amount}"));
        Ok(StepExecInfo {
            detail: format!("scrolled {direction:?}"),
            used_background: false,
        })
    }

    fn set_text(
        &self,
        ref_id: Option<&str>,
        text: &str,
        _replace: bool,
        _submit: bool,
        _method: TextMethod,
        _focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError> {
        self.state.lock().unwrap().actions_log.push(format!(
            "set_text:{}:{len}",
            ref_id.unwrap_or("focused"),
            len = text.len()
        ));
        Ok(StepExecInfo {
            detail: format!("set {} chars", text.len()),
            used_background: ref_id.is_some(),
        })
    }

    fn key(
        &self,
        combo: KeyCombo,
        repeat: u8,
        _focus_mode: FocusMode,
    ) -> Result<StepExecInfo, RpcError> {
        self.state
            .lock()
            .unwrap()
            .actions_log
            .push(format!("key:{:?}x{repeat}", combo.key));
        Ok(StepExecInfo {
            detail: format!("pressed {:?}", combo.key),
            used_background: false,
        })
    }

    fn menu(&self, app: &str, path: &[String]) -> Result<String, RpcError> {
        self.state
            .lock()
            .unwrap()
            .actions_log
            .push(format!("menu:{app}:{}", path.join(">")));
        Ok(format!("invoked {} in {app}", path.join(" > ")))
    }

    fn read_text(
        &self,
        ref_id: Option<&str>,
        _window_id: Option<u32>,
        mode: ReadTextMode,
        _redact_passwords: bool,
    ) -> Result<ReadTextResult, RpcError> {
        Ok(ReadTextResult {
            text: format!("mock-{mode:?}-{}", ref_id.unwrap_or("focused")),
            source: "mock".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use desktop_proto::{
        ActParams, ActionStep, ClickMode, ErrorCode, FindParams, FocusMode, MouseButton,
        PolicyConfig, PolicyMode, PostAction, RpcMethod, RpcResult, ScreenState, ScreenshotParams,
        SessionStartParams, SnapshotParams, Target, TextMethod, WaitCondition, WaitParams,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::audit::AuditLog;
    use crate::clock::MockClock;
    use crate::engine::DesktopEngine;
    use crate::policy::PolicyStore;

    fn make_engine() -> (DesktopEngine, Arc<MockBackend>, Arc<MockClock>, TempDir) {
        let tmp = TempDir::new().unwrap();
        let backend = Arc::new(MockBackend::new());
        let clock = Arc::new(MockClock::new(Utc::now()));
        let policy = PolicyStore::new(tmp.path().join("policy.toml"));
        let audit = AuditLog::new(tmp.path());
        let engine = DesktopEngine::new(backend.clone(), clock.clone(), policy, audit);
        (engine, backend, clock, tmp)
    }

    #[test]
    fn open_policy_auto_acquires_lease_and_disconnect_releases() {
        let (engine, backend, _, _tmp) = make_engine();

        // Client 1 auto-acquires lease on snapshot without explicit session_start.
        let res = engine
            .handle(
                Some(1),
                None,
                Some("gemini"),
                RpcMethod::Snapshot(SnapshotParams::default()),
            )
            .unwrap();
        assert!(matches!(res, RpcResult::Snapshot(_)));
        assert!(backend.state.lock().unwrap().display_awake);

        // Client 2 gets LeaseBusy while client 1 holds control.
        let err = engine
            .handle(
                Some(2),
                None,
                Some("claude"),
                RpcMethod::Snapshot(SnapshotParams::default()),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::LeaseBusy);

        // Client 1 disconnects -> lease immediately freed & overlay cleared.
        engine.on_client_disconnect(1);
        assert!(!backend.state.lock().unwrap().display_awake);
        assert_eq!(
            backend.state.lock().unwrap().overlay_history.last(),
            Some(&None)
        );

        // Client 2 now succeeds immediately.
        assert!(
            engine
                .handle(
                    Some(2),
                    None,
                    Some("claude"),
                    RpcMethod::Snapshot(SnapshotParams::default()),
                )
                .is_ok()
        );
    }

    #[test]
    fn lease_expires_via_injectable_clock() {
        let (engine, _, clock, _tmp) = make_engine();
        engine
            .handle(
                Some(1),
                None,
                Some("a"),
                RpcMethod::SessionStart(SessionStartParams::default()),
            )
            .unwrap();

        clock.advance(Duration::minutes(11));

        // Client 2 can take over after expiry.
        assert!(
            engine
                .handle(
                    Some(2),
                    None,
                    Some("b"),
                    RpcMethod::SessionStart(SessionStartParams::default()),
                )
                .is_ok()
        );
    }

    #[test]
    fn screen_locked_fails_fast_and_display_asleep_auto_wakes() {
        let (engine, backend, _, _tmp) = make_engine();
        backend.state.lock().unwrap().screen_state = Some(ScreenState::ScreenLocked);
        let err = engine
            .handle(
                Some(1),
                None,
                Some("a"),
                RpcMethod::Screenshot(ScreenshotParams::default()),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::ScreenLocked);
        // Must not acquire a lease or flash overlay when failing on ScreenLocked.
        assert!(backend.state.lock().unwrap().overlay_history.is_empty());
        assert!(!backend.state.lock().unwrap().display_awake);

        backend.state.lock().unwrap().screen_state = Some(ScreenState::DisplayAsleep);
        let res = engine
            .handle(
                Some(1),
                None,
                Some("a"),
                RpcMethod::Screenshot(ScreenshotParams::default()),
            )
            .unwrap();
        assert!(matches!(res, RpcResult::Screenshot(_)));
        assert_eq!(backend.state.lock().unwrap().woke_count, 1);
    }

    #[test]
    fn batched_act_and_mid_batch_stop_work_cleanly() {
        let (engine, backend, _, _tmp) = make_engine();

        let snap = match engine
            .handle(
                Some(1),
                None,
                Some("a"),
                RpcMethod::Snapshot(SnapshotParams::default()),
            )
            .unwrap()
        {
            RpcResult::Snapshot(s) => s,
            _ => unreachable!(),
        };

        // Run a 3-step batch with expect_snapshot + post-action snapshot.
        let act = match engine
            .handle(
                Some(1),
                None,
                Some("a"),
                RpcMethod::Act(ActParams {
                    steps: vec![
                        ActionStep::SetText {
                            ref_id: Some("@e2".into()),
                            text: "hello world".into(),
                            replace: true,
                            submit: false,
                            method: TextMethod::Auto,
                        },
                        ActionStep::Click {
                            target: Target::Ref {
                                ref_id: "@e3".into(),
                            },
                            button: MouseButton::Left,
                            count: 1,
                            modifiers: vec![],
                            mode: ClickMode::Auto,
                        },
                        ActionStep::Key {
                            combo: "mod+s".into(),
                            repeat: 1,
                        },
                    ],
                    after: PostAction::Snapshot,
                    expect_snapshot: Some(snap.snapshot_id),
                    focus_mode: FocusMode::Auto,
                    release_after: false,
                }),
            )
            .unwrap()
        {
            RpcResult::Act(r) => r,
            _ => unreachable!(),
        };
        assert_eq!(act.completed, 3);
        assert_eq!(act.total, 3);
        assert!(act.error.is_none());
        assert!(act.snapshot.is_some());

        // Trigger physical stop -> next act fails with StoppedByUser cleanly.
        backend.stop.store(true, Ordering::Release);
        let err = engine
            .handle(
                Some(1),
                None,
                Some("a"),
                RpcMethod::Act(ActParams {
                    steps: vec![ActionStep::Key {
                        combo: "Return".into(),
                        repeat: 1,
                    }],
                    after: PostAction::None,
                    expect_snapshot: None,
                    focus_mode: FocusMode::Auto,
                    release_after: false,
                }),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::StoppedByUser);
    }

    #[test]
    fn background_focus_mode_reports_requires_focus_when_needed() {
        let (engine, backend, _, _tmp) = make_engine();
        backend.state.lock().unwrap().ref_requires_focus = true;

        let res = match engine
            .handle(
                Some(1),
                None,
                Some("a"),
                RpcMethod::Act(ActParams {
                    steps: vec![ActionStep::Click {
                        target: Target::Ref {
                            ref_id: "@e3".into(),
                        },
                        button: MouseButton::Left,
                        count: 1,
                        modifiers: vec![],
                        mode: ClickMode::Auto,
                    }],
                    after: PostAction::None,
                    expect_snapshot: None,
                    focus_mode: FocusMode::Background,
                    release_after: false,
                }),
            )
            .unwrap()
        {
            RpcResult::Act(r) => r,
            _ => unreachable!(),
        };
        assert_eq!(res.completed, 0);
        assert_eq!(res.error.unwrap().code, ErrorCode::RequiresFocus);
    }

    #[test]
    fn screenshot_unchanged_short_circuits_image_payload() {
        let (engine, backend, _, _tmp) = make_engine();
        let first = match engine
            .handle(
                Some(1),
                None,
                Some("a"),
                RpcMethod::Screenshot(ScreenshotParams::default()),
            )
            .unwrap()
        {
            RpcResult::Screenshot(s) => s,
            _ => unreachable!(),
        };
        assert!(!first.unchanged);
        assert!(first.data_base64.is_some());

        let second = match engine
            .handle(
                Some(1),
                None,
                Some("a"),
                RpcMethod::Screenshot(ScreenshotParams {
                    if_changed_since: Some(first.hash.clone()),
                    ..ScreenshotParams::default()
                }),
            )
            .unwrap()
        {
            RpcResult::Screenshot(s) => s,
            _ => unreachable!(),
        };
        assert!(second.unchanged);
        assert!(second.data_base64.is_none());

        backend.state.lock().unwrap().screenshot_bytes = b"mock-jpeg-v2".to_vec();
        let third = match engine
            .handle(
                Some(1),
                None,
                Some("a"),
                RpcMethod::Screenshot(ScreenshotParams {
                    if_changed_since: Some(first.hash),
                    ..ScreenshotParams::default()
                }),
            )
            .unwrap()
        {
            RpcResult::Screenshot(s) => s,
            _ => unreachable!(),
        };
        assert!(!third.unchanged);
        assert!(third.data_base64.is_some());
    }

    #[test]
    fn opt_in_policy_deny_and_confirm_and_find_and_wait() {
        let (engine, backend, _, tmp) = make_engine();

        // Find matches Save button.
        let found = match engine
            .handle(
                Some(1),
                None,
                Some("a"),
                RpcMethod::Find(FindParams {
                    query: "save".into(),
                    role: Some("button".into()),
                    app: None,
                    window_id: None,
                    limit: 10,
                }),
            )
            .unwrap()
        {
            RpcResult::Find { matches, .. } => matches,
            _ => unreachable!(),
        };
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].ref_id, "@e3");

        // Wait for Exists succeeds immediately.
        let waited = match engine
            .handle(
                Some(1),
                None,
                Some("a"),
                RpcMethod::Wait(WaitParams {
                    condition: WaitCondition::Exists {
                        query: "Save".into(),
                        role: Some("button".into()),
                        app: None,
                    },
                    timeout_ms: 500,
                }),
            )
            .unwrap()
        {
            RpcResult::Wait { matched, .. } => matched,
            _ => unreachable!(),
        };
        assert!(waited);

        // Opt-in deny_apps blocks Notes when explicitly configured by owner.
        engine
            .handle(
                Some(1),
                None,
                Some("a"),
                RpcMethod::PolicySet(PolicyConfig {
                    mode: PolicyMode::Open,
                    deny_apps: vec!["com.apple.Notes".into()],
                    confirm_apps: vec![],
                    redact_passwords: true,
                    overlay: true,
                }),
            )
            .unwrap();
        let err = engine
            .handle(
                Some(1),
                None,
                Some("a"),
                RpcMethod::Snapshot(SnapshotParams::default()),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::PolicyDenied);

        // Audit file was written.
        let audit_text = std::fs::read_to_string(tmp.path().join("events.jsonl")).unwrap();
        assert!(audit_text.contains("policy_set"));

        // Confirm mode respects dialog denial.
        backend.state.lock().unwrap().confirm_result = false;
        engine
            .handle(
                Some(2),
                None,
                Some("a"),
                RpcMethod::PolicySet(PolicyConfig {
                    mode: PolicyMode::Confirm,
                    ..PolicyConfig::default()
                }),
            )
            .unwrap();
        engine.on_client_disconnect(1);
        let err = engine
            .handle(
                Some(2),
                None,
                Some("a"),
                RpcMethod::SessionStart(SessionStartParams::default()),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::PolicyDenied);
    }

    #[test]
    fn open_app_passes_focus_mode_and_reports_background_execution() {
        let (engine, backend, _, _tmp) = make_engine();

        for (mode, expect_bg) in [
            (FocusMode::Background, true),
            (FocusMode::Auto, true),
            (FocusMode::Foreground, false),
        ] {
            let act = match engine
                .handle(
                    Some(1),
                    None,
                    Some("agent"),
                    RpcMethod::Act(ActParams {
                        steps: vec![ActionStep::OpenApp {
                            app: "Notes".into(),
                        }],
                        after: PostAction::None,
                        expect_snapshot: None,
                        focus_mode: mode,
                        release_after: false,
                    }),
                )
                .unwrap()
            {
                RpcResult::Act(r) => r,
                _ => unreachable!(),
            };
            assert_eq!(act.completed, 1);
            assert_eq!(act.steps[0].op, "open_app");
            assert_eq!(act.steps[0].used_background, expect_bg);
        }

        assert_eq!(
            backend.state.lock().unwrap().open_app_calls,
            vec![
                ("Notes".into(), FocusMode::Background),
                ("Notes".into(), FocusMode::Auto),
                ("Notes".into(), FocusMode::Foreground),
            ]
        );
    }

    #[test]
    fn compound_act_read_text_and_release_after() {
        let (engine, backend, _, _tmp) = make_engine();

        let act = match engine
            .handle(
                Some(1),
                None,
                Some("agent-1"),
                RpcMethod::Act(ActParams {
                    steps: vec![
                        ActionStep::Click {
                            target: Target::Ref {
                                ref_id: "@e3".into(),
                            },
                            button: MouseButton::Left,
                            count: 1,
                            modifiers: vec![],
                            mode: ClickMode::Auto,
                        },
                        ActionStep::ReadText {
                            mode: ReadTextMode::Value,
                            ref_id: Some("@e2".into()),
                            window_id: Some(101),
                        },
                        ActionStep::ReadText {
                            mode: ReadTextMode::Clipboard,
                            ref_id: None,
                            window_id: None,
                        },
                        ActionStep::Menu {
                            app: "Notes".into(),
                            path: vec!["File".into(), "New Note".into()],
                        },
                    ],
                    after: PostAction::Snapshot,
                    expect_snapshot: None,
                    focus_mode: FocusMode::Auto,
                    release_after: true,
                }),
            )
            .unwrap()
        {
            RpcResult::Act(r) => r,
            _ => unreachable!(),
        };

        assert_eq!(act.completed, 4);
        assert_eq!(act.total, 4);
        assert!(act.error.is_none());
        assert!(act.released);
        assert!(act.snapshot.is_some());
        assert_eq!(act.steps[1].op, "read_text");
        assert!(act.steps[1].used_background);
        assert_eq!(act.steps[1].text.as_deref(), Some("mock-Value-@e2"));
        assert!(act.steps[1].detail.contains("mock-Value-@e2"));
        assert_eq!(act.steps[2].text.as_deref(), Some("mock-Clipboard-focused"));
        assert!(act.steps[2].detail.contains("mock-Clipboard-focused"));

        // Lease was released cleanly: display_awake is false and another client can immediately acquire.
        assert!(!backend.state.lock().unwrap().display_awake);
        assert!(
            engine
                .handle(
                    Some(2),
                    None,
                    Some("agent-2"),
                    RpcMethod::Snapshot(SnapshotParams::default()),
                )
                .is_ok()
        );
    }

    #[test]
    fn act_session_end_step_and_release_after_on_error() {
        let (engine, backend, _, _tmp) = make_engine();

        // ActionStep::SessionEnd releases the lease inline and is idempotent with release_after.
        let act = match engine
            .handle(
                Some(1),
                None,
                Some("agent-1"),
                RpcMethod::Act(ActParams {
                    steps: vec![
                        ActionStep::ReadText {
                            mode: ReadTextMode::Selection,
                            ref_id: Some("@e2".into()),
                            window_id: None,
                        },
                        ActionStep::SessionEnd,
                    ],
                    after: PostAction::None,
                    expect_snapshot: None,
                    focus_mode: FocusMode::Background,
                    release_after: true,
                }),
            )
            .unwrap()
        {
            RpcResult::Act(r) => r,
            _ => unreachable!(),
        };
        assert_eq!(act.completed, 2);
        assert!(act.released);
        assert_eq!(act.steps[1].op, "session_end");
        assert!(act.steps[1].used_background);
        assert!(!backend.state.lock().unwrap().display_awake);

        // On mid-batch failure, release_after=true still frees the lease so it never leaks.
        backend.state.lock().unwrap().ref_requires_focus = true;
        let failed = match engine
            .handle(
                Some(1),
                None,
                Some("agent-1"),
                RpcMethod::Act(ActParams {
                    steps: vec![ActionStep::Click {
                        target: Target::Ref {
                            ref_id: "@e3".into(),
                        },
                        button: MouseButton::Left,
                        count: 1,
                        modifiers: vec![],
                        mode: ClickMode::Auto,
                    }],
                    after: PostAction::None,
                    expect_snapshot: None,
                    focus_mode: FocusMode::Background,
                    release_after: true,
                }),
            )
            .unwrap()
        {
            RpcResult::Act(r) => r,
            _ => unreachable!(),
        };
        assert_eq!(failed.completed, 0);
        assert!(failed.error.is_some());
        assert!(failed.released);
        assert!(!backend.state.lock().unwrap().display_awake);
        assert!(
            engine
                .handle(
                    Some(2),
                    None,
                    Some("agent-2"),
                    RpcMethod::Snapshot(SnapshotParams::default()),
                )
                .is_ok()
        );
    }
}
