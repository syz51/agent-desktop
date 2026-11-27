use std::cell::RefCell;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use block2::RcBlock;
use desktop_core::EscKillSwitch;
use desktop_proto::{ErrorCode, RpcError};
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{AnyThread, DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSApplicationActivationPolicy,
    NSBackingStoreType, NSButton, NSColor, NSEvent, NSEventMask, NSFont, NSPanel, NSScreen,
    NSTextField, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use crate::ffi::{self, K_CG_EVENT_SOURCE_USER_DATA, SYNTHETIC_MARK};

/// Handle allowing Tokio worker threads to safely dispatch AppKit UI updates onto the main thread.
#[derive(Clone)]
pub struct UiHandle {
    kill_switch: Arc<EscKillSwitch>,
}

struct StopButtonIvars {
    kill_switch: Arc<EscKillSwitch>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "AgentDesktopStopButtonTarget"]
    #[ivars = StopButtonIvars]
    struct StopButtonTarget;

    impl StopButtonTarget {
        #[allow(non_snake_case)]
        #[unsafe(method(onStopClicked:))]
        fn on_stop_clicked(&self, _sender: Option<&AnyObject>) {
            // Ignore synthetic agent clicks stamped with `SYNTHETIC_MARK` so an agent clicking
            // near the bottom-right corner never self-aborts its own session.
            if let Some(mtm) = MainThreadMarker::new()
                && let Some(ev) = NSApplication::sharedApplication(mtm).currentEvent()
                && is_synthetic_event(&ev)
            {
                return;
            }
            self.ivars().kill_switch.trigger_stop_button();
            hide_overlay();
        }
    }
);

impl StopButtonTarget {
    fn new(kill_switch: Arc<EscKillSwitch>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(StopButtonIvars { kill_switch });
        // SAFETY: Standard NSObject `init` on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }
}

thread_local! {
    static OVERLAY_PANEL: RefCell<Option<OverlayState>> = const { RefCell::new(None) };
}

struct OverlayState {
    panel: Retained<NSPanel>,
    label: Retained<NSTextField>,
    _stop_button: Retained<NSButton>,
    /// Retained explicitly because `NSControl.target` is unretained/weak in AppKit.
    _stop_target: Retained<StopButtonTarget>,
}

impl UiHandle {
    #[must_use]
    pub fn new(kill_switch: Arc<EscKillSwitch>) -> Self {
        Self { kill_switch }
    }

    pub fn set_overlay(&self, text: Option<&str>) {
        let owned = text.map(str::to_string);
        let kill_switch = Arc::clone(&self.kill_switch);
        DispatchQueue::main().exec_async(move || {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            match owned {
                Some(label_text) => {
                    // Re-check `control_active` on the main queue so a queued show can never
                    // leave the pill visible if double-Esc, Stop, or teardown already ran.
                    if kill_switch.is_control_active() {
                        show_or_update_overlay(mtm, &label_text, kill_switch);
                    } else {
                        hide_overlay();
                    }
                }
                None => hide_overlay(),
            }
        });
    }

    pub fn confirm_dialog(&self, title: &str, message: &str) -> Result<bool, RpcError> {
        let title = title.to_string();
        let message = message.to_string();
        let (tx, rx) = mpsc::sync_channel::<bool>(1);
        DispatchQueue::main().exec_async(move || {
            let allowed = if let Some(mtm) = MainThreadMarker::new() {
                run_ns_alert(mtm, &title, &message)
            } else {
                false
            };
            let _ = tx.send(allowed);
        });
        rx.recv_timeout(std::time::Duration::from_secs(125))
            .map_err(|_| RpcError::new(ErrorCode::Timeout, "confirmation dialog timed out"))
    }

    pub fn shutdown(&self) {
        self.kill_switch.set_control_active(false);
        self.kill_switch.stop_flag().store(true, Ordering::Release);
        DispatchQueue::main().exec_async(|| {
            if let Some(mtm) = MainThreadMarker::new() {
                hide_overlay();
                let app = NSApplication::sharedApplication(mtm);
                app.terminate(None);
            }
        });
    }
}

/// Start the macOS AppKit runloop on the process main thread.
///
/// Registers a global physical keyDown monitor that only arms while a mutating control lease is
/// active (`kill_switch.is_control_active()`), ignores synthetic agent events stamped with
/// `SYNTHETIC_MARK` and key repeats, and requires a deliberate double-Esc (within 450 ms) to
/// latch `stop_flag`.
pub fn run_main_loop(kill_switch: Arc<EscKillSwitch>) -> ! {
    let mtm = MainThreadMarker::new().expect("run_main_loop must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let monitor_ks = Arc::clone(&kill_switch);
    let monitor_block = RcBlock::new(move |event: NonNull<NSEvent>| {
        // Fast-path when idle or only performing read-only observation: never latch `stop_flag`.
        if !monitor_ks.is_control_active() {
            return;
        }
        // SAFETY: `event` is a valid NSEvent delivered by AppKit's global monitor.
        let ev = unsafe { event.as_ref() };
        if is_synthetic_event(ev) {
            return;
        }
        if ev.keyCode() == 53 {
            if ev.isARepeat() {
                return;
            }
            if monitor_ks.on_physical_esc() {
                hide_overlay();
            }
        } else {
            monitor_ks.on_other_physical_key();
        }
    });

    // Keep the monitor retained for the lifetime of the daemon.
    let _monitor = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(
        NSEventMask::KeyDown,
        &monitor_block,
    );

    app.run();
    std::process::exit(0);
}

fn is_synthetic_event(ev: &NSEvent) -> bool {
    ev.CGEvent()
        .map(|cg| {
            let raw = (&*cg as *const objc2_core_graphics::CGEvent)
                .cast_mut()
                .cast();
            // SAFETY: `raw` is a valid CGEventRef owned by the NSEvent.
            let mark =
                unsafe { ffi::CGEventGetIntegerValueField(raw, K_CG_EVENT_SOURCE_USER_DATA) };
            mark == SYNTHETIC_MARK
        })
        .unwrap_or(false)
}

fn overlay_label_text(text: &str) -> String {
    format!("{text}  ·  Double-Esc or Stop")
}

fn show_or_update_overlay(mtm: MainThreadMarker, text: &str, kill_switch: Arc<EscKillSwitch>) {
    OVERLAY_PANEL.with(|slot| {
        let mut guard = slot.borrow_mut();
        if let Some(existing) = guard.as_ref() {
            let ns_text = NSString::from_str(&overlay_label_text(text));
            existing.label.setStringValue(&ns_text);
            existing.panel.orderFrontRegardless();
            return;
        }

        let visible = NSScreen::mainScreen(mtm)
            .map(|s| s.visibleFrame())
            .unwrap_or(NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(1440.0, 900.0),
            ));
        let width = 440.0;
        let height = 36.0;
        let button_width = 62.0;
        let frame = NSRect::new(
            NSPoint::new(
                visible.origin.x + visible.size.width - width - 14.0,
                visible.origin.y + 14.0,
            ),
            NSSize::new(width, height),
        );

        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        );
        // `NSStatusWindowLevel` = 25.
        panel.setLevel(25);
        panel.setFloatingPanel(true);
        panel.setHidesOnDeactivate(false);
        panel.setOpaque(false);
        panel.setHasShadow(true);
        // Allow mouse clicks on the non-activating pill so the Stop button works.
        panel.setIgnoresMouseEvents(false);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        let bg = NSColor::colorWithSRGBRed_green_blue_alpha(0.08, 0.09, 0.11, 0.88);
        panel.setBackgroundColor(Some(&bg));

        let ns_text = NSString::from_str(&overlay_label_text(text));
        let label = NSTextField::labelWithString(&ns_text, mtm);
        label.setFrame(NSRect::new(
            NSPoint::new(12.0, 9.0),
            NSSize::new(width - button_width - 28.0, 18.0),
        ));
        label.setTextColor(Some(&NSColor::whiteColor()));
        label.setFont(Some(&NSFont::systemFontOfSize(12.5)));

        let stop_target = StopButtonTarget::new(kill_switch);
        let target_obj: &AnyObject = &stop_target;
        let stop_title = NSString::from_str("Stop");
        // SAFETY: `stop_target` is an `AgentDesktopStopButtonTarget` implementing
        // `onStopClicked:` and is retained in `OverlayState` for the lifetime of `stop_button`.
        let stop_button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &stop_title,
                Some(target_obj),
                Some(sel!(onStopClicked:)),
                mtm,
            )
        };
        stop_button.setFrame(NSRect::new(
            NSPoint::new(width - button_width - 10.0, 6.0),
            NSSize::new(button_width, 24.0),
        ));
        stop_button.setFont(Some(&NSFont::systemFontOfSize(12.0)));
        let stop_bg = NSColor::colorWithSRGBRed_green_blue_alpha(0.85, 0.22, 0.22, 1.0);
        stop_button.setBezelColor(Some(&stop_bg));

        if let Some(content) = panel.contentView() {
            content.addSubview(&label);
            content.addSubview(&stop_button);
        }
        panel.orderFrontRegardless();
        *guard = Some(OverlayState {
            panel,
            label,
            _stop_button: stop_button,
            _stop_target: stop_target,
        });
    });
}

fn hide_overlay() {
    OVERLAY_PANEL.with(|slot| {
        if let Some(existing) = slot.borrow().as_ref() {
            existing.panel.orderOut(None);
        }
    });
}

fn run_ns_alert(mtm: MainThreadMarker, title: &str, message: &str) -> bool {
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Informational);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    alert.addButtonWithTitle(&NSString::from_str("Allow"));
    alert.addButtonWithTitle(&NSString::from_str("Deny"));
    alert.runModal() == NSAlertFirstButtonReturn
}

/// Fallback dialog when running headless without `run_main_loop` (only reached if Confirm mode is opted in).
pub fn confirm_dialog_fallback(title: &str, message: &str) -> Result<bool, RpcError> {
    let script = "on run argv\ndisplay dialog (item 1 of argv) with title (item 2 of argv) buttons {\"Deny\", \"Allow\"} default button \"Deny\" cancel button \"Deny\" giving up after 120\nreturn button returned of result\nend run";
    let output = std::process::Command::new("/usr/bin/osascript")
        .args(["-e", script, message, title])
        .output()
        .map_err(|e| RpcError::new(ErrorCode::BackendError, format!("osascript failed: {e}")))?;
    Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "Allow")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_label_mentions_double_esc_and_stop() {
        let label = overlay_label_text("agent controls: desktop");
        assert!(label.contains("Double-Esc or Stop"));
    }
}
