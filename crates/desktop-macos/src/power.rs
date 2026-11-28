use std::sync::Mutex;

use desktop_proto::{PermissionsStatus, RpcError, ScreenState};

use crate::ffi::{
    self, CfOwned, IOPMAssertionID, K_IOPM_ASSERTION_LEVEL_ON, cf_dict_get, cf_string, cf_to_bool,
};

/// Query live macOS TCC Accessibility and Screen Recording permissions.
#[must_use]
pub fn permissions() -> PermissionsStatus {
    // SAFETY: Both C functions take no pointers and are thread-safe status queries.
    unsafe {
        PermissionsStatus {
            accessibility: ffi::AXIsProcessTrusted(),
            screen_recording: ffi::CGPreflightScreenCaptureAccess(),
        }
    }
}

/// Ask macOS TCC to register `AgentDesktop.app` in both Accessibility and Screen Recording
/// System Settings lists and show the native permission prompts if not yet granted.
#[must_use]
pub fn request_permissions() -> PermissionsStatus {
    let key = cf_string("AXTrustedCheckOptionPrompt");
    let keys = [key.as_ptr()];
    let vals = [unsafe { ffi::kCFBooleanTrue }];
    // SAFETY: Creates a 1-entry CFDictionary `{ AXTrustedCheckOptionPrompt: true }` and calls
    // Apple's official TCC prompt APIs.
    let accessibility = unsafe {
        let opts = CfOwned::from_owned(ffi::CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            vals.as_ptr(),
            1,
            (&raw const ffi::kCFTypeDictionaryKeyCallBacks).cast(),
            (&raw const ffi::kCFTypeDictionaryValueCallBacks).cast(),
        ));
        if let Some(opts) = opts {
            ffi::AXIsProcessTrustedWithOptions(opts.as_ptr())
        } else {
            ffi::AXIsProcessTrusted()
        }
    };
    let screen_recording = unsafe { ffi::CGRequestScreenCaptureAccess() };
    PermissionsStatus {
        accessibility,
        screen_recording,
    }
}

/// Inspect whether the current GUI session is unlocked, locked (`loginwindow`), or has no active display.
#[must_use]
pub fn screen_state() -> ScreenState {
    // SAFETY: `CGSessionCopyCurrentDictionary` returns an owned CFDictionary or null.
    let dict = unsafe { CfOwned::from_owned(ffi::CGSessionCopyCurrentDictionary()) };
    if let Some(dict) = dict {
        // SAFETY: `dict` is a valid CFDictionaryRef.
        let locked_raw = unsafe { cf_dict_get(dict.as_ptr(), "CGSSessionScreenIsLocked") };
        if unsafe { cf_to_bool(locked_raw) } == Some(true) {
            return ScreenState::ScreenLocked;
        }
        let on_console_raw = unsafe { cf_dict_get(dict.as_ptr(), "kCGSSessionOnConsoleKey") };
        if unsafe { cf_to_bool(on_console_raw) } == Some(false) {
            return ScreenState::ScreenLocked;
        }
    }

    // Check whether at least one display is online/active.
    let mut count: u32 = 0;
    // SAFETY: Null display array queries the active display count.
    let rc = unsafe { ffi::CGGetActiveDisplayList(0, std::ptr::null_mut(), &mut count) };
    if rc == 0 && count == 0 {
        return ScreenState::DisplayAsleep;
    }

    ScreenState::Unlocked
}

/// Declare user activity to wake an unlocked sleeping display.
pub fn wake_display() -> Result<bool, RpcError> {
    let name = cf_string("agent-desktop wake display");
    let mut assertion_id: IOPMAssertionID = 0;
    // `kIOPMUserActiveLocal = 0`.
    // SAFETY: `name` is a valid CFString and `assertion_id` is a valid out-pointer.
    let rc = unsafe { ffi::IOPMAssertionDeclareUserActivity(name.as_ptr(), 0, &mut assertion_id) };
    if rc == 0 {
        // Give WindowServer a brief moment to bring the display surface online.
        std::thread::sleep(std::time::Duration::from_millis(120));
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Holds a `PreventUserIdleDisplaySleep` IOKit power assertion while an agent session is active.
pub struct DisplayAwakeGuard {
    assertion: Mutex<Option<IOPMAssertionID>>,
}

impl DisplayAwakeGuard {
    #[must_use]
    pub fn new() -> Self {
        Self {
            assertion: Mutex::new(None),
        }
    }

    pub fn set_active(&self, active: bool) {
        let mut guard = self.assertion.lock().unwrap_or_else(|e| e.into_inner());
        match (active, *guard) {
            (true, None) => {
                let kind = cf_string("PreventUserIdleDisplaySleep");
                let name = cf_string("agent-desktop active session");
                let mut id: IOPMAssertionID = 0;
                // SAFETY: Both strings are valid CFStrings and `id` is a valid out-pointer.
                let rc = unsafe {
                    ffi::IOPMAssertionCreateWithName(
                        kind.as_ptr(),
                        K_IOPM_ASSERTION_LEVEL_ON,
                        name.as_ptr(),
                        &mut id,
                    )
                };
                if rc == 0 {
                    *guard = Some(id);
                }
            }
            (false, Some(id)) => {
                // SAFETY: `id` was returned by `IOPMAssertionCreateWithName`.
                unsafe {
                    ffi::IOPMAssertionRelease(id);
                }
                *guard = None;
            }
            _ => {}
        }
    }
}

impl Drop for DisplayAwakeGuard {
    fn drop(&mut self) {
        self.set_active(false);
    }
}
