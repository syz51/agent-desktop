use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use desktop_core::StepExecInfo;
use desktop_proto::{
    ClickMode, ElementAction, ErrorCode, FocusMode, KeyCode, KeyCombo, Modifiers, MouseButton,
    Point, RpcError, ScrollDirection, TextMethod,
};
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

use crate::ax::{AxRefStore, require_accessibility};
use crate::ffi::{
    self, CGEventRef, CGPoint, CfOwned, K_CG_EVENT_FLAG_MASK_ALTERNATE,
    K_CG_EVENT_FLAG_MASK_COMMAND, K_CG_EVENT_FLAG_MASK_CONTROL, K_CG_EVENT_FLAG_MASK_SECONDARY_FN,
    K_CG_EVENT_FLAG_MASK_SHIFT, K_CG_EVENT_LEFT_MOUSE_DOWN, K_CG_EVENT_LEFT_MOUSE_DRAGGED,
    K_CG_EVENT_LEFT_MOUSE_UP, K_CG_EVENT_MOUSE_MOVED, K_CG_EVENT_OTHER_MOUSE_DOWN,
    K_CG_EVENT_OTHER_MOUSE_UP, K_CG_EVENT_RIGHT_MOUSE_DOWN, K_CG_EVENT_RIGHT_MOUSE_UP,
    K_CG_EVENT_SOURCE_STATE_HID_SYSTEM_STATE, K_CG_EVENT_SOURCE_USER_DATA, K_CG_HID_EVENT_TAP,
    K_CG_MOUSE_BUTTON_CENTER, K_CG_MOUSE_BUTTON_LEFT, K_CG_MOUSE_BUTTON_RIGHT,
    K_CG_MOUSE_EVENT_CLICK_STATE, K_CG_SCROLL_EVENT_UNIT_LINE, SYNTHETIC_MARK,
};
use crate::windows;

fn event_source() -> Result<CfOwned, RpcError> {
    // SAFETY: `CGEventSourceCreate` returns an owned +1 CGEventSourceRef.
    unsafe {
        CfOwned::from_owned(
            ffi::CGEventSourceCreate(K_CG_EVENT_SOURCE_STATE_HID_SYSTEM_STATE).cast_const(),
        )
    }
    .ok_or_else(|| RpcError::new(ErrorCode::BackendError, "CGEventSourceCreate failed"))
}

fn modifier_flags(mods: Modifiers) -> u64 {
    let mut f = 0u64;
    if mods.cmd {
        f |= K_CG_EVENT_FLAG_MASK_COMMAND;
    }
    if mods.ctrl {
        f |= K_CG_EVENT_FLAG_MASK_CONTROL;
    }
    if mods.alt {
        f |= K_CG_EVENT_FLAG_MASK_ALTERNATE;
    }
    if mods.shift {
        f |= K_CG_EVENT_FLAG_MASK_SHIFT;
    }
    if mods.fn_key {
        f |= K_CG_EVENT_FLAG_MASK_SECONDARY_FN;
    }
    f
}

fn check_stop(stop: &AtomicBool) -> Result<(), RpcError> {
    if stop.load(Ordering::Acquire) {
        return Err(RpcError::with_next(
            ErrorCode::StoppedByUser,
            "stopped by the user during action",
            "Pause and check with the user before resuming.",
        ));
    }
    Ok(())
}

const PASTE_SETTLE_DELAY: Duration = Duration::from_millis(150);
const STOP_POLL_SLICE: Duration = Duration::from_millis(20);

fn sleep_abortable(stop: &AtomicBool, dur: Duration) -> Result<(), RpcError> {
    let deadline = std::time::Instant::now() + dur;
    loop {
        check_stop(stop)?;
        let now = std::time::Instant::now();
        if now >= deadline {
            return Ok(());
        }
        std::thread::sleep(deadline.saturating_duration_since(now).min(STOP_POLL_SLICE));
    }
}

fn apply_event_flags(ev: &CfOwned, mods: Modifiers) {
    let flags = modifier_flags(mods);
    // SAFETY: `ev` is a valid retained CGEventRef; setting `flags` unconditionally (including `0`)
    // clears any stale/latched HID modifier flags inherited from the event source.
    unsafe {
        ffi::CGEventSetFlags(ev.as_mut_ptr(), flags);
    }
}

fn post_event(ev: CfOwned, mods: Modifiers, stop: &AtomicBool) -> Result<(), RpcError> {
    check_stop(stop)?;
    apply_event_flags(&ev, mods);
    // SAFETY: `ev` is a valid retained CGEventRef.
    unsafe {
        ffi::CGEventSetIntegerValueField(
            ev.as_mut_ptr(),
            K_CG_EVENT_SOURCE_USER_DATA,
            SYNTHETIC_MARK,
        );
        ffi::CGEventPost(K_CG_HID_EVENT_TAP, ev.as_mut_ptr());
    }
    Ok(())
}

fn current_cursor_location() -> Option<CGPoint> {
    // SAFETY: `CGEventCreate(NULL)` captures the current mouse location.
    let ev = unsafe { CfOwned::from_owned(ffi::CGEventCreate(std::ptr::null_mut()).cast_const()) }?;
    Some(unsafe { ffi::CGEventGetLocation(ev.as_mut_ptr()) })
}

/// Temporarily save previous frontmost app + cursor position when `FocusMode::Auto` must
/// briefly borrow focus or pointer, then restore both on drop if they changed.
struct FocusRestoreGuard {
    prev_pid: Option<i32>,
    prev_cursor: Option<CGPoint>,
    active: bool,
}

impl FocusRestoreGuard {
    fn enter(target_pid: Option<i32>, focus_mode: FocusMode) -> Self {
        if focus_mode != FocusMode::Auto {
            if focus_mode == FocusMode::Foreground
                && let Some(pid) = target_pid
            {
                windows::activate_pid(pid);
                std::thread::sleep(Duration::from_millis(30));
            }
            return Self {
                prev_pid: None,
                prev_cursor: None,
                active: false,
            };
        }
        let prev_pid = windows::frontmost_pid();
        let prev_cursor = current_cursor_location();
        let needs_switch = target_pid.is_some() && target_pid != prev_pid;
        if needs_switch && let Some(pid) = target_pid {
            windows::activate_pid(pid);
            std::thread::sleep(Duration::from_millis(35));
        }
        Self {
            prev_pid,
            prev_cursor,
            active: true,
        }
    }
}

impl Drop for FocusRestoreGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Some(pt) = self.prev_cursor
            && let Ok(src) = event_source()
        {
            // SAFETY: Move pointer back silently without marking a click.
            if let Some(ev) = unsafe {
                CfOwned::from_owned(
                    ffi::CGEventCreateMouseEvent(
                        src.as_mut_ptr(),
                        K_CG_EVENT_MOUSE_MOVED,
                        pt,
                        K_CG_MOUSE_BUTTON_LEFT,
                    )
                    .cast_const(),
                )
            } {
                apply_event_flags(&ev, Modifiers::default());
                // SAFETY: `ev` is a valid retained CGEventRef; tagging `SYNTHETIC_MARK` and
                // posting to `K_CG_HID_EVENT_TAP` restores the cursor without tripping the kill-switch.
                unsafe {
                    ffi::CGEventSetIntegerValueField(
                        ev.as_mut_ptr(),
                        K_CG_EVENT_SOURCE_USER_DATA,
                        SYNTHETIC_MARK,
                    );
                    ffi::CGEventPost(K_CG_HID_EVENT_TAP, ev.as_mut_ptr());
                }
            }
        }
        // Always allow a brief settle window after async CGEventPost before checking/restoring
        // focus so mouse-up/paste/Return finishes delivering to the target app and macOS finishes
        // any implicit click-to-activate before we decide whether focus changed.
        if let Some(pid) = self.prev_pid {
            std::thread::sleep(Duration::from_millis(20));
            if windows::frontmost_pid() != Some(pid) {
                windows::activate_pid(pid);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn click_ref(
    refs: &AxRefStore,
    stop: &AtomicBool,
    ref_id: &str,
    button: MouseButton,
    count: u8,
    mods: Modifiers,
    click_mode: ClickMode,
    focus_mode: FocusMode,
) -> Result<StepExecInfo, RpcError> {
    require_accessibility()?;
    check_stop(stop)?;
    let live = refs.get(ref_id)?;

    // Tier 1: Zero-focus semantic AXPress (fastest, zero cursor warp, works in background).
    let can_semantic = click_mode != ClickMode::Pointer
        && button == MouseButton::Left
        && count == 1
        && mods.is_empty()
        && live.actions.contains(&ElementAction::Press);

    if can_semantic {
        if live.element.perform_action("AXPress").is_ok() {
            return Ok(StepExecInfo {
                detail: format!(
                    "pressed {ref_id} in {} via AXPress (background)",
                    live.app.name
                ),
                used_background: true,
            });
        }
        if click_mode == ClickMode::Semantic {
            return Err(RpcError::with_next(
                ErrorCode::BackendError,
                format!("AXPress failed on {ref_id}"),
                "Retry with mode=\"auto\" or mode=\"pointer\".",
            ));
        }
    }

    if focus_mode == FocusMode::Background {
        return Err(RpcError::with_next(
            ErrorCode::RequiresFocus,
            format!(
                "{ref_id} in {} requires foreground pointer input",
                live.app.name
            ),
            "Retry with focus_mode=\"auto\" (brief focus-and-restore) or focus_mode=\"foreground\".",
        ));
    }

    let bounds = live.element.bounds().ok_or_else(|| {
        RpcError::with_next(
            ErrorCode::StaleRef,
            format!("{ref_id} has no visible bounding box"),
            "Take a fresh snapshot.",
        )
    })?;
    let _guard = FocusRestoreGuard::enter(live.app.pid.map(|p| p as i32), focus_mode);
    let info = click_point_raw(stop, bounds.center(), button, count, mods)?;
    Ok(StepExecInfo {
        detail: format!("clicked {ref_id} in {} ({})", live.app.name, info.detail),
        used_background: false,
    })
}

pub fn click_point(
    stop: &AtomicBool,
    point: Point,
    button: MouseButton,
    count: u8,
    mods: Modifiers,
    focus_mode: FocusMode,
) -> Result<StepExecInfo, RpcError> {
    require_accessibility()?;
    if focus_mode == FocusMode::Background {
        return Err(RpcError::with_next(
            ErrorCode::RequiresFocus,
            "raw coordinate clicks require foreground HID pointer input on macOS",
            "Target an @eN ref from snapshot for background AXPress, or use focus_mode=\"auto\".",
        ));
    }
    let _guard = if focus_mode == FocusMode::Auto {
        FocusRestoreGuard::enter(None, FocusMode::Auto)
    } else {
        FocusRestoreGuard::enter(None, FocusMode::Foreground)
    };
    click_point_raw(stop, point, button, count, mods)
}

fn click_point_raw(
    stop: &AtomicBool,
    point: Point,
    button: MouseButton,
    count: u8,
    mods: Modifiers,
) -> Result<StepExecInfo, RpcError> {
    let src = event_source()?;
    let pt = CGPoint {
        x: point.x,
        y: point.y,
    };
    let (down_ty, up_ty, btn) = match button {
        MouseButton::Left => (
            K_CG_EVENT_LEFT_MOUSE_DOWN,
            K_CG_EVENT_LEFT_MOUSE_UP,
            K_CG_MOUSE_BUTTON_LEFT,
        ),
        MouseButton::Right => (
            K_CG_EVENT_RIGHT_MOUSE_DOWN,
            K_CG_EVENT_RIGHT_MOUSE_UP,
            K_CG_MOUSE_BUTTON_RIGHT,
        ),
        MouseButton::Middle => (
            K_CG_EVENT_OTHER_MOUSE_DOWN,
            K_CG_EVENT_OTHER_MOUSE_UP,
            K_CG_MOUSE_BUTTON_CENTER,
        ),
    };

    let mv = make_mouse(&src, K_CG_EVENT_MOUSE_MOVED, pt, btn)?;
    post_event(mv, Modifiers::default(), stop)?;
    std::thread::sleep(Duration::from_millis(20));

    for click_state in 1..=count.max(1) {
        for ty in [down_ty, up_ty] {
            let ev = make_mouse(&src, ty, pt, btn)?;
            // SAFETY: Sets clickState (1, 2, 3) for single/double/triple clicks.
            unsafe {
                ffi::CGEventSetIntegerValueField(
                    ev.as_mut_ptr(),
                    K_CG_MOUSE_EVENT_CLICK_STATE,
                    i64::from(click_state),
                );
            }
            post_event(ev, mods, stop)?;
        }
        if click_state < count {
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    Ok(StepExecInfo {
        detail: format!("clicked ({:.0}, {:.0})", point.x, point.y),
        used_background: false,
    })
}

pub fn hover_point(
    stop: &AtomicBool,
    point: Point,
    focus_mode: FocusMode,
) -> Result<StepExecInfo, RpcError> {
    require_accessibility()?;
    if focus_mode == FocusMode::Background {
        return Err(RpcError::with_next(
            ErrorCode::RequiresFocus,
            "hover requires physical pointer movement on macOS",
            "Retry with focus_mode=\"auto\" or focus_mode=\"foreground\".",
        ));
    }
    let src = event_source()?;
    let ev = make_mouse(
        &src,
        K_CG_EVENT_MOUSE_MOVED,
        CGPoint {
            x: point.x,
            y: point.y,
        },
        K_CG_MOUSE_BUTTON_LEFT,
    )?;
    post_event(ev, Modifiers::default(), stop)?;
    Ok(StepExecInfo {
        detail: format!("hovered ({:.0}, {:.0})", point.x, point.y),
        used_background: false,
    })
}

pub fn drag(
    stop: &AtomicBool,
    from: Point,
    to: Point,
    mods: Modifiers,
    focus_mode: FocusMode,
) -> Result<StepExecInfo, RpcError> {
    require_accessibility()?;
    if focus_mode == FocusMode::Background {
        return Err(RpcError::with_next(
            ErrorCode::RequiresFocus,
            "drag requires foreground pointer input",
            "Use focus_mode=\"auto\" or \"foreground\".",
        ));
    }
    let _guard = FocusRestoreGuard::enter(None, focus_mode);
    let src = event_source()?;
    let p1 = CGPoint {
        x: from.x,
        y: from.y,
    };
    let p2 = CGPoint { x: to.x, y: to.y };

    post_event(
        make_mouse(&src, K_CG_EVENT_MOUSE_MOVED, p1, K_CG_MOUSE_BUTTON_LEFT)?,
        Modifiers::default(),
        stop,
    )?;
    std::thread::sleep(Duration::from_millis(25));
    post_event(
        make_mouse(&src, K_CG_EVENT_LEFT_MOUSE_DOWN, p1, K_CG_MOUSE_BUTTON_LEFT)?,
        mods,
        stop,
    )?;

    let steps = 12;
    for step in 1..=steps {
        let t = f64::from(step) / f64::from(steps);
        let cur = CGPoint {
            x: p1.x + (p2.x - p1.x) * t,
            y: p1.y + (p2.y - p1.y) * t,
        };
        post_event(
            make_mouse(
                &src,
                K_CG_EVENT_LEFT_MOUSE_DRAGGED,
                cur,
                K_CG_MOUSE_BUTTON_LEFT,
            )?,
            mods,
            stop,
        )?;
        std::thread::sleep(Duration::from_millis(12));
    }

    post_event(
        make_mouse(&src, K_CG_EVENT_LEFT_MOUSE_UP, p2, K_CG_MOUSE_BUTTON_LEFT)?,
        mods,
        stop,
    )?;
    Ok(StepExecInfo {
        detail: format!(
            "dragged ({:.0},{:.0}) -> ({:.0},{:.0})",
            from.x, from.y, to.x, to.y
        ),
        used_background: false,
    })
}

pub fn scroll_ref(
    refs: &AxRefStore,
    stop: &AtomicBool,
    ref_id: &str,
    direction: ScrollDirection,
    amount: u32,
    focus_mode: FocusMode,
) -> Result<StepExecInfo, RpcError> {
    require_accessibility()?;
    check_stop(stop)?;
    let live = refs.get(ref_id)?;

    // Tier 1: Zero-focus, zero-cursor background AX scroll via ancestor AXScrollArea scrollbar.
    if try_ax_scroll(&live.element, direction, amount) {
        return Ok(StepExecInfo {
            detail: format!(
                "scrolled {ref_id} {direction:?} in {} via AXScrollBar (background)",
                live.app.name
            ),
            used_background: true,
        });
    }

    if focus_mode == FocusMode::Background {
        return Err(RpcError::with_next(
            ErrorCode::RequiresFocus,
            format!(
                "{ref_id} in {} does not expose a writable AXScrollBar for background scrolling",
                live.app.name
            ),
            "Retry with focus_mode=\"auto\" (cursor-restoring scroll) or focus_mode=\"foreground\".",
        ));
    }

    let bounds = live.element.bounds().ok_or_else(|| {
        RpcError::with_next(
            ErrorCode::StaleRef,
            format!("{ref_id} has no visible bounding box"),
            "Take a fresh snapshot.",
        )
    })?;
    let center = bounds.center();
    // macOS delivers wheel events to unfocused visible windows without activating them; only
    // temporarily activate `live.app.pid` if another window currently occludes `center`.
    let occluded = windows::app_at_point(center)
        .ok()
        .flatten()
        .is_some_and(|top| top.pid != live.app.pid);
    let target_pid = if occluded || focus_mode == FocusMode::Foreground {
        live.app.pid.map(|p| p as i32)
    } else {
        None
    };
    let _guard = FocusRestoreGuard::enter(target_pid, focus_mode);
    scroll_point_raw(stop, center, direction, amount)
}

/// Try adjusting an element's own or nearest ancestor `AXScrollArea`'s `AXVerticalScrollBar` /
/// `AXHorizontalScrollBar` value directly via Accessibility without moving the cursor or focusing.
fn try_ax_scroll(el: &crate::ax::AxElement, direction: ScrollDirection, amount: u32) -> bool {
    let bar_attr = match direction {
        ScrollDirection::Up | ScrollDirection::Down => "AXVerticalScrollBar",
        ScrollDirection::Left | ScrollDirection::Right => "AXHorizontalScrollBar",
    };
    let sign = match direction {
        ScrollDirection::Up | ScrollDirection::Left => -1.0,
        ScrollDirection::Down | ScrollDirection::Right => 1.0,
    };
    let delta = (f64::from(amount.clamp(1, 50)) * 0.05).clamp(0.02, 0.5);

    let mut cur = Some(el.clone());
    let mut had_writable_at_boundary = false;
    for _ in 0..16 {
        let Some(node) = cur else { break };
        let role = node.string_attr("AXRole").unwrap_or_default();
        let bar = if role == "AXScrollBar" {
            Some(node.clone())
        } else {
            node.element_attr(bar_attr)
        };
        if let Some(sb) = bar
            && let Some(val) = sb.f64_attr("AXValue")
        {
            let next = (val + sign * delta).clamp(0.0, 1.0);
            if (next - val).abs() > 1e-4 {
                if sb.set_f64_attr("AXValue", next).is_ok() {
                    return true;
                }
            } else if sb.set_f64_attr("AXValue", val).is_ok() {
                // Already at the 0.0/1.0 boundary of this writable scroll area; keep checking
                // outer containers in case a parent can still scroll, otherwise treat as handled.
                had_writable_at_boundary = true;
            }
        }
        cur = node.element_attr("AXParent");
    }
    had_writable_at_boundary
}

pub fn scroll(
    stop: &AtomicBool,
    point: Point,
    direction: ScrollDirection,
    amount: u32,
    focus_mode: FocusMode,
) -> Result<StepExecInfo, RpcError> {
    require_accessibility()?;
    if focus_mode == FocusMode::Background {
        return Err(RpcError::with_next(
            ErrorCode::RequiresFocus,
            "raw coordinate scroll requires physical pointer input on macOS",
            "Target an @eN ref for background AX scrolling, or use focus_mode=\"auto\".",
        ));
    }
    let _guard = FocusRestoreGuard::enter(None, focus_mode);
    scroll_point_raw(stop, point, direction, amount)
}

fn scroll_point_raw(
    stop: &AtomicBool,
    point: Point,
    direction: ScrollDirection,
    amount: u32,
) -> Result<StepExecInfo, RpcError> {
    let src = event_source()?;
    let pt = CGPoint {
        x: point.x,
        y: point.y,
    };
    post_event(
        make_mouse(&src, K_CG_EVENT_MOUSE_MOVED, pt, K_CG_MOUSE_BUTTON_LEFT)?,
        Modifiers::default(),
        stop,
    )?;
    std::thread::sleep(Duration::from_millis(15));
    let amt = amount as i32;
    let (dx, dy) = match direction {
        ScrollDirection::Up => (0, amt),
        ScrollDirection::Down => (0, -amt),
        ScrollDirection::Left => (amt, 0),
        ScrollDirection::Right => (-amt, 0),
    };
    // SAFETY: Creates a line-unit scroll wheel CGEvent.
    let ev = unsafe {
        CfOwned::from_owned(
            ffi::CGEventCreateScrollWheelEvent2(
                src.as_mut_ptr(),
                K_CG_SCROLL_EVENT_UNIT_LINE,
                2,
                dy,
                dx,
                0,
            )
            .cast_const(),
        )
    }
    .ok_or_else(|| RpcError::new(ErrorCode::BackendError, "scroll event creation failed"))?;
    post_event(ev, Modifiers::default(), stop)?;
    Ok(StepExecInfo {
        detail: format!("scrolled {direction:?} by {amount}"),
        used_background: false,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn set_text(
    refs: &AxRefStore,
    stop: &AtomicBool,
    ref_id: Option<&str>,
    text: &str,
    replace: bool,
    submit: bool,
    method: TextMethod,
    focus_mode: FocusMode,
) -> Result<StepExecInfo, RpcError> {
    require_accessibility()?;
    check_stop(stop)?;

    // Tier 1: Direct zero-focus AXSetValue (<5 ms, works in background).
    if matches!(method, TextMethod::Auto | TextMethod::Ax)
        && let Some(rid) = ref_id
    {
        let live = refs.get(rid)?;
        let current = live.element.string_attr("AXValue").unwrap_or_default();
        let target_val = if replace || current.is_empty() {
            text.to_string()
        } else {
            format!("{current}{text}")
        };
        if live.element.set_string_attr("AXValue", &target_val).is_ok() {
            if submit {
                let _ = live.element.perform_action("AXConfirm");
            }
            return Ok(StepExecInfo {
                detail: format!(
                    "set {} chars on {rid} via AXSetValue (background)",
                    text.chars().count()
                ),
                used_background: true,
            });
        }
        if method == TextMethod::Ax {
            return Err(RpcError::with_next(
                ErrorCode::BackendError,
                format!("AXSetValue not supported on {rid}"),
                "Retry with method=\"auto\" or \"paste\".",
            ));
        }
    }

    if focus_mode == FocusMode::Background {
        return Err(RpcError::with_next(
            ErrorCode::RequiresFocus,
            "control does not support background AXSetValue",
            "Retry with focus_mode=\"auto\" (brief focus-and-restore) or focus_mode=\"foreground\".",
        ));
    }

    // Focus the target ref if provided.
    let target_pid = if let Some(rid) = ref_id {
        let live = refs.get(rid)?;
        let _ = live.element.set_bool_true("AXFocused");
        live.app.pid.map(|p| p as i32)
    } else {
        None
    };

    let _guard = FocusRestoreGuard::enter(target_pid, focus_mode);

    if let Some(rid) = ref_id
        && let Ok(live) = refs.get(rid)
        && let Some(bounds) = live.element.bounds()
        && !live.element.bool_attr("AXFocused").unwrap_or(false)
    {
        let _ = click_point_raw(
            stop,
            bounds.center(),
            MouseButton::Left,
            1,
            Modifiers::default(),
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    if replace {
        press_key_raw(
            stop,
            KeyCombo {
                key: KeyCode::A,
                modifiers: Modifiers {
                    cmd: true,
                    ..Modifiers::default()
                },
            },
            1,
        )?;
        std::thread::sleep(Duration::from_millis(15));
    }

    let is_short_ascii =
        text.len() <= 16 && text.is_ascii() && !text.contains('\n') && !text.contains('\r');

    let used_method = match method {
        TextMethod::Type => {
            type_unicode_chunks(stop, text)?;
            "type"
        }
        TextMethod::Paste => {
            paste_with_clipboard_restore(stop, text)?;
            "paste"
        }
        TextMethod::Auto | TextMethod::Ax => {
            if is_short_ascii {
                type_unicode_chunks(stop, text)?;
                "type"
            } else {
                paste_with_clipboard_restore(stop, text)?;
                "paste"
            }
        }
    };

    if submit {
        std::thread::sleep(Duration::from_millis(15));
        press_key_raw(
            stop,
            KeyCombo {
                key: KeyCode::Return,
                modifiers: Modifiers::default(),
            },
            1,
        )?;
    }

    Ok(StepExecInfo {
        detail: format!("entered {} chars via {used_method}", text.chars().count()),
        used_background: false,
    })
}

/// Fastest bulk text entry: save current NSPasteboard string, paste via Cmd+V, restore original.
fn paste_with_clipboard_restore(stop: &AtomicBool, text: &str) -> Result<(), RpcError> {
    let pb = NSPasteboard::generalPasteboard();
    // SAFETY: `NSPasteboardTypeString` is a static AppKit constant.
    let saved = unsafe { pb.stringForType(NSPasteboardTypeString) }.map(|s| s.to_string());
    let _ = pb.clearContents();
    let ns_text = NSString::from_str(text);
    // SAFETY: `NSPasteboardTypeString` is valid and `ns_text` is a retained NSString.
    let ok = unsafe { pb.setString_forType(&ns_text, NSPasteboardTypeString) };
    if !ok {
        if let Some(prev) = saved {
            let ns_prev = NSString::from_str(&prev);
            // SAFETY: `NSPasteboardTypeString` is a static AppKit constant and `ns_prev` is retained.
            unsafe {
                let _ = pb.setString_forType(&ns_prev, NSPasteboardTypeString);
            }
        }
        return type_unicode_chunks(stop, text);
    }
    let after_write_count = pb.changeCount();

    let paste_res = press_key_raw(
        stop,
        KeyCombo {
            key: KeyCode::V,
            modifiers: Modifiers {
                cmd: true,
                ..Modifiers::default()
            },
        },
        1,
    );
    // Chromium/Electron browsers read NSPasteboard asynchronously after Cmd+V; wait in short
    // stop-aware slices so the target consumes the pasted text before we restore the clipboard.
    let settle_res = if paste_res.is_ok() {
        sleep_abortable(stop, PASTE_SETTLE_DELAY)
    } else {
        Ok(())
    };

    // Only restore if nothing else modified the clipboard in the meantime.
    if pb.changeCount() == after_write_count {
        let _ = pb.clearContents();
        if let Some(prev) = saved {
            let ns_prev = NSString::from_str(&prev);
            // SAFETY: `NSPasteboardTypeString` is a static AppKit constant and `ns_prev` is retained.
            unsafe {
                let _ = pb.setString_forType(&ns_prev, NSPasteboardTypeString);
            }
        }
    }
    paste_res.and(settle_res)
}

fn type_unicode_chunks(stop: &AtomicBool, text: &str) -> Result<(), RpcError> {
    let src = event_source()?;
    for (line_idx, line) in text.split('\n').enumerate() {
        if line_idx > 0 {
            press_key_raw(
                stop,
                KeyCombo {
                    key: KeyCode::Return,
                    modifiers: Modifiers::default(),
                },
                1,
            )?;
        }
        // Chunk by whole Unicode scalar values, <= 16 UTF-16 code units per event so surrogate
        // pairs never split across CGEvents.
        let mut chunk: Vec<u16> = Vec::with_capacity(16);
        let flush = |buf: &mut Vec<u16>| -> Result<(), RpcError> {
            if buf.is_empty() {
                return Ok(());
            }
            for down in [true, false] {
                let ev = make_key(&src, 0, down)?;
                // SAFETY: `buf` is valid UTF-16 for `buf.len()` code units.
                unsafe {
                    ffi::CGEventKeyboardSetUnicodeString(ev.as_mut_ptr(), buf.len(), buf.as_ptr());
                }
                post_event(ev, Modifiers::default(), stop)?;
            }
            buf.clear();
            std::thread::sleep(Duration::from_millis(6));
            Ok(())
        };

        for ch in line.chars() {
            let mut tmp = [0u16; 2];
            let units = ch.encode_utf16(&mut tmp);
            if chunk.len() + units.len() > 16 {
                flush(&mut chunk)?;
            }
            chunk.extend_from_slice(units);
        }
        flush(&mut chunk)?;
    }
    Ok(())
}

pub fn press_key_combo(
    stop: &AtomicBool,
    combo: KeyCombo,
    repeat: u8,
    focus_mode: FocusMode,
) -> Result<StepExecInfo, RpcError> {
    require_accessibility()?;
    if focus_mode == FocusMode::Background {
        // Attempt `CGEventPostToPid` to focused app if known, otherwise explain focus needed.
        if let Some(pid) = windows::frontmost_pid() {
            let src = event_source()?;
            let code = combo.key.macos_keycode();
            for _ in 0..repeat.max(1) {
                check_stop(stop)?;
                for down in [true, false] {
                    let ev = make_key(&src, code, down)?;
                    apply_event_flags(&ev, combo.modifiers);
                    // SAFETY: `ev` is a valid retained CGEventRef; tag `SYNTHETIC_MARK` and post to `pid`.
                    unsafe {
                        ffi::CGEventSetIntegerValueField(
                            ev.as_mut_ptr(),
                            K_CG_EVENT_SOURCE_USER_DATA,
                            SYNTHETIC_MARK,
                        );
                        ffi::CGEventPostToPid(pid, ev.as_mut_ptr());
                    }
                }
            }
            return Ok(StepExecInfo {
                detail: format!("posted {:?} x{repeat} to pid {pid}", combo.key),
                used_background: true,
            });
        }
    }
    press_key_raw(stop, combo, repeat)?;
    Ok(StepExecInfo {
        detail: format!("pressed {:?} x{repeat}", combo.key),
        used_background: false,
    })
}

fn press_key_raw(stop: &AtomicBool, combo: KeyCombo, repeat: u8) -> Result<(), RpcError> {
    let src = event_source()?;
    let code = combo.key.macos_keycode();
    for i in 0..repeat.max(1) {
        for down in [true, false] {
            let ev = make_key(&src, code, down)?;
            post_event(ev, combo.modifiers, stop)?;
        }
        if i + 1 < repeat {
            std::thread::sleep(Duration::from_millis(15));
        }
    }
    Ok(())
}

fn make_mouse(
    src: &CfOwned,
    event_type: u32,
    point: CGPoint,
    button: u32,
) -> Result<CfOwned, RpcError> {
    // SAFETY: `src` is a valid CGEventSourceRef.
    unsafe {
        CfOwned::from_owned(
            ffi::CGEventCreateMouseEvent(src.as_mut_ptr(), event_type, point, button).cast_const(),
        )
    }
    .ok_or_else(|| RpcError::new(ErrorCode::BackendError, "CGEventCreateMouseEvent failed"))
}

fn make_key(src: &CfOwned, code: u16, down: bool) -> Result<CfOwned, RpcError> {
    // SAFETY: `src` is a valid CGEventSourceRef.
    unsafe {
        CfOwned::from_owned(
            ffi::CGEventCreateKeyboardEvent(src.as_mut_ptr(), code, down).cast_const(),
        )
    }
    .ok_or_else(|| RpcError::new(ErrorCode::BackendError, "CGEventCreateKeyboardEvent failed"))
}

#[allow(dead_code)]
fn _keep_cgeventref(_: Option<CGEventRef>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_hover_and_raw_scroll_refuse_pointer_movement() {
        let stop = AtomicBool::new(false);
        let pt = Point { x: 100.0, y: 100.0 };

        let hover_err = hover_point(&stop, pt, FocusMode::Background).unwrap_err();
        assert!(matches!(
            hover_err.code,
            ErrorCode::RequiresFocus | ErrorCode::PermissionDenied
        ));

        let scroll_err =
            scroll(&stop, pt, ScrollDirection::Down, 3, FocusMode::Background).unwrap_err();
        assert!(matches!(
            scroll_err.code,
            ErrorCode::RequiresFocus | ErrorCode::PermissionDenied
        ));
    }

    #[test]
    fn apply_event_flags_clears_latched_modifiers_when_unmodified() {
        let src = event_source().expect("CGEventSourceCreate");
        let ev = make_key(&src, KeyCode::Return.macos_keycode(), true).expect("make_key");

        // Simulate stale Command + Shift flags on the CGEvent.
        apply_event_flags(
            &ev,
            Modifiers {
                cmd: true,
                shift: true,
                ..Modifiers::default()
            },
        );
        // SAFETY: `ev` is a valid retained CGEventRef.
        let set_flags = unsafe { ffi::CGEventGetFlags(ev.as_mut_ptr()) };
        assert_ne!(set_flags & K_CG_EVENT_FLAG_MASK_COMMAND, 0);
        assert_ne!(set_flags & K_CG_EVENT_FLAG_MASK_SHIFT, 0);

        // Applying default (unmodified) modifiers must explicitly clear modifier bits to 0.
        apply_event_flags(&ev, Modifiers::default());
        // SAFETY: `ev` is a valid retained CGEventRef.
        let cleared_flags = unsafe { ffi::CGEventGetFlags(ev.as_mut_ptr()) };
        assert_eq!(
            cleared_flags
                & (K_CG_EVENT_FLAG_MASK_COMMAND
                    | K_CG_EVENT_FLAG_MASK_CONTROL
                    | K_CG_EVENT_FLAG_MASK_ALTERNATE
                    | K_CG_EVENT_FLAG_MASK_SHIFT
                    | K_CG_EVENT_FLAG_MASK_SECONDARY_FN),
            0,
            "unmodified events must clear stale modifier bits"
        );
    }

    #[test]
    fn sleep_abortable_returns_stopped_immediately_when_stop_latched() {
        let stop = AtomicBool::new(true);
        let start = std::time::Instant::now();
        let err = sleep_abortable(&stop, PASTE_SETTLE_DELAY).unwrap_err();
        assert_eq!(err.code, ErrorCode::StoppedByUser);
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "abortable paste settle must exit immediately when stop is latched"
        );
    }
}
