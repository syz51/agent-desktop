use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use desktop_core::ResolvedRef;
use desktop_core::tree::{
    AxNodeData, RecordOutcome, SnapshotWalkBuilder, WalkBudget, is_decorative_container,
};
use desktop_proto::{
    AppInfo, ElementAction, ErrorCode, FocusMode, ReadTextMode, ReadTextResult, Rect, RpcError,
    Snapshot, SnapshotParams, UiNode, WindowInfo,
};
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};

use crate::ffi::{
    self, AXError, AXObserverRef, AXUIElementRef, CFArrayRef, CFRunLoopRef, CFStringRef, CFTypeRef,
    CGPoint, CGSize, CfOwned, K_AX_COPY_MULTIPLE_ATTRIBUTE_OPTION_NONE, K_AX_ERROR_SUCCESS,
    K_AX_VALUE_CGPOINT_TYPE, K_AX_VALUE_CGSIZE_TYPE, K_CF_NUMBER_FLOAT64_TYPE, cf_string,
    cf_to_bool, cf_to_f64, cf_to_string,
};
use crate::windows;

const DEFAULT_MAX_DEPTH: usize = 24;
const DEFAULT_MAX_NODES: usize = 250;
const SNAPSHOT_BUDGET: Duration = Duration::from_millis(250);

/// Thread-safe RAII wrapper over `AXUIElementRef`.
#[derive(Clone)]
pub struct AxElement(CfOwned);

impl AxElement {
    pub fn system_wide() -> Self {
        // SAFETY: `AXUIElementCreateSystemWide` returns an owned +1 AXUIElementRef.
        let ptr = unsafe { ffi::AXUIElementCreateSystemWide() };
        let owned = unsafe { CfOwned::from_owned(ptr.cast_const()) }
            .expect("AXUIElementCreateSystemWide returned null");
        Self(owned)
    }

    pub fn application(pid: i32) -> Self {
        // SAFETY: `AXUIElementCreateApplication` returns an owned +1 AXUIElementRef.
        let ptr = unsafe { ffi::AXUIElementCreateApplication(pid) };
        let owned = unsafe { CfOwned::from_owned(ptr.cast_const()) }
            .expect("AXUIElementCreateApplication returned null");
        let elem = Self(owned);
        elem.set_timeout(0.5);
        elem
    }

    #[must_use]
    pub fn as_raw(&self) -> AXUIElementRef {
        self.0.as_mut_ptr()
    }

    pub fn set_timeout(&self, secs: f32) {
        // SAFETY: `self.as_raw()` is a valid retained AXUIElementRef.
        unsafe {
            ffi::AXUIElementSetMessagingTimeout(self.as_raw(), secs);
        }
    }

    pub fn pid(&self) -> Option<i32> {
        let mut pid: i32 = 0;
        // SAFETY: `self.as_raw()` is valid and `pid` is a valid out pointer.
        let err = unsafe { ffi::AXUIElementGetPid(self.as_raw(), &mut pid) };
        (err == K_AX_ERROR_SUCCESS && pid > 0).then_some(pid)
    }

    pub fn copy_attr(&self, name: &str) -> Result<CfOwned, AXError> {
        let attr = cf_string(name);
        let mut value: CFTypeRef = ptr::null();
        // SAFETY: `self.as_raw()` and `attr` are valid CF refs; `value` receives a +1 ref on success.
        let err =
            unsafe { ffi::AXUIElementCopyAttributeValue(self.as_raw(), attr.as_ptr(), &mut value) };
        if err != K_AX_ERROR_SUCCESS {
            return Err(err);
        }
        unsafe { CfOwned::from_owned(value) }.ok_or(-25212)
    }

    pub fn string_attr(&self, name: &str) -> Option<String> {
        let val = self.copy_attr(name).ok()?;
        // SAFETY: `val` is a valid retained CFTypeRef.
        unsafe { cf_to_string(val.as_ptr()) }
    }

    pub fn bool_attr(&self, name: &str) -> Option<bool> {
        let val = self.copy_attr(name).ok()?;
        // SAFETY: `val` is a valid retained CFTypeRef.
        unsafe { cf_to_bool(val.as_ptr()) }
    }

    pub fn f64_attr(&self, name: &str) -> Option<f64> {
        let val = self.copy_attr(name).ok()?;
        // SAFETY: `val` is a valid retained CFTypeRef.
        unsafe { cf_to_f64(val.as_ptr()) }
    }

    pub fn set_f64_attr(&self, name: &str, value: f64) -> Result<(), AXError> {
        let attr = cf_string(name);
        // SAFETY: Creates an owned CFNumber wrapping `value`.
        let num = unsafe {
            CfOwned::from_owned(ffi::CFNumberCreate(
                ptr::null(),
                K_CF_NUMBER_FLOAT64_TYPE,
                (&value as *const f64).cast(),
            ))
        }
        .ok_or(-25200)?;
        // SAFETY: `self.as_raw()`, `attr`, and `num` are valid CF refs.
        let err = unsafe {
            ffi::AXUIElementSetAttributeValue(self.as_raw(), attr.as_ptr(), num.as_ptr())
        };
        if err == K_AX_ERROR_SUCCESS {
            Ok(())
        } else {
            Err(err)
        }
    }

    /// Fetch core snapshot attributes in a single `AXUIElementCopyMultipleAttributeValues` IPC
    /// round-trip (cutting 9-11 serial Mach IPCs per element down to 1), falling back to
    /// individual reads if the target app rejects batch queries.
    pub fn read_node_attrs(&self, include_bounds: bool) -> BatchedNodeAttrs {
        let keys = cached_snapshot_attr_keys(include_bounds);
        let mut raw_values: CFArrayRef = ptr::null();
        // SAFETY: `self.as_raw()` and `keys` are valid retained CF refs; `raw_values` receives
        // an owned +1 CFArray on `K_AX_ERROR_SUCCESS`.
        let err = unsafe {
            ffi::AXUIElementCopyMultipleAttributeValues(
                self.as_raw(),
                keys.as_ptr(),
                K_AX_COPY_MULTIPLE_ATTRIBUTE_OPTION_NONE,
                &mut raw_values,
            )
        };
        if err == K_AX_ERROR_SUCCESS
            && let Some(arr) = (unsafe { CfOwned::from_owned(raw_values) })
        {
            let expected = if include_bounds { 11 } else { 9 };
            // SAFETY: `arr` is an owned CFArray returned by `AXUIElementCopyMultipleAttributeValues`.
            let count = unsafe { ffi::CFArrayGetCount(arr.as_ptr()) };
            if count >= expected {
                // SAFETY: Indices `0..expected` are in bounds; `cf_to_*` and `unpack_ax_bounds`
                // validate CFTypeIDs so `kAXValueAXErrorType` placeholders cleanly yield `None`.
                unsafe {
                    let get = |i: isize| ffi::CFArrayGetValueAtIndex(arr.as_ptr(), i);
                    let raw_role = cf_to_string(get(0)).unwrap_or_default();
                    let raw_subrole = cf_to_string(get(1));
                    let title = cf_to_string(get(2)).filter(|s| !s.trim().is_empty());
                    let label = cf_to_string(get(3))
                        .or_else(|| cf_to_string(get(4)))
                        .filter(|s| !s.trim().is_empty());
                    let value = cf_to_string(get(5)).filter(|s| !s.is_empty());
                    let focused = cf_to_bool(get(6)).unwrap_or(false);
                    let selected = cf_to_bool(get(7)).unwrap_or(false);
                    let enabled = cf_to_bool(get(8)).unwrap_or(true);
                    let bounds = if include_bounds {
                        unpack_ax_bounds(get(9), get(10))
                    } else {
                        None
                    };
                    return BatchedNodeAttrs {
                        raw_role,
                        raw_subrole,
                        title,
                        label,
                        value,
                        focused,
                        selected,
                        enabled,
                        bounds,
                    };
                }
            }
        }

        // Fallback for apps that do not support `AXUIElementCopyMultipleAttributeValues`.
        let raw_role = self.string_attr("AXRole").unwrap_or_default();
        let raw_subrole = self.string_attr("AXSubrole");
        let title = self.string_attr("AXTitle").filter(|s| !s.trim().is_empty());
        let label = self
            .string_attr("AXDescription")
            .or_else(|| self.string_attr("AXHelp"))
            .filter(|s| !s.trim().is_empty());
        let value = self.string_attr("AXValue").filter(|s| !s.is_empty());
        let focused = self.bool_attr("AXFocused").unwrap_or(false);
        let selected = self.bool_attr("AXSelected").unwrap_or(false);
        let enabled = self.bool_attr("AXEnabled").unwrap_or(true);
        let bounds = if include_bounds { self.bounds() } else { None };
        BatchedNodeAttrs {
            raw_role,
            raw_subrole,
            title,
            label,
            value,
            focused,
            selected,
            enabled,
            bounds,
        }
    }

    pub fn element_attr(&self, name: &str) -> Option<AxElement> {
        let val = self.copy_attr(name).ok()?;
        // SAFETY: Verify CFTypeID matches AXUIElement before wrapping.
        if unsafe { ffi::CFGetTypeID(val.as_ptr()) == ffi::AXUIElementGetTypeID() } {
            Some(AxElement(val))
        } else {
            None
        }
    }

    pub fn element_array_attr(&self, name: &str) -> Vec<AxElement> {
        let Ok(arr) = self.copy_attr(name) else {
            return Vec::new();
        };
        // SAFETY: Verify CFArray type before iterating elements.
        unsafe {
            if ffi::CFGetTypeID(arr.as_ptr()) != ffi::CFArrayGetTypeID() {
                return Vec::new();
            }
            let count = ffi::CFArrayGetCount(arr.as_ptr());
            let ax_type = ffi::AXUIElementGetTypeID();
            let mut out = Vec::with_capacity(count.max(0) as usize);
            for i in 0..count {
                let item = ffi::CFArrayGetValueAtIndex(arr.as_ptr(), i);
                if !item.is_null()
                    && ffi::CFGetTypeID(item) == ax_type
                    && let Some(owned) = CfOwned::from_borrowed(item)
                {
                    out.push(AxElement(owned));
                }
            }
            out
        }
    }

    pub fn bounds(&self) -> Option<Rect> {
        let pos_val = self.copy_attr("AXPosition").ok()?;
        let size_val = self.copy_attr("AXSize").ok()?;
        let mut pt = CGPoint::default();
        let mut sz = CGSize::default();
        // SAFETY: Verify both values are AXValueRef before unpacking CGPoint/CGSize.
        unsafe {
            let ax_val_ty = ffi::AXValueGetTypeID();
            if ffi::CFGetTypeID(pos_val.as_ptr()) != ax_val_ty
                || ffi::CFGetTypeID(size_val.as_ptr()) != ax_val_ty
            {
                return None;
            }
            if !ffi::AXValueGetValue(
                pos_val.as_ptr(),
                K_AX_VALUE_CGPOINT_TYPE,
                (&mut pt as *mut CGPoint).cast(),
            ) || !ffi::AXValueGetValue(
                size_val.as_ptr(),
                K_AX_VALUE_CGSIZE_TYPE,
                (&mut sz as *mut CGSize).cast(),
            ) {
                return None;
            }
        }
        Some(Rect {
            x: pt.x,
            y: pt.y,
            width: sz.width,
            height: sz.height,
        })
    }

    pub fn action_names(&self) -> Vec<String> {
        let mut arr_raw: CFArrayRef = ptr::null();
        // SAFETY: `arr_raw` receives an owned CFArray of CFString action names.
        let err = unsafe { ffi::AXUIElementCopyActionNames(self.as_raw(), &mut arr_raw) };
        if err != K_AX_ERROR_SUCCESS {
            return Vec::new();
        }
        let Some(arr) = (unsafe { CfOwned::from_owned(arr_raw) }) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        // SAFETY: `arr` is a valid CFArrayRef.
        unsafe {
            let count = ffi::CFArrayGetCount(arr.as_ptr());
            for i in 0..count {
                let item = ffi::CFArrayGetValueAtIndex(arr.as_ptr(), i);
                if let Some(name) = cf_to_string(item) {
                    out.push(name);
                }
            }
        }
        out
    }

    pub fn perform_action(&self, action: &str) -> Result<(), AXError> {
        let act = cf_string(action);
        // SAFETY: `self.as_raw()` and `act` are valid CF refs.
        let err = unsafe { ffi::AXUIElementPerformAction(self.as_raw(), act.as_ptr()) };
        if err == K_AX_ERROR_SUCCESS {
            Ok(())
        } else {
            Err(err)
        }
    }

    pub fn set_string_attr(&self, name: &str, value: &str) -> Result<(), AXError> {
        let attr = cf_string(name);
        let val = cf_string(value);
        // SAFETY: `self.as_raw()`, `attr`, and `val` are valid CF refs.
        let err = unsafe {
            ffi::AXUIElementSetAttributeValue(self.as_raw(), attr.as_ptr(), val.as_ptr())
        };
        if err == K_AX_ERROR_SUCCESS {
            Ok(())
        } else {
            Err(err)
        }
    }

    pub fn set_bool_true(&self, name: &str) -> Result<(), AXError> {
        let attr = cf_string(name);
        // SAFETY: `kCFBooleanTrue` is a static CoreFoundation singleton.
        let err = unsafe {
            ffi::AXUIElementSetAttributeValue(self.as_raw(), attr.as_ptr(), ffi::kCFBooleanTrue)
        };
        if err == K_AX_ERROR_SUCCESS {
            Ok(())
        } else {
            Err(err)
        }
    }
}

pub struct BatchedNodeAttrs {
    pub raw_role: String,
    pub raw_subrole: Option<String>,
    pub title: Option<String>,
    pub label: Option<String>,
    pub value: Option<String>,
    pub focused: bool,
    pub selected: bool,
    pub enabled: bool,
    pub bounds: Option<Rect>,
}

fn build_cf_string_array(names: &[&str]) -> CfOwned {
    let owned: Vec<CfOwned> = names.iter().map(|s| cf_string(s)).collect();
    let ptrs: Vec<CFTypeRef> = owned.iter().map(CfOwned::as_ptr).collect();
    // SAFETY: `ptrs` holds `ptrs.len()` valid CFStringRef pointers; `kCFTypeArrayCallBacks`
    // retains each entry inside the newly created CFArray.
    let arr = unsafe {
        ffi::CFArrayCreate(
            ptr::null(),
            ptrs.as_ptr(),
            ptrs.len() as ffi::CFIndex,
            &ffi::kCFTypeArrayCallBacks,
        )
    };
    // SAFETY: `arr` is a newly created +1 CFArrayRef.
    unsafe { CfOwned::from_owned(arr) }.expect("CFArrayCreate failed")
}

fn cached_snapshot_attr_keys(include_bounds: bool) -> &'static CfOwned {
    static NO_BOUNDS: OnceLock<CfOwned> = OnceLock::new();
    static WITH_BOUNDS: OnceLock<CfOwned> = OnceLock::new();
    if include_bounds {
        WITH_BOUNDS.get_or_init(|| {
            build_cf_string_array(&[
                "AXRole",
                "AXSubrole",
                "AXTitle",
                "AXDescription",
                "AXHelp",
                "AXValue",
                "AXFocused",
                "AXSelected",
                "AXEnabled",
                "AXPosition",
                "AXSize",
            ])
        })
    } else {
        NO_BOUNDS.get_or_init(|| {
            build_cf_string_array(&[
                "AXRole",
                "AXSubrole",
                "AXTitle",
                "AXDescription",
                "AXHelp",
                "AXValue",
                "AXFocused",
                "AXSelected",
                "AXEnabled",
            ])
        })
    }
}

/// Unpack a borrowed `(AXPosition, AXSize)` pair into `Rect` when both are valid `AXValueRef`s.
///
/// # Safety
/// `pos_val` and `size_val` must be null or valid CFTypeRefs.
unsafe fn unpack_ax_bounds(pos_val: CFTypeRef, size_val: CFTypeRef) -> Option<Rect> {
    if pos_val.is_null() || size_val.is_null() {
        return None;
    }
    let mut pt = CGPoint::default();
    let mut sz = CGSize::default();
    // SAFETY: Verify both values are AXValueRef before unpacking CGPoint/CGSize.
    unsafe {
        let ax_val_ty = ffi::AXValueGetTypeID();
        if ffi::CFGetTypeID(pos_val) != ax_val_ty || ffi::CFGetTypeID(size_val) != ax_val_ty {
            return None;
        }
        if !ffi::AXValueGetValue(
            pos_val,
            K_AX_VALUE_CGPOINT_TYPE,
            (&mut pt as *mut CGPoint).cast(),
        ) || !ffi::AXValueGetValue(
            size_val,
            K_AX_VALUE_CGSIZE_TYPE,
            (&mut sz as *mut CGSize).cast(),
        ) {
            return None;
        }
    }
    Some(Rect {
        x: pt.x,
        y: pt.y,
        width: sz.width,
        height: sz.height,
    })
}

/// Event-driven wait for an accessibility notification on `pid` (or the frontmost app),
/// returning immediately when a UI change fires or once `timeout` / `stop` expires.
pub fn wait_for_ui_event(pid: Option<u32>, timeout: Duration, stop: &AtomicBool) {
    if timeout.is_zero() || stop.load(Ordering::Acquire) {
        return;
    }
    if !crate::power::permissions().accessibility {
        std::thread::sleep(timeout.min(Duration::from_millis(40)));
        return;
    }
    let Some(target_pid) = pid
        .and_then(|p| i32::try_from(p).ok())
        .filter(|&p| p > 0)
        .or_else(windows::frontmost_pid)
    else {
        std::thread::sleep(timeout.min(Duration::from_millis(40)));
        return;
    };

    struct ObserverCtx {
        run_loop: CFRunLoopRef,
        triggered: AtomicBool,
    }

    unsafe extern "C" fn on_ax_event(
        _observer: AXObserverRef,
        _element: AXUIElementRef,
        _notification: CFStringRef,
        refcon: *mut c_void,
    ) {
        if refcon.is_null() {
            return;
        }
        // SAFETY: `refcon` points to the stack-pinned `ObserverCtx` for the duration of
        // `CFRunLoopRunInMode` on this exact thread; source is removed before `ctx` drops.
        let ctx = unsafe { &*(refcon.cast::<ObserverCtx>()) };
        ctx.triggered.store(true, Ordering::Release);
        unsafe {
            ffi::CFRunLoopStop(ctx.run_loop);
        }
    }

    let mut raw_obs: AXObserverRef = ptr::null_mut();
    // SAFETY: `on_ax_event` matches `AXObserverCallback`; `raw_obs` receives a +1 AXObserverRef.
    let err = unsafe { ffi::AXObserverCreate(target_pid, on_ax_event, &mut raw_obs) };
    let Some(observer) = (err == K_AX_ERROR_SUCCESS)
        .then(|| unsafe { CfOwned::from_owned(raw_obs.cast_const()) })
        .flatten()
    else {
        std::thread::sleep(timeout.min(Duration::from_millis(40)));
        return;
    };

    // SAFETY: `CFRunLoopGetCurrent()` returns the current thread's non-null run loop.
    let run_loop = unsafe { ffi::CFRunLoopGetCurrent() };
    let mut ctx = ObserverCtx {
        run_loop,
        triggered: AtomicBool::new(false),
    };
    let refcon = (&mut ctx as *mut ObserverCtx).cast::<c_void>();

    let app_el = AxElement::application(target_pid);
    const NOTIFICATIONS: &[&str] = &[
        "AXCreated",
        "AXUIElementDestroyed",
        "AXTitleChanged",
        "AXValueChanged",
        "AXFocusedWindowChanged",
        "AXFocusedUIElementChanged",
        "AXWindowCreated",
        "AXLayoutChanged",
        "AXApplicationActivated",
        "AXApplicationDeactivated",
    ];
    let mut added: Vec<CfOwned> = Vec::with_capacity(NOTIFICATIONS.len());
    for &name in NOTIFICATIONS {
        let cf_name = cf_string(name);
        // SAFETY: `observer`, `app_el`, `cf_name`, and `refcon` are valid for the loop below.
        let rc = unsafe {
            ffi::AXObserverAddNotification(
                observer.as_mut_ptr(),
                app_el.as_raw(),
                cf_name.as_ptr(),
                refcon,
            )
        };
        if rc == K_AX_ERROR_SUCCESS {
            added.push(cf_name);
        }
    }

    if added.is_empty() {
        std::thread::sleep(timeout.min(Duration::from_millis(40)));
        return;
    }

    // SAFETY: `observer` is valid and `source` is borrowed from it until `observer` drops.
    let source = unsafe { ffi::AXObserverGetRunLoopSource(observer.as_mut_ptr()) };
    if !source.is_null() {
        // SAFETY: Attach source to current thread's run loop, run bounded stop-checkable slices,
        // and always detach before returning so `refcon` cannot dangle.
        unsafe {
            ffi::CFRunLoopAddSource(run_loop, source, ffi::kCFRunLoopDefaultMode);
        }
        let deadline = Instant::now() + timeout;
        while !ctx.triggered.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let slice = deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(50));
            // SAFETY: Runs current thread's run loop in default mode, returning immediately when
            // `on_ax_event` fires or `slice` elapses.
            unsafe {
                ffi::CFRunLoopRunInMode(ffi::kCFRunLoopDefaultMode, slice.as_secs_f64(), 1);
            }
        }
        // SAFETY: Detach source before removing notifications and dropping `ctx`.
        unsafe {
            ffi::CFRunLoopRemoveSource(run_loop, source, ffi::kCFRunLoopDefaultMode);
        }
    }

    for cf_name in &added {
        // SAFETY: Matches every notification successfully registered above.
        unsafe {
            ffi::AXObserverRemoveNotification(
                observer.as_mut_ptr(),
                app_el.as_raw(),
                cf_name.as_ptr(),
            );
        }
    }
}

#[derive(Clone)]
pub struct LiveRef {
    pub element: AxElement,
    pub app: AppInfo,
    pub window_id: Option<u32>,
    pub actions: Vec<ElementAction>,
}

/// Live store mapping `@eN` refs from the latest snapshot to retained `AXUIElement` handles.
pub struct AxRefStore {
    seq: AtomicU64,
    refs: Mutex<HashMap<String, LiveRef>>,
    /// PIDs for which we have already enabled `AXManualAccessibility` (Chromium/Electron).
    enabled_manual_ax: Mutex<HashSet<i32>>,
}

impl AxRefStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            refs: Mutex::new(HashMap::new()),
            enabled_manual_ax: Mutex::new(HashSet::new()),
        }
    }

    pub fn mark_manual_ax_enabled(&self, pid: i32) -> bool {
        self.enabled_manual_ax
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(pid)
    }

    pub fn next_snapshot_id(&self) -> String {
        let n = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        format!("s{n}")
    }

    pub fn replace_all(&self, map: HashMap<String, LiveRef>) {
        *self.refs.lock().unwrap_or_else(|e| e.into_inner()) = map;
    }

    pub fn get(&self, ref_id: &str) -> Result<LiveRef, RpcError> {
        let guard = self.refs.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(ref_id).cloned().ok_or_else(|| {
            RpcError::with_next(
                ErrorCode::StaleRef,
                format!("unknown or expired element ref {ref_id:?}"),
                "Take a fresh snapshot or find call to obtain current @eN refs.",
            )
        })
    }

    pub fn resolve(&self, ref_id: &str) -> Result<ResolvedRef, RpcError> {
        let live = self.get(ref_id)?;
        // Probe live PID or role to confirm the underlying AX element is still alive.
        let alive = live.element.pid().is_some() || live.element.string_attr("AXRole").is_some();
        if !alive {
            return Err(RpcError::with_next(
                ErrorCode::StaleRef,
                format!("element {ref_id} is no longer valid in {}", live.app.name),
                "Take a fresh snapshot to inspect updated UI elements.",
            ));
        }
        let bounds = live.element.bounds();
        Ok(ResolvedRef {
            ref_id: ref_id.to_string(),
            app: live.app,
            window_id: live.window_id,
            bounds,
            actions: live.actions,
        })
    }
}

pub fn require_accessibility() -> Result<(), RpcError> {
    if !crate::power::permissions().accessibility {
        return Err(RpcError::with_next(
            ErrorCode::PermissionDenied,
            "Accessibility permission is not granted",
            "Grant Accessibility to AgentDesktop.app (or the host app running desktop) in System Settings > Privacy & Security.",
        ));
    }
    Ok(())
}

pub fn focused_app() -> Result<Option<AppInfo>, RpcError> {
    require_accessibility()?;
    let sys = AxElement::system_wide();
    sys.set_timeout(0.5);
    if let Some(app_el) = sys.element_attr("AXFocusedApplication")
        && let Some(pid) = app_el.pid()
        && let Some(app) = windows::running_app_for_pid(pid)
    {
        return Ok(Some(app));
    }
    Ok(windows::frontmost_app())
}

pub fn raise_window_by_title(pid: i32, title: &str) -> bool {
    if pid <= 0 {
        return false;
    }
    let app = AxElement::application(pid);
    for win in app.element_array_attr("AXWindows") {
        let win_title = win.string_attr("AXTitle").unwrap_or_default();
        if title.is_empty() || win_title == title {
            let _ = win.perform_action("AXRaise");
            let _ = win.set_bool_true("AXMain");
            return true;
        }
    }
    false
}

/// Take a fast, bounded Accessibility tree snapshot of a target window or app.
pub fn take_snapshot(
    store: &AxRefStore,
    params: &SnapshotParams,
    redact_passwords: bool,
) -> Result<Snapshot, RpcError> {
    require_accessibility()?;
    let start = Instant::now();

    // Optional subtree drilldown from an existing @eN ref.
    if let Some(root_ref) = &params.root_ref {
        let live = store.get(root_ref)?;
        return build_snapshot_from_root(
            store,
            live.element,
            live.app,
            params.window_id.or(live.window_id),
            None,
            params,
            redact_passwords,
            start,
        );
    }

    // Resolve target app + window.
    let (app, window_id, window_title, target_win_bounds) = if let Some(wid) = params.window_id {
        let all = windows::windows(None)?;
        let w = all.into_iter().find(|w| w.id == wid).ok_or_else(|| {
            RpcError::with_next(
                ErrorCode::NotFound,
                format!("window w{wid} not found"),
                "Call list_windows to see active window IDs.",
            )
        })?;
        if params.focus_mode == FocusMode::Foreground
            && let Some(pid) = w.app.pid
        {
            windows::activate_pid(pid as i32);
        }
        (w.app, Some(w.id), Some(w.title), Some(w.bounds))
    } else if let Some(app_query) = &params.app {
        let found = windows::find_app(app_query).ok_or_else(|| {
            RpcError::with_next(
                ErrorCode::NotFound,
                format!("app {app_query:?} not found"),
                "Pass a running app name or bundle ID, or open it first.",
            )
        })?;
        let pid = found.pid.ok_or_else(|| {
            RpcError::with_next(
                ErrorCode::NotFound,
                format!("{} is not running", found.name),
                "Open the app first with open_app.",
            )
        })?;
        if params.focus_mode == FocusMode::Foreground {
            windows::activate_pid(pid as i32);
        }
        let win = resolve_window_for_app(&found);
        (
            found,
            win.as_ref().map(|w| w.id),
            win.as_ref().map(|w| w.title.clone()),
            win.as_ref().map(|w| w.bounds),
        )
    } else {
        let app = focused_app()?
            .ok_or_else(|| RpcError::new(ErrorCode::NotFound, "no focused application found"))?;
        let win = resolve_window_for_app(&app);
        (
            app,
            win.as_ref().map(|w| w.id),
            win.as_ref().map(|w| w.title.clone()),
            win.as_ref().map(|w| w.bounds),
        )
    };

    let pid = app.pid.ok_or_else(|| {
        RpcError::new(
            ErrorCode::NotFound,
            format!("{} has no active PID", app.name),
        )
    })? as i32;
    let app_el = AxElement::application(pid);

    // Enable manual accessibility for Chromium/Electron apps (safe no-op for native AppKit apps).
    // On the first activation for a PID, poll briefly until Chromium populates its renderer tree
    // (exiting early as soon as content appears instead of returning an empty chrome shell).
    let manual_ok = app_el.set_bool_true("AXManualAccessibility").is_ok();
    let _ = app_el.set_bool_true("AXEnhancedUserInterface");
    if manual_ok && store.mark_manual_ax_enabled(pid) {
        wait_for_electron_ax_ready(&app_el, window_title.as_deref(), Duration::from_millis(260));
    }

    // Pick the best AX window: match by title if specified, else focused/main/first window.
    let root_el = pick_ax_window(&app_el, window_title.as_deref()).unwrap_or(app_el);
    let actual_title = root_el
        .string_attr("AXTitle")
        .filter(|s| !s.is_empty())
        .or(window_title);
    let actual_bounds = root_el.bounds().or(target_win_bounds);

    build_snapshot_from_root(
        store,
        root_el,
        app,
        window_id,
        actual_title,
        params,
        redact_passwords,
        start,
    )
    .map(|mut s| {
        if s.window_bounds.is_none() {
            s.window_bounds = actual_bounds;
        }
        s
    })
}

fn resolve_window_for_app(app: &AppInfo) -> Option<WindowInfo> {
    let filter = app
        .bundle_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&app.name);
    let wins = windows::windows(Some(filter)).ok()?;
    pick_primary_window(&wins, app.pid).cloned()
}

fn pick_primary_window(wins: &[WindowInfo], pid: Option<u32>) -> Option<&WindowInfo> {
    if let Some(pid) = pid {
        if let Some(w) = wins.iter().find(|w| w.app.pid == Some(pid) && w.layer == 0) {
            return Some(w);
        }
        if let Some(w) = wins.iter().find(|w| w.app.pid == Some(pid)) {
            return Some(w);
        }
    }
    wins.iter().find(|w| w.layer == 0).or_else(|| wins.first())
}

fn wait_for_electron_ax_ready(app_el: &AxElement, title: Option<&str>, max_wait: Duration) {
    let deadline = Instant::now() + max_wait;
    loop {
        if let Some(win) = pick_ax_window(app_el, title)
            && window_has_populated_content(&win)
        {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let step = deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(25));
        std::thread::sleep(step);
    }
}

/// Returns true once a window's AX tree has populated beyond bare window-chrome buttons
/// (e.g., an `AXWebArea` or populated container subtree inside an Electron/Chromium window).
fn window_has_populated_content(win: &AxElement) -> bool {
    let children = win.element_array_attr("AXChildren");
    if children.len() > 6 {
        return true;
    }
    for child in &children {
        let role = child.string_attr("AXRole").unwrap_or_default();
        if role == "AXWebArea" {
            return true;
        }
        if matches!(role.as_str(), "AXGroup" | "AXSplitGroup" | "AXScrollArea") {
            let grand = child.element_array_attr("AXChildren");
            if !grand.is_empty() {
                return true;
            }
        }
    }
    false
}

fn pick_ax_window(app_el: &AxElement, title: Option<&str>) -> Option<AxElement> {
    if let Some(t) = title
        && !t.is_empty()
    {
        for w in app_el.element_array_attr("AXWindows") {
            if w.string_attr("AXTitle").as_deref() == Some(t) {
                return Some(w);
            }
        }
    }
    app_el
        .element_attr("AXFocusedWindow")
        .or_else(|| app_el.element_attr("AXMainWindow"))
        .or_else(|| app_el.element_array_attr("AXWindows").into_iter().next())
}

#[allow(clippy::too_many_arguments)]
fn build_snapshot_from_root(
    store: &AxRefStore,
    root_el: AxElement,
    app: AppInfo,
    window_id: Option<u32>,
    window_title: Option<String>,
    params: &SnapshotParams,
    redact_passwords: bool,
    start: Instant,
) -> Result<Snapshot, RpcError> {
    let max_depth = params.max_depth.unwrap_or(DEFAULT_MAX_DEPTH);
    let max_nodes = params.max_nodes.unwrap_or(DEFAULT_MAX_NODES);
    let deadline = start + SNAPSHOT_BUDGET;

    let root_bounds = root_el.bounds();
    let fallback_root = UiNode {
        ref_id: "@e1".into(),
        role: "window".into(),
        subrole: None,
        title: window_title.clone(),
        label: None,
        value: None,
        bounds: root_bounds,
        enabled: true,
        focused: false,
        selected: false,
        actions: vec![],
        children: vec![],
    };

    let mut builder = SnapshotWalkBuilder::new(root_el, WalkBudget::new(max_depth, max_nodes));
    let fetch_bounds = params.include_bounds;

    while let Some(pending) = builder.next_pending() {
        if Instant::now() >= deadline {
            builder.mark_truncated();
            break;
        }
        let (el, meta) = pending.into_parts();
        let depth = meta.depth();

        // Single batched IPC call for all scalar attributes (skipping AXPosition/AXSize unless requested).
        let attrs = el.read_node_attrs(fetch_bounds);
        let role = normalize_role(&attrs.raw_role);
        let subrole = attrs
            .raw_subrole
            .as_deref()
            .map(normalize_subrole)
            .filter(|s| !s.is_empty());

        let is_secure = attrs.raw_subrole.as_deref() == Some("AXSecureTextField")
            || attrs.raw_role == "AXSecureTextField";
        let value = if is_secure && redact_passwords {
            Some("[REDACTED]".into())
        } else {
            attrs.value
        };

        let actions = map_actions(&el.action_names(), &role);

        let decorative = is_decorative_container(
            depth,
            &role,
            attrs.title.as_deref(),
            attrs.label.as_deref(),
            value.as_deref(),
            attrs.focused,
            attrs.selected,
            &actions,
        );

        let (bounds, enabled, live_ref) = if decorative {
            (None, true, None)
        } else {
            let bounds = if fetch_bounds {
                if depth == 0 {
                    root_bounds.or(attrs.bounds)
                } else {
                    attrs.bounds
                }
            } else if depth == 0 {
                root_bounds
            } else {
                None
            };
            let live = LiveRef {
                element: el.clone(),
                app: app.clone(),
                window_id,
                actions: actions.clone(),
            };
            (bounds, attrs.enabled, Some(live))
        };

        let data = AxNodeData {
            role,
            subrole,
            title: attrs.title,
            label: attrs.label,
            value,
            bounds,
            enabled,
            focused: attrs.focused,
            selected: attrs.selected,
            actions,
        };

        match builder.record_node(meta, data, live_ref) {
            RecordOutcome::Expand(node_idx) => {
                let raw_children = el.element_array_attr("AXChildren");
                builder.enqueue_children(node_idx, depth + 1, raw_children);
            }
            RecordOutcome::CheckChildrenForTruncation => {
                if !el.element_array_attr("AXChildren").is_empty() {
                    builder.mark_truncated();
                }
            }
            RecordOutcome::Done => {}
        }
    }

    let output = builder.finish(fallback_root);
    store.replace_all(output.refs);

    Ok(Snapshot {
        snapshot_id: store.next_snapshot_id(),
        app,
        window_id,
        window_title,
        window_bounds: root_bounds,
        truncated: output.truncated,
        total_nodes: output.total_visited,
        emitted_nodes: output.emitted_nodes,
        elapsed_ms: start.elapsed().as_millis() as u64,
        root: output.root,
    })
}

fn normalize_role(raw: &str) -> String {
    match raw {
        "AXWindow" => "window",
        "AXApplication" => "application",
        "AXButton" => "button",
        "AXTextField" | "AXSearchField" | "AXSecureTextField" => "textfield",
        "AXTextArea" => "textarea",
        "AXStaticText" => "text",
        "AXCheckBox" => "checkbox",
        "AXRadioButton" => "radio",
        "AXPopUpButton" | "AXComboBox" | "AXMenuButton" => "popup",
        "AXTabGroup" => "tabgroup",
        "AXTab" => "tab",
        "AXToolbar" => "toolbar",
        "AXMenuBar" => "menubar",
        "AXMenu" => "menu",
        "AXMenuItem" | "AXMenuBarItem" => "menuitem",
        "AXList" => "list",
        "AXOutline" => "tree",
        "AXTable" => "table",
        "AXRow" => "row",
        "AXCell" => "cell",
        "AXLink" => "link",
        "AXImage" => "image",
        "AXSlider" => "slider",
        "AXSheet" | "AXDialog" => "dialog",
        "AXScrollArea" => "scrollarea",
        "AXSplitGroup" => "splitgroup",
        "AXWebArea" => "webarea",
        "AXGroup" => "group",
        "" => "unknown",
        other => other.strip_prefix("AX").unwrap_or(other),
    }
    .to_ascii_lowercase()
}

fn normalize_subrole(raw: &str) -> String {
    match raw {
        "AXSecureTextField" => "secure".into(),
        "AXSearchField" => "search".into(),
        "AXCloseButton" => "close".into(),
        "AXMinimizeButton" => "minimize".into(),
        "AXFullScreenButton" => "fullscreen".into(),
        "AXTabButton" => "tab".into(),
        _ => String::new(),
    }
}

fn map_actions(raw: &[String], role: &str) -> Vec<ElementAction> {
    let mut out = Vec::new();
    for a in raw {
        let mapped = match a.as_str() {
            "AXPress" => Some(ElementAction::Press),
            "AXConfirm" => Some(ElementAction::Confirm),
            "AXShowMenu" => Some(ElementAction::ShowMenu),
            "AXRaise" => Some(ElementAction::Raise),
            "AXIncrement" => Some(ElementAction::Increment),
            "AXDecrement" => Some(ElementAction::Decrement),
            _ => None,
        };
        if let Some(m) = mapped
            && !out.contains(&m)
        {
            out.push(m);
        }
    }
    if matches!(role, "textfield" | "textarea") && !out.contains(&ElementAction::SetValue) {
        out.push(ElementAction::SetValue);
    }
    out
}

/// Invoke a native macOS app menu-bar item directly via AX without visual menu animation.
pub fn invoke_menu(app_query: &str, path: &[String]) -> Result<String, RpcError> {
    require_accessibility()?;
    let app = windows::find_app(app_query).ok_or_else(|| {
        RpcError::new(ErrorCode::NotFound, format!("app {app_query:?} not found"))
    })?;
    let pid = app
        .pid
        .ok_or_else(|| RpcError::new(ErrorCode::NotFound, format!("{} is not running", app.name)))?
        as i32;
    let app_el = AxElement::application(pid);
    let menu_bar = app_el.element_attr("AXMenuBar").ok_or_else(|| {
        RpcError::new(
            ErrorCode::NotFound,
            format!("{} exposes no AXMenuBar", app.name),
        )
    })?;

    let mut current_container = menu_bar;
    let mut last_item: Option<AxElement> = None;

    for (idx, segment) in path.iter().enumerate() {
        let wanted = segment.trim().to_ascii_lowercase();
        let children = current_container.element_array_attr("AXChildren");
        let found = children.into_iter().find(|child| {
            child
                .string_attr("AXTitle")
                .is_some_and(|t| t.trim().to_ascii_lowercase() == wanted)
        });
        let item = found.ok_or_else(|| {
            RpcError::with_next(
                ErrorCode::NotFound,
                format!("menu item {segment:?} not found in {}", app.name),
                "Check the exact menu bar item title (e.g. [\"File\", \"New Window\"]).",
            )
        })?;
        if idx + 1 < path.len() {
            // Descend into the submenu AXMenu child.
            let sub = item
                .element_array_attr("AXChildren")
                .into_iter()
                .find(|c| c.string_attr("AXRole").as_deref() == Some("AXMenu"))
                .ok_or_else(|| {
                    RpcError::new(
                        ErrorCode::NotFound,
                        format!("menu item {segment:?} has no submenu"),
                    )
                })?;
            current_container = sub;
        }
        last_item = Some(item);
    }

    let target = last_item.ok_or_else(|| RpcError::new(ErrorCode::InvalidInput, "empty path"))?;
    target.perform_action("AXPress").map_err(|err| {
        RpcError::new(
            ErrorCode::BackendError,
            format!("AXPress on menu item failed (AXError {err})"),
        )
    })?;
    Ok(format!("invoked {} in {}", path.join(" > "), app.name))
}

/// Read text directly from an AX element, focused selection, or system clipboard.
pub fn read_text(
    store: &AxRefStore,
    ref_id: Option<&str>,
    _window_id: Option<u32>,
    mode: ReadTextMode,
    redact_passwords: bool,
) -> Result<ReadTextResult, RpcError> {
    match mode {
        ReadTextMode::Clipboard => {
            let pb = NSPasteboard::generalPasteboard();
            // SAFETY: `NSPasteboardTypeString` is a static AppKit pasteboard type constant.
            let text = unsafe { pb.stringForType(NSPasteboardTypeString) }
                .map(|s| s.to_string())
                .unwrap_or_default();
            Ok(ReadTextResult {
                text,
                source: "clipboard".into(),
            })
        }
        ReadTextMode::Selection => {
            require_accessibility()?;
            let sys = AxElement::system_wide();
            sys.set_timeout(0.5);
            let el = if let Some(r) = ref_id {
                store.get(r)?.element
            } else {
                sys.element_attr("AXFocusedUIElement").ok_or_else(|| {
                    RpcError::new(ErrorCode::NotFound, "no focused UI element found")
                })?
            };
            let text = el.string_attr("AXSelectedText").unwrap_or_default();
            Ok(ReadTextResult {
                text,
                source: ref_id.unwrap_or("focused").into(),
            })
        }
        ReadTextMode::Value => {
            require_accessibility()?;
            let el = if let Some(r) = ref_id {
                store.get(r)?.element
            } else {
                let sys = AxElement::system_wide();
                sys.set_timeout(0.5);
                sys.element_attr("AXFocusedUIElement").ok_or_else(|| {
                    RpcError::new(ErrorCode::NotFound, "no focused UI element found")
                })?
            };
            let is_secure = el.string_attr("AXSubrole").as_deref() == Some("AXSecureTextField");
            let text = if is_secure && redact_passwords {
                "[REDACTED]".into()
            } else {
                el.string_attr("AXValue")
                    .or_else(|| el.string_attr("AXTitle"))
                    .or_else(|| el.string_attr("AXDescription"))
                    .unwrap_or_default()
            };
            Ok(ReadTextResult {
                text,
                source: ref_id.unwrap_or("focused").into(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batched_node_attrs_skips_bounds_when_not_requested_and_observer_respects_stop() {
        let stop = AtomicBool::new(true);
        let t0 = Instant::now();
        wait_for_ui_event(Some(1), Duration::from_millis(200), &stop);
        assert!(
            t0.elapsed() < Duration::from_millis(50),
            "wait_for_ui_event must return immediately when stop_flag is set"
        );

        if !crate::power::permissions().accessibility {
            return;
        }
        let Some(finder) = windows::find_app("Finder").and_then(|a| a.pid) else {
            return;
        };
        let app_el = AxElement::application(finder as i32);
        let without_bounds = app_el.read_node_attrs(false);
        assert_eq!(without_bounds.raw_role, "AXApplication");
        assert!(without_bounds.bounds.is_none());

        let with_bounds = app_el.read_node_attrs(true);
        assert_eq!(with_bounds.raw_role, "AXApplication");
    }
}
