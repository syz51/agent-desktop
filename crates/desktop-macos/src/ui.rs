use std::cell::RefCell;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use block2::RcBlock;
use desktop_proto::{ErrorCode, RpcError};
use dispatch2::DispatchQueue;
use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSApplicationActivationPolicy,
    NSBackingStoreType, NSColor, NSEvent, NSEventMask, NSFont, NSPanel, NSScreen, NSTextField,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use crate::ffi::{self, K_CG_EVENT_SOURCE_USER_DATA, SYNTHETIC_MARK};

/// Handle allowing Tokio worker threads to safely dispatch AppKit UI updates onto the main thread.
#[derive(Clone)]
pub struct UiHandle {
    stop_flag: Arc<AtomicBool>,
}

thread_local! {
    static OVERLAY_PANEL: RefCell<Option<OverlayState>> = const { RefCell::new(None) };
}

struct OverlayState {
    panel: Retained<NSPanel>,
    label: Retained<NSTextField>,
}

impl UiHandle {
    #[must_use]
    pub fn new(stop_flag: Arc<AtomicBool>) -> Self {
        Self { stop_flag }
    }

    pub fn set_overlay(&self, text: Option<&str>) {
        let owned = text.map(str::to_string);
        DispatchQueue::main().exec_async(move || {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            match owned {
                Some(label_text) => show_or_update_overlay(mtm, &label_text),
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
        self.stop_flag.store(true, Ordering::Release);
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
/// Registers a global physical-Esc monitor that flips `stop_flag` immediately while ignoring
/// synthetic Esc events stamped with `SYNTHETIC_MARK`.
pub fn run_main_loop(stop_flag: Arc<AtomicBool>) -> ! {
    let mtm = MainThreadMarker::new().expect("run_main_loop must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let esc_flag = Arc::clone(&stop_flag);
    let monitor_block = RcBlock::new(move |event: NonNull<NSEvent>| {
        // SAFETY: `event` is a valid NSEvent delivered by AppKit's global monitor.
        let ev = unsafe { event.as_ref() };
        if ev.keyCode() == 53 {
            let is_synthetic = ev
                .CGEvent()
                .map(|cg| {
                    let raw = (&*cg as *const objc2_core_graphics::CGEvent)
                        .cast_mut()
                        .cast();
                    // SAFETY: `raw` is a valid CGEventRef owned by the NSEvent.
                    let mark = unsafe {
                        ffi::CGEventGetIntegerValueField(raw, K_CG_EVENT_SOURCE_USER_DATA)
                    };
                    mark == SYNTHETIC_MARK
                })
                .unwrap_or(false);
            if !is_synthetic {
                esc_flag.store(true, Ordering::Release);
                hide_overlay();
            }
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

fn show_or_update_overlay(mtm: MainThreadMarker, text: &str) {
    OVERLAY_PANEL.with(|slot| {
        let mut guard = slot.borrow_mut();
        if let Some(existing) = guard.as_ref() {
            let ns_text = NSString::from_str(&format!("{text}  ·  Esc to stop"));
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
        let width = 380.0;
        let height = 36.0;
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
        panel.setIgnoresMouseEvents(true);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        let bg = NSColor::colorWithSRGBRed_green_blue_alpha(0.08, 0.09, 0.11, 0.86);
        panel.setBackgroundColor(Some(&bg));

        let ns_text = NSString::from_str(&format!("{text}  ·  Esc to stop"));
        let label = NSTextField::labelWithString(&ns_text, mtm);
        label.setFrame(NSRect::new(
            NSPoint::new(12.0, 9.0),
            NSSize::new(width - 24.0, 18.0),
        ));
        label.setTextColor(Some(&NSColor::whiteColor()));
        label.setFont(Some(&NSFont::systemFontOfSize(12.5)));

        if let Some(content) = panel.contentView() {
            content.addSubview(&label);
        }
        panel.orderFrontRegardless();
        *guard = Some(OverlayState { panel, label });
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
