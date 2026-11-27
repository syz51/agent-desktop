use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::DateTime;
use desktop_proto::{
    ActParams, ActResult, ActionStep, AppInfo, ErrorCode, FocusMode, HostStatus, ImageFormat,
    LeaseInfo, ListWindowsParams, Point, PolicyConfig, PolicyMode, PostAction, Rect, RpcError,
    RpcMethod, RpcResult, ScreenState, ScreenshotParams, ScreenshotResult, ScreenshotTarget,
    SessionStartParams, Snapshot, SnapshotParams, StepOutcome, Target, WaitCondition,
    parse_key_combo, parse_modifiers, screenshot_to_global,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::audit::{AuditEvent, AuditLog};
use crate::backend::{PlatformBackend, RawScreenshot};
use crate::clock::{Clock, default_lease_ttl};
use crate::frame_ring::{DEFAULT_WARM_FRAME_TTL_MS, FrameKey, FrameRing, has_visual_change};
use crate::policy::{PolicyStore, check_app_policy};
use crate::tree::{collect_annotate_boxes, find_in_snapshot};

#[derive(Debug, Clone)]
struct ActiveLease {
    info: LeaseInfo,
    client_id: Option<u64>,
    /// Optional explicit app scope requested by the caller in `session_start(apps: [...])`.
    scoped_apps: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
struct CachedShot {
    width: u32,
    height: u32,
    display_bounds: Rect,
}

#[derive(Default)]
struct EngineState {
    lease: Option<ActiveLease>,
    last_snapshot: Option<Snapshot>,
    shots_by_display: HashMap<u32, CachedShot>,
    last_display: Option<u32>,
    last_shot_hash: Option<String>,
    frame_ring: FrameRing,
}

/// Core stateful engine shared by `desktopd` and unit/integration tests.
pub struct DesktopEngine {
    backend: Arc<dyn PlatformBackend>,
    clock: Arc<dyn Clock>,
    policy: PolicyStore,
    audit: AuditLog,
    state: Mutex<EngineState>,
}

impl DesktopEngine {
    #[must_use]
    pub fn new(
        backend: Arc<dyn PlatformBackend>,
        clock: Arc<dyn Clock>,
        policy: PolicyStore,
        audit: AuditLog,
    ) -> Self {
        Self {
            backend,
            clock,
            policy,
            audit,
            state: Mutex::new(EngineState::default()),
        }
    }

    /// Release any lease held by a disconnected client immediately (eliminating stale PID locks).
    pub fn on_client_disconnect(&self, client_id: u64) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st
            .lease
            .as_ref()
            .is_some_and(|l| l.client_id == Some(client_id))
        {
            let lease = st.lease.take().expect("checked above");
            drop(st);
            self.teardown_lease(&lease, "client_disconnect");
        }
    }

    /// Trigger an immediate external stop (e.g. `desktop stop` CLI or SIGINT).
    pub fn trigger_stop(&self, caller: Option<&str>) -> bool {
        self.backend.stop_flag().store(true, Ordering::Release);
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lease) = st.lease.take() {
            drop(st);
            self.teardown_lease(&lease, "stopped_by_user");
            self.backend.stop_flag().store(false, Ordering::Release);
            self.audit.record(AuditEvent {
                time: self.clock.now(),
                event: "stop",
                caller,
                session_id: Some(&lease.info.session_id),
                detail: Some("explicit stop"),
            });
            true
        } else {
            self.backend.stop_flag().store(false, Ordering::Release);
            false
        }
    }

    /// Dispatch one RPC method on behalf of `(client_id, session_id, caller)`.
    pub fn handle(
        &self,
        client_id: Option<u64>,
        session_id: Option<&str>,
        caller: Option<&str>,
        method: RpcMethod,
    ) -> Result<RpcResult, RpcError> {
        match method {
            RpcMethod::Status => self.handle_status(),
            RpcMethod::RequestPermissions => {
                let _ = self.backend.request_permissions();
                self.handle_status()
            }
            RpcMethod::ListWindows(params) => self.handle_list_windows(params),
            RpcMethod::PolicyGet => Ok(RpcResult::Policy(self.policy.load()?)),
            RpcMethod::PolicySet(cfg) => {
                self.policy.save(&cfg)?;
                self.audit.record(AuditEvent {
                    time: self.clock.now(),
                    event: "policy_set",
                    caller,
                    session_id,
                    detail: Some(match cfg.mode {
                        PolicyMode::Open => "open",
                        PolicyMode::Confirm => "confirm",
                    }),
                });
                Ok(RpcResult::Policy(cfg))
            }
            RpcMethod::Stop => {
                let had_active_session = self.trigger_stop(caller);
                Ok(RpcResult::Stopped { had_active_session })
            }
            RpcMethod::SessionStart(params) => self.handle_session_start(client_id, caller, params),
            RpcMethod::SessionEnd => self.handle_session_end(client_id, session_id, caller),
            RpcMethod::Snapshot(params) => {
                params.validate()?;
                let policy = self.policy.load()?;
                self.ensure_display_ready(false)?;
                self.ensure_active_session(client_id, session_id, caller, &policy)?;
                let snap = self.capture_snapshot(&params, &policy)?;
                Ok(RpcResult::Snapshot(Box::new(snap)))
            }
            RpcMethod::Find(params) => {
                params.validate()?;
                let policy = self.policy.load()?;
                self.ensure_display_ready(false)?;
                self.ensure_active_session(client_id, session_id, caller, &policy)?;
                let snap_params = SnapshotParams {
                    app: params.app.clone(),
                    window_id: params.window_id,
                    root_ref: None,
                    max_depth: Some(16),
                    max_nodes: Some(500),
                    focus_mode: FocusMode::Background,
                };
                let snap = self.capture_snapshot(&snap_params, &policy)?;
                let matches =
                    find_in_snapshot(&snap, &params.query, params.role.as_deref(), params.limit);
                Ok(RpcResult::Find {
                    snapshot_id: snap.snapshot_id,
                    matches,
                })
            }
            RpcMethod::Screenshot(params) => {
                params.validate()?;
                let policy = self.policy.load()?;
                self.ensure_display_ready(true)?;
                self.ensure_active_session(client_id, session_id, caller, &policy)?;
                let shot = self.capture_screenshot(&params, &policy)?;
                Ok(RpcResult::Screenshot(Box::new(shot)))
            }
            RpcMethod::Act(params) => {
                params.validate(self.backend.is_macos())?;
                let policy = self.policy.load()?;
                self.ensure_display_ready(true)?;
                self.ensure_active_session(client_id, session_id, caller, &policy)?;
                let result = self.run_act(params, &policy, caller)?;
                Ok(RpcResult::Act(Box::new(result)))
            }
            RpcMethod::ReadText(params) => {
                let policy = self.policy.load()?;
                self.ensure_active_session(client_id, session_id, caller, &policy)?;
                let res = self.backend.read_text(
                    params.ref_id.as_deref(),
                    params.window_id,
                    params.mode,
                    policy.redact_passwords,
                )?;
                Ok(RpcResult::ReadText(res))
            }
            RpcMethod::Menu(params) => {
                params.validate()?;
                let policy = self.policy.load()?;
                self.ensure_active_session(client_id, session_id, caller, &policy)?;
                self.ensure_app_allowed_by_query(&params.app, &policy)?;
                let detail = self.backend.menu(&params.app, &params.path)?;
                self.invalidate_frame_caches();
                Ok(RpcResult::Menu { detail })
            }
            RpcMethod::Wait(params) => {
                params.validate()?;
                let policy = self.policy.load()?;
                self.ensure_active_session(client_id, session_id, caller, &policy)?;
                self.ensure_display_ready(false)?;
                let (matched, elapsed_ms, detail) =
                    self.run_wait(&params.condition, params.timeout_ms, &policy)?;
                self.invalidate_frame_caches();
                Ok(RpcResult::Wait {
                    matched,
                    elapsed_ms,
                    detail,
                })
            }
        }
    }

    fn handle_status(&self) -> Result<RpcResult, RpcError> {
        let policy = self.policy.load()?;
        let displays = self.backend.displays().map(|d| d.len() as u32).unwrap_or(0);
        let active_lease = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            self.expire_lease_if_needed(&mut st);
            st.lease.as_ref().map(|l| l.info.clone())
        };
        Ok(RpcResult::Status(HostStatus {
            host: self.backend.host_name(),
            os: self.backend.os_name().into(),
            version: env!("CARGO_PKG_VERSION").into(),
            screen_state: self.backend.screen_state(),
            permissions: self.backend.permissions(),
            policy_mode: policy.mode,
            active_lease,
            displays,
        }))
    }

    fn handle_list_windows(&self, params: ListWindowsParams) -> Result<RpcResult, RpcError> {
        let displays = if params.include_displays {
            self.backend.displays()?
        } else {
            Vec::new()
        };
        let windows = self.backend.windows(params.app.as_deref())?;
        Ok(RpcResult::Windows { displays, windows })
    }

    fn handle_session_start(
        &self,
        client_id: Option<u64>,
        caller: Option<&str>,
        params: SessionStartParams,
    ) -> Result<RpcResult, RpcError> {
        let policy = self.policy.load()?;
        // Check opt-in deny/confirm rules on explicitly named apps before taking the lease.
        let mut needs_confirm = policy.mode == PolicyMode::Confirm;
        if let Some(apps) = &params.apps {
            for query in apps {
                if let Some(app) = self.backend.find_app(query)?
                    && check_app_policy(&policy, &app)?
                {
                    needs_confirm = true;
                }
            }
        }

        if needs_confirm {
            let owner = caller.unwrap_or("agent");
            let app_list = params
                .apps
                .as_ref()
                .map(|a| a.join(", "))
                .unwrap_or_else(|| "all apps".into());
            let reason = params.reason.as_deref().unwrap_or("desktop automation");
            let ok = self.backend.confirm_dialog(
                "agent-desktop confirmation",
                &format!("{owner} wants to control {app_list}.\n\nReason: {reason}"),
            )?;
            if !ok {
                self.audit.record(AuditEvent {
                    time: self.clock.now(),
                    event: "session_denied",
                    caller,
                    session_id: None,
                    detail: Some(reason),
                });
                return Err(RpcError::with_next(
                    ErrorCode::PolicyDenied,
                    "user denied session confirmation",
                    "Ask the user or switch policy.toml mode to \"open\".",
                ));
            }
        }

        let wait_deadline = params
            .wait_for_lease_ms
            .map(|ms| Instant::now() + StdDuration::from_millis(ms.min(15_000)));

        let lease = loop {
            let now = self.clock.now();
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            self.expire_lease_if_needed(&mut st);

            let can_take = match &st.lease {
                None => true,
                Some(existing) if client_id.is_some() && existing.client_id == client_id => true,
                Some(_) => false,
            };

            if can_take {
                let session_id = st
                    .lease
                    .as_ref()
                    .map(|l| l.info.session_id.clone())
                    .unwrap_or_else(|| Uuid::new_v4().to_string());
                let owner = caller.unwrap_or("agent").to_string();
                let info = LeaseInfo {
                    session_id,
                    owner: owner.clone(),
                    acquired_at: now,
                    expires_at: now + default_lease_ttl(),
                    apps: params.apps.clone(),
                };
                let active = ActiveLease {
                    info: info.clone(),
                    client_id,
                    scoped_apps: params.apps.clone(),
                };
                st.lease = Some(active);
                break info;
            }

            let holder = st
                .lease
                .as_ref()
                .map(|l| l.info.owner.clone())
                .unwrap_or_default();
            drop(st);

            if wait_deadline.is_some_and(|d| Instant::now() < d) {
                std::thread::sleep(StdDuration::from_millis(50));
                continue;
            }
            return Err(RpcError::with_next(
                ErrorCode::LeaseBusy,
                format!("another session ({holder}) currently holds control of this machine"),
                "Retry with wait_for_lease_ms or wait for the active session to release.",
            ));
        };

        self.backend.stop_flag().store(false, Ordering::Release);
        let woke_display = self.ensure_display_ready(false)?;
        self.backend.set_display_awake(true);
        if policy.overlay {
            let scope = params
                .apps
                .as_ref()
                .map(|a| a.join(", "))
                .unwrap_or_else(|| "desktop".into());
            self.backend
                .set_overlay(Some(&format!("{} controls: {scope}", lease.owner)));
        }

        self.audit.record(AuditEvent {
            time: self.clock.now(),
            event: "session_start",
            caller,
            session_id: Some(&lease.session_id),
            detail: params.reason.as_deref(),
        });

        Ok(RpcResult::Session {
            lease,
            woke_display,
        })
    }

    fn handle_session_end(
        &self,
        client_id: Option<u64>,
        session_id: Option<&str>,
        caller: Option<&str>,
    ) -> Result<RpcResult, RpcError> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let matches = st.lease.as_ref().is_some_and(|l| {
            session_id.is_some_and(|sid| sid == l.info.session_id)
                || (client_id.is_some() && l.client_id == client_id)
                || (session_id.is_none() && client_id.is_none())
        });
        if matches {
            let lease = st.lease.take().expect("checked above");
            drop(st);
            self.teardown_lease(&lease, "session_end");
            self.audit.record(AuditEvent {
                time: self.clock.now(),
                event: "session_end",
                caller,
                session_id: Some(&lease.info.session_id),
                detail: None,
            });
            Ok(RpcResult::Released {
                message: "Session released; lease is free.".into(),
            })
        } else {
            Ok(RpcResult::Released {
                message: "No matching active session was held.".into(),
            })
        }
    }

    fn ensure_active_session(
        &self,
        client_id: Option<u64>,
        session_id: Option<&str>,
        caller: Option<&str>,
        policy: &PolicyConfig,
    ) -> Result<(), RpcError> {
        self.check_not_stopped()?;

        let now = self.clock.now();
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.expire_lease_if_needed(&mut st);

        if let Some(existing) = &mut st.lease {
            let owns = session_id.is_some_and(|sid| sid == existing.info.session_id)
                || (client_id.is_some() && existing.client_id == client_id);
            if owns {
                existing.info.expires_at = now + default_lease_ttl();
                return Ok(());
            }
            let owner = existing.info.owner.clone();
            return Err(RpcError::with_next(
                ErrorCode::LeaseBusy,
                format!("another session ({owner}) currently controls this machine"),
                "Call session_start with wait_for_lease_ms or wait for it to release.",
            ));
        }

        // In default Open mode, auto-acquire a lease on first action/observe call to save a round-trip.
        if policy.mode == PolicyMode::Open {
            let owner = caller.unwrap_or("agent").to_string();
            let info = LeaseInfo {
                session_id: Uuid::new_v4().to_string(),
                owner: owner.clone(),
                acquired_at: now,
                expires_at: now + default_lease_ttl(),
                apps: None,
            };
            let sid = info.session_id.clone();
            st.lease = Some(ActiveLease {
                info,
                client_id,
                scoped_apps: None,
            });
            drop(st);
            self.backend.stop_flag().store(false, Ordering::Release);
            self.backend.set_display_awake(true);
            if policy.overlay {
                self.backend
                    .set_overlay(Some(&format!("{owner} controls: desktop")));
            }
            self.audit.record(AuditEvent {
                time: now,
                event: "session_auto_start",
                caller,
                session_id: Some(&sid),
                detail: None,
            });
            return Ok(());
        }

        Err(RpcError::with_next(
            ErrorCode::PolicyDenied,
            "confirm policy is active; no session is started",
            "Call session_start first so the user can confirm.",
        ))
    }

    fn ensure_display_ready(&self, require_screen_recording: bool) -> Result<bool, RpcError> {
        match self.backend.screen_state() {
            ScreenState::ScreenLocked => {
                return Err(RpcError::with_next(
                    ErrorCode::ScreenLocked,
                    "this machine's screen is locked (loginwindow is active)",
                    "Unlock the Mac, or disable 'Require password after display is turned off' with a virtual display for unattended use.",
                ));
            }
            ScreenState::DisplayAsleep => {
                let woke = self.backend.wake_display()?;
                if require_screen_recording && !self.backend.permissions().screen_recording {
                    return Err(RpcError::with_next(
                        ErrorCode::PermissionDenied,
                        "Screen Recording permission is not granted",
                        "Grant Screen Recording to AgentDesktop.app in System Settings > Privacy & Security.",
                    ));
                }
                return Ok(woke);
            }
            ScreenState::Unlocked => {}
        }
        if require_screen_recording && !self.backend.permissions().screen_recording {
            return Err(RpcError::with_next(
                ErrorCode::PermissionDenied,
                "Screen Recording permission is not granted",
                "Grant Screen Recording to AgentDesktop.app in System Settings > Privacy & Security.",
            ));
        }
        Ok(false)
    }

    fn check_not_stopped(&self) -> Result<(), RpcError> {
        if self.backend.stop_flag().load(Ordering::Acquire) {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let lease = st.lease.take();
            drop(st);
            if let Some(lease) = lease {
                self.teardown_lease(&lease, "stopped_by_user");
            }
            self.backend.stop_flag().store(false, Ordering::Release);
            return Err(RpcError::with_next(
                ErrorCode::StoppedByUser,
                "stopped by the user (Esc or Stop button)",
                "Pause and check with the user before starting a new desktop session.",
            ));
        }
        Ok(())
    }

    fn expire_lease_if_needed(&self, st: &mut EngineState) {
        let now: DateTime<chrono::Utc> = self.clock.now();
        if st.lease.as_ref().is_some_and(|l| now >= l.info.expires_at)
            && let Some(lease) = st.lease.take()
        {
            self.teardown_lease(&lease, "lease_expired");
        }
    }

    fn teardown_lease(&self, lease: &ActiveLease, reason: &str) {
        self.backend.set_overlay(None);
        self.backend.set_display_awake(false);
        self.audit.record(AuditEvent {
            time: self.clock.now(),
            event: reason,
            caller: Some(&lease.info.owner),
            session_id: Some(&lease.info.session_id),
            detail: None,
        });
    }

    fn ensure_app_allowed(&self, app: &AppInfo, policy: &PolicyConfig) -> Result<(), RpcError> {
        let needs_confirm = check_app_policy(policy, app)?;
        if needs_confirm && policy.mode != PolicyMode::Confirm {
            // App matched user-configured `confirm_apps`.
            let ok = self.backend.confirm_dialog(
                "agent-desktop confirmation",
                &format!("Allow control of {}?", app.name),
            )?;
            if !ok {
                return Err(RpcError::new(
                    ErrorCode::PolicyDenied,
                    format!("user denied control of {}", app.name),
                ));
            }
        }
        // Also honor optional caller-requested app scope in `session_start(apps: [...])`.
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(scope) = st.lease.as_ref().and_then(|l| l.scoped_apps.as_ref())
            && !scope.iter().any(|rule| app.matches_query(rule))
        {
            return Err(RpcError::with_next(
                ErrorCode::PolicyDenied,
                format!("{} is outside this session's requested app scope", app.name),
                "Call session_start with that app included (or omit apps for unrestricted access).",
            ));
        }
        Ok(())
    }

    fn ensure_app_allowed_by_query(
        &self,
        query: &str,
        policy: &PolicyConfig,
    ) -> Result<(), RpcError> {
        if let Some(app) = self.backend.find_app(query)? {
            self.ensure_app_allowed(&app, policy)?;
        }
        Ok(())
    }

    fn capture_snapshot(
        &self,
        params: &SnapshotParams,
        policy: &PolicyConfig,
    ) -> Result<Snapshot, RpcError> {
        let snap = self.backend.snapshot(params, policy.redact_passwords)?;
        self.ensure_app_allowed(&snap.app, policy)?;
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.last_snapshot = Some(snap.clone());
        Ok(snap)
    }

    /// Capture a screenshot and return `(metadata_without_base64, optional_raw_bytes)` for
    /// zero-Base64 binary HTTP transfer (`/v1/screenshot`).
    pub fn capture_screenshot_binary(
        &self,
        client_id: Option<u64>,
        session_id: Option<&str>,
        caller: Option<&str>,
        params: &ScreenshotParams,
    ) -> Result<(ScreenshotResult, Option<Vec<u8>>), RpcError> {
        params.validate()?;
        let policy = self.policy.load()?;
        self.ensure_display_ready(true)?;
        self.ensure_active_session(client_id, session_id, caller, &policy)?;
        self.capture_screenshot_raw(params, &policy)
    }

    fn invalidate_frame_caches(&self) {
        let now = self.clock.now();
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.frame_ring.invalidate_all(now);
        }
        self.backend.invalidate_caches();
    }

    fn capture_screenshot(
        &self,
        params: &ScreenshotParams,
        policy: &PolicyConfig,
    ) -> Result<ScreenshotResult, RpcError> {
        let (mut meta, raw_bytes) = self.capture_screenshot_raw(params, policy)?;
        meta.data_base64 = raw_bytes.map(|b| BASE64.encode(&b));
        Ok(meta)
    }

    fn capture_screenshot_raw(
        &self,
        params: &ScreenshotParams,
        policy: &PolicyConfig,
    ) -> Result<(ScreenshotResult, Option<Vec<u8>>), RpcError> {
        let (annotate_boxes, snapshot_id) = if params.annotate {
            let snap_params = match &params.target {
                ScreenshotTarget::Window { window_id } => SnapshotParams {
                    window_id: Some(*window_id),
                    focus_mode: params.focus_mode,
                    ..SnapshotParams::default()
                },
                ScreenshotTarget::App { app } => SnapshotParams {
                    app: Some(app.clone()),
                    focus_mode: params.focus_mode,
                    ..SnapshotParams::default()
                },
                _ => SnapshotParams {
                    focus_mode: params.focus_mode,
                    ..SnapshotParams::default()
                },
            };
            match self.capture_snapshot(&snap_params, policy) {
                Ok(snap) => (collect_annotate_boxes(&snap, 80), Some(snap.snapshot_id)),
                Err(_) => (Vec::new(), None),
            }
        } else {
            (Vec::new(), None)
        };

        let frame_key = FrameKey::from_target(&params.target);
        let now = self.clock.now();
        // Serve unannotated screenshots from the warm frame ring if captured within 120 ms with
        // no intervening action/wait/sleep (bypass when caller explicitly polls `if_changed_since`).
        let cached = if params.if_changed_since.is_none()
            && annotate_boxes.is_empty()
            && params.focus_mode != FocusMode::Foreground
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.frame_ring
                .get_fresh(now, &frame_key, params.format, DEFAULT_WARM_FRAME_TTL_MS)
        } else {
            None
        };

        let (raw, hash) = if let Some(hit) = cached {
            (hit.raw, hit.hash)
        } else {
            let raw: RawScreenshot = self.backend.capture(
                &params.target,
                params.format,
                &annotate_boxes,
                params.focus_mode,
            )?;
            let hash = short_sha256(&raw.bytes);
            if annotate_boxes.is_empty() {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                st.frame_ring.record(
                    self.clock.now(),
                    frame_key,
                    params.format,
                    raw.clone(),
                    hash.clone(),
                );
            }
            (raw, hash)
        };

        let unchanged = params
            .if_changed_since
            .as_deref()
            .is_some_and(|prev| prev == hash);

        let display_idx = match &params.target {
            ScreenshotTarget::Display { index } => index.unwrap_or(0),
            ScreenshotTarget::Region { display, .. } => display.unwrap_or(0),
            _ => 0,
        };

        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.shots_by_display.insert(
                display_idx,
                CachedShot {
                    width: raw.width,
                    height: raw.height,
                    display_bounds: raw.display_bounds,
                },
            );
            st.last_display = Some(display_idx);
            st.last_shot_hash = Some(hash.clone());
        }

        let annotated_refs = annotate_boxes.into_iter().map(|b| b.ref_id).collect();
        let bytes_opt = if unchanged { None } else { Some(raw.bytes) };

        let meta = ScreenshotResult {
            snapshot_id,
            width: raw.width,
            height: raw.height,
            display_bounds: raw.display_bounds,
            mime_type: raw.mime_type,
            hash,
            unchanged,
            data_base64: None,
            annotated_refs,
        };
        Ok((meta, bytes_opt))
    }

    fn resolve_point(&self, target: &Target) -> Result<Point, RpcError> {
        match target {
            Target::Ref { ref_id } => {
                let resolved = self.backend.resolve_ref(ref_id)?;
                resolved
                    .bounds
                    .filter(|b| !b.is_empty())
                    .map(|b| b.center())
                    .ok_or_else(|| {
                        RpcError::with_next(
                            ErrorCode::StaleRef,
                            format!("{ref_id} has no visible bounding box"),
                            "Take a fresh snapshot or screenshot.",
                        )
                    })
            }
            Target::Point { x, y, display } => {
                let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let idx = display.or(st.last_display);
                if let Some(idx) = idx
                    && let Some(shot) = st.shots_by_display.get(&idx)
                {
                    return screenshot_to_global(
                        shot.display_bounds,
                        shot.width,
                        shot.height,
                        *x,
                        *y,
                    );
                }
                // Fallback: if coordinates are direct display points (e.g. from snapshot bounds),
                // validate finiteness and pass through directly.
                if !x.is_finite() || !y.is_finite() {
                    return Err(RpcError::new(
                        ErrorCode::InvalidInput,
                        "point coordinates must be finite numbers",
                    ));
                }
                Ok(Point { x: *x, y: *y })
            }
        }
    }

    fn run_act(
        &self,
        params: ActParams,
        policy: &PolicyConfig,
        caller: Option<&str>,
    ) -> Result<ActResult, RpcError> {
        let batch_start = Instant::now();
        if let Some(expected) = &params.expect_snapshot {
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let current = st.last_snapshot.as_ref().map(|s| s.snapshot_id.as_str());
            if current != Some(expected.as_str()) {
                return Err(RpcError::with_next(
                    ErrorCode::StaleRef,
                    format!(
                        "expected snapshot {expected:?}, but current snapshot is {:?}",
                        current.unwrap_or("none")
                    ),
                    "Take a fresh snapshot and retry with the updated refs.",
                ));
            }
        }

        let total = params.steps.len();
        let mut outcomes = Vec::with_capacity(total);
        let mut failed_error: Option<RpcError> = None;
        let mut stopped_early = false;

        for (index, step) in params.steps.into_iter().enumerate() {
            if let Err(err) = self.check_not_stopped() {
                stopped_early = true;
                failed_error = Some(err);
                break;
            }
            let step_start = Instant::now();
            let op_name = step_op_name(&step).to_string();
            match self.execute_step(step, params.focus_mode, policy) {
                Ok((detail, used_background)) => {
                    // Always invalidate warm frame caches after every step (including Wait/Sleep)
                    // so post-step or post-wait screenshots never return a pre-step cached frame.
                    self.invalidate_frame_caches();
                    outcomes.push(StepOutcome {
                        index,
                        op: op_name,
                        ok: true,
                        detail,
                        used_background,
                        duration_ms: step_start.elapsed().as_millis() as u64,
                    });
                }
                Err(err) => {
                    let is_stop = err.code == ErrorCode::StoppedByUser;
                    outcomes.push(StepOutcome {
                        index,
                        op: op_name,
                        ok: false,
                        detail: err.message.clone(),
                        used_background: false,
                        duration_ms: step_start.elapsed().as_millis() as u64,
                    });
                    stopped_early = is_stop;
                    failed_error = Some(err);
                    break;
                }
            }
        }

        let completed = outcomes.iter().filter(|s| s.ok).count();
        let mut snapshot = None;
        let mut screenshot = None;
        if failed_error.is_none() {
            match params.after {
                PostAction::None => {}
                PostAction::Snapshot => {
                    snapshot = self
                        .capture_snapshot(
                            &SnapshotParams {
                                focus_mode: params.focus_mode,
                                ..SnapshotParams::default()
                            },
                            policy,
                        )
                        .ok();
                }
                PostAction::Screenshot => {
                    screenshot = self
                        .capture_screenshot(
                            &ScreenshotParams {
                                focus_mode: params.focus_mode,
                                ..ScreenshotParams::default()
                            },
                            policy,
                        )
                        .ok();
                }
                PostAction::Annotated => {
                    screenshot = self
                        .capture_screenshot(
                            &ScreenshotParams {
                                annotate: true,
                                focus_mode: params.focus_mode,
                                ..ScreenshotParams::default()
                            },
                            policy,
                        )
                        .ok();
                }
            }
        }

        self.audit.record(AuditEvent {
            time: self.clock.now(),
            event: "act",
            caller,
            session_id: self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .lease
                .as_ref()
                .map(|l| l.info.session_id.clone())
                .as_deref(),
            detail: Some(&format!("{completed}/{total} steps")),
        });

        Ok(ActResult {
            completed,
            total,
            stopped_early,
            error: failed_error,
            duration_ms: batch_start.elapsed().as_millis() as u64,
            steps: outcomes,
            snapshot,
            screenshot,
        })
    }

    fn execute_step(
        &self,
        step: ActionStep,
        focus_mode: FocusMode,
        policy: &PolicyConfig,
    ) -> Result<(String, bool), RpcError> {
        let is_macos = self.backend.is_macos();
        match step {
            ActionStep::Click {
                target,
                button,
                count,
                modifiers,
                mode,
            } => {
                let mods = parse_modifiers(&modifiers, is_macos)?;
                match target {
                    Target::Ref { ref_id } => {
                        let resolved = self.backend.resolve_ref(&ref_id)?;
                        self.ensure_app_allowed(&resolved.app, policy)?;
                        let info = self
                            .backend
                            .click_ref(&ref_id, button, count, mods, mode, focus_mode)?;
                        Ok((info.detail, info.used_background))
                    }
                    point_target @ Target::Point { .. } => {
                        let p = self.resolve_point(&point_target)?;
                        if let Some(app) = self.backend.app_at_point(p)? {
                            self.ensure_app_allowed(&app, policy)?;
                        }
                        let info = self
                            .backend
                            .click_point(p, button, count, mods, focus_mode)?;
                        Ok((info.detail, info.used_background))
                    }
                }
            }
            ActionStep::Hover { target } => {
                let p = self.resolve_point(&target)?;
                if let Some(app) = self.backend.app_at_point(p)? {
                    self.ensure_app_allowed(&app, policy)?;
                }
                let info = self.backend.hover_point(p)?;
                Ok((info.detail, info.used_background))
            }
            ActionStep::Drag {
                from,
                to,
                modifiers,
            } => {
                let mods = parse_modifiers(&modifiers, is_macos)?;
                let p1 = self.resolve_point(&from)?;
                let p2 = self.resolve_point(&to)?;
                if let Some(app) = self.backend.app_at_point(p1)? {
                    self.ensure_app_allowed(&app, policy)?;
                }
                if let Some(app) = self.backend.app_at_point(p2)? {
                    self.ensure_app_allowed(&app, policy)?;
                }
                let info = self.backend.drag(p1, p2, mods, focus_mode)?;
                Ok((info.detail, info.used_background))
            }
            ActionStep::Scroll {
                target,
                direction,
                amount,
            } => {
                let p = self.resolve_point(&target)?;
                if let Some(app) = self.backend.app_at_point(p)? {
                    self.ensure_app_allowed(&app, policy)?;
                }
                let info = self.backend.scroll(p, direction, amount, focus_mode)?;
                Ok((info.detail, info.used_background))
            }
            ActionStep::SetText {
                ref_id,
                text,
                replace,
                submit,
                method,
            } => {
                if let Some(r) = &ref_id {
                    let resolved = self.backend.resolve_ref(r)?;
                    self.ensure_app_allowed(&resolved.app, policy)?;
                } else if let Some(app) = self.backend.focused_app()? {
                    self.ensure_app_allowed(&app, policy)?;
                }
                let info = self.backend.set_text(
                    ref_id.as_deref(),
                    &text,
                    replace,
                    submit,
                    method,
                    focus_mode,
                )?;
                Ok((info.detail, info.used_background))
            }
            ActionStep::Key { combo, repeat } => {
                let parsed = parse_key_combo(&combo, is_macos)?;
                if let Some(app) = self.backend.focused_app()? {
                    self.ensure_app_allowed(&app, policy)?;
                }
                let info = self.backend.key(parsed, repeat, focus_mode)?;
                Ok((info.detail, info.used_background))
            }
            ActionStep::OpenApp { app } => {
                self.ensure_app_allowed_by_query(&app, policy)?;
                let opened = self.backend.open_app(&app)?;
                Ok((format!("opened {}", opened.name), false))
            }
            ActionStep::FocusWindow { window_id } => {
                let win = self.backend.focus_window(window_id)?;
                self.ensure_app_allowed(&win.app, policy)?;
                Ok((
                    format!("focused window w{} ({})", win.id, win.app.name),
                    false,
                ))
            }
            ActionStep::Menu { app, path } => {
                self.ensure_app_allowed_by_query(&app, policy)?;
                let detail = self.backend.menu(&app, &path)?;
                Ok((detail, true))
            }
            ActionStep::Wait { until, timeout_ms } => {
                let (matched, elapsed, detail) = self.run_wait(&until, timeout_ms, policy)?;
                if !matched {
                    return Err(RpcError::with_next(
                        ErrorCode::Timeout,
                        format!("wait timed out after {elapsed} ms: {detail}"),
                        "Take a snapshot or screenshot to inspect current UI state.",
                    ));
                }
                Ok((detail, true))
            }
            ActionStep::Sleep { ms } => {
                let deadline = Instant::now() + StdDuration::from_millis(ms);
                while Instant::now() < deadline {
                    self.check_not_stopped()?;
                    let rem = deadline.saturating_duration_since(Instant::now());
                    std::thread::sleep(rem.min(StdDuration::from_millis(25)));
                }
                Ok((format!("slept {ms} ms"), true))
            }
        }
    }

    fn run_wait(
        &self,
        condition: &WaitCondition,
        timeout_ms: u64,
        policy: &PolicyConfig,
    ) -> Result<(bool, u64, String), RpcError> {
        let start = Instant::now();
        let deadline = start + StdDuration::from_millis(timeout_ms);

        // Capture initial hash + 8x8 visual signature for VisualChange comparisons.
        let initial_visual = if let WaitCondition::VisualChange { target } = condition {
            let raw =
                self.backend
                    .capture(target, ImageFormat::Jpeg, &[], FocusMode::Background)?;
            Some((short_sha256(&raw.bytes), raw.visual_sig))
        } else {
            None
        };

        loop {
            self.check_not_stopped()?;
            match condition {
                WaitCondition::Exists { query, role, app } => {
                    let snap = self.capture_snapshot(
                        &SnapshotParams {
                            app: app.clone(),
                            focus_mode: FocusMode::Background,
                            ..SnapshotParams::default()
                        },
                        policy,
                    )?;
                    let hits = find_in_snapshot(&snap, query, role.as_deref(), 1);
                    if let Some(hit) = hits.first() {
                        return Ok((
                            true,
                            start.elapsed().as_millis() as u64,
                            format!("found {} ({})", hit.ref_id, hit.role),
                        ));
                    }
                }
                WaitCondition::Gone { ref_id, query, app } => {
                    if let Some(r) = ref_id {
                        if self.backend.resolve_ref(r).is_err() {
                            return Ok((
                                true,
                                start.elapsed().as_millis() as u64,
                                format!("{r} is gone"),
                            ));
                        }
                    } else if let Some(q) = query {
                        let snap = self.capture_snapshot(
                            &SnapshotParams {
                                app: app.clone(),
                                focus_mode: FocusMode::Background,
                                ..SnapshotParams::default()
                            },
                            policy,
                        )?;
                        if find_in_snapshot(&snap, q, None, 1).is_empty() {
                            return Ok((
                                true,
                                start.elapsed().as_millis() as u64,
                                format!("no element matching {q:?}"),
                            ));
                        }
                    }
                }
                WaitCondition::FocusedApp { app } => {
                    if let Some(focused) = self.backend.focused_app()?
                        && focused.matches_query(app)
                    {
                        return Ok((
                            true,
                            start.elapsed().as_millis() as u64,
                            format!("{} is focused", focused.name),
                        ));
                    }
                }
                WaitCondition::WindowTitleContains { text, app } => {
                    let needle = text.to_ascii_lowercase();
                    let wins = self.backend.windows(app.as_deref())?;
                    if let Some(w) = wins
                        .iter()
                        .find(|w| w.title.to_ascii_lowercase().contains(&needle))
                    {
                        return Ok((
                            true,
                            start.elapsed().as_millis() as u64,
                            format!("window w{} matched {:?}", w.id, w.title),
                        ));
                    }
                }
                WaitCondition::VisualChange { target } => {
                    let raw = self.backend.capture(
                        target,
                        ImageFormat::Jpeg,
                        &[],
                        FocusMode::Background,
                    )?;
                    let h = short_sha256(&raw.bytes);
                    if let Some((base_sha, base_sig)) = &initial_visual
                        && has_visual_change(
                            base_sha,
                            base_sig.as_ref(),
                            &h,
                            raw.visual_sig.as_ref(),
                        )
                    {
                        return Ok((
                            true,
                            start.elapsed().as_millis() as u64,
                            format!("visual change detected ({h})"),
                        ));
                    }
                }
            }

            if Instant::now() >= deadline {
                return Ok((
                    false,
                    start.elapsed().as_millis() as u64,
                    "condition not met before timeout".into(),
                ));
            }
            std::thread::sleep(StdDuration::from_millis(40));
        }
    }
}

fn step_op_name(step: &ActionStep) -> &'static str {
    match step {
        ActionStep::Click { .. } => "click",
        ActionStep::Hover { .. } => "hover",
        ActionStep::Drag { .. } => "drag",
        ActionStep::Scroll { .. } => "scroll",
        ActionStep::SetText { .. } => "set_text",
        ActionStep::Key { .. } => "key",
        ActionStep::OpenApp { .. } => "open_app",
        ActionStep::FocusWindow { .. } => "focus_window",
        ActionStep::Menu { .. } => "menu",
        ActionStep::Wait { .. } => "wait",
        ActionStep::Sleep { .. } => "sleep",
    }
}

fn short_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(16);
    for b in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}
