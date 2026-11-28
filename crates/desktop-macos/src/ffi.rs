//! Minimal, audited C FFI bindings to CoreFoundation, CoreGraphics, ApplicationServices,
//! ImageIO, and IOKit, wrapped in RAII types.

use std::ffi::c_void;
use std::ptr::{self, NonNull};

/// Marker stamped into `kCGEventSourceUserData` on every synthetic event posted by `desktopd`,
/// so the global Esc monitor ignores agent-generated Esc presses while catching physical user Esc.
pub const SYNTHETIC_MARK: i64 = 0x4453_4B54;

pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFBooleanRef = *const c_void;
pub type CFNumberRef = *const c_void;
pub type CFDataRef = *const c_void;
pub type CFMutableDataRef = *mut c_void;
pub type CFTypeID = usize;
pub type CFIndex = isize;
pub type AXUIElementRef = *mut c_void;
pub type AXObserverRef = *mut c_void;
pub type AXValueRef = *const c_void;
pub type AXError = i32;
pub type CFRunLoopRef = *mut c_void;
pub type CFRunLoopSourceRef = *mut c_void;
pub type AXObserverCallback = unsafe extern "C" fn(
    observer: AXObserverRef,
    element: AXUIElementRef,
    notification: CFStringRef,
    refcon: *mut c_void,
);
pub type CGDirectDisplayID = u32;
pub type CGDisplayModeRef = *mut c_void;
pub type CGEventSourceRef = *mut c_void;
pub type CGEventRef = *mut c_void;
pub type CGImageRef = *mut c_void;
pub type CGImageDestinationRef = *mut c_void;
pub type CGDataProviderRef = *const c_void;

/// Opaque type for CoreFoundation static callback tables (avoiding `&c_void` 1-byte provenance UB).
#[repr(C)]
pub struct CFOpaqueStatic {
    _private: [u8; 0],
}
pub type CGContextRef = *mut c_void;
pub type CGColorSpaceRef = *mut c_void;
pub type IOPMAssertionID = u32;

pub const K_AX_ERROR_SUCCESS: AXError = 0;
pub const K_AX_COPY_MULTIPLE_ATTRIBUTE_OPTION_NONE: u32 = 0;
pub const K_AX_VALUE_CGPOINT_TYPE: u32 = 1;
pub const K_AX_VALUE_CGSIZE_TYPE: u32 = 2;

pub const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
pub const K_CF_NUMBER_SINT64_TYPE: i32 = 4;
pub const K_CF_NUMBER_FLOAT64_TYPE: i32 = 6;

pub const K_CG_NULL_WINDOW_ID: u32 = 0;
pub const K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1 << 0;
pub const K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;

pub const K_CG_EVENT_SOURCE_STATE_HID_SYSTEM_STATE: i32 = 1;
pub const K_CG_HID_EVENT_TAP: u32 = 0;
pub const K_CG_EVENT_SOURCE_USER_DATA: u32 = 42;
pub const K_CG_MOUSE_EVENT_CLICK_STATE: u32 = 1;

pub const K_CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
pub const K_CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
pub const K_CG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
pub const K_CG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
pub const K_CG_EVENT_MOUSE_MOVED: u32 = 5;
pub const K_CG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
pub const K_CG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
pub const K_CG_EVENT_OTHER_MOUSE_UP: u32 = 26;

pub const K_CG_MOUSE_BUTTON_LEFT: u32 = 0;
pub const K_CG_MOUSE_BUTTON_RIGHT: u32 = 1;
pub const K_CG_MOUSE_BUTTON_CENTER: u32 = 2;

pub const K_CG_EVENT_FLAG_MASK_SHIFT: u64 = 0x0002_0000;
pub const K_CG_EVENT_FLAG_MASK_CONTROL: u64 = 0x0004_0000;
pub const K_CG_EVENT_FLAG_MASK_ALTERNATE: u64 = 0x0008_0000;
pub const K_CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x0010_0000;
pub const K_CG_EVENT_FLAG_MASK_SECONDARY_FN: u64 = 0x0080_0000;

pub const K_CG_SCROLL_EVENT_UNIT_LINE: u32 = 1;
pub const K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST: u32 = 1;
pub const K_CG_IMAGE_ALPHA_PREMULTIPLIED_FIRST: u32 = 2;
pub const K_CG_BITMAP_BYTE_ORDER_32_LITTLE: u32 = 2 << 12;
pub const K_CG_BITMAP_INFO_BGRA: u32 =
    K_CG_IMAGE_ALPHA_PREMULTIPLIED_FIRST | K_CG_BITMAP_BYTE_ORDER_32_LITTLE;

pub const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;

    pub fn CFStringGetTypeID() -> CFTypeID;
    pub fn CFStringCreateWithBytes(
        alloc: CFTypeRef,
        bytes: *const u8,
        num_bytes: CFIndex,
        encoding: u32,
        is_external_representation: u8,
    ) -> CFStringRef;
    pub fn CFStringGetLength(the_string: CFStringRef) -> CFIndex;
    pub fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
    pub fn CFStringGetCString(
        the_string: CFStringRef,
        buffer: *mut u8,
        buffer_size: CFIndex,
        encoding: u32,
    ) -> u8;

    pub fn CFArrayGetTypeID() -> CFTypeID;
    pub fn CFArrayGetCount(the_array: CFArrayRef) -> CFIndex;
    pub fn CFArrayGetValueAtIndex(the_array: CFArrayRef, idx: CFIndex) -> CFTypeRef;
    pub fn CFArrayCreate(
        allocator: CFTypeRef,
        values: *const CFTypeRef,
        num_values: CFIndex,
        callbacks: *const c_void,
    ) -> CFArrayRef;
    pub static kCFTypeArrayCallBacks: CFOpaqueStatic;

    pub static kCFRunLoopDefaultMode: CFStringRef;
    pub fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    pub fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    pub fn CFRunLoopRemoveSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    pub fn CFRunLoopStop(rl: CFRunLoopRef);
    pub fn CFRunLoopRunInMode(
        mode: CFStringRef,
        seconds: f64,
        return_after_source_handled: u8,
    ) -> i32;

    pub fn CFDictionaryGetValue(the_dict: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
    pub fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        num_values: CFIndex,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
    pub static kCFTypeDictionaryKeyCallBacks: CFOpaqueStatic;
    pub static kCFTypeDictionaryValueCallBacks: CFOpaqueStatic;

    pub fn CFBooleanGetTypeID() -> CFTypeID;
    pub fn CFBooleanGetValue(boolean: CFBooleanRef) -> u8;
    pub static kCFBooleanTrue: CFBooleanRef;

    pub fn CFNumberGetTypeID() -> CFTypeID;
    pub fn CFNumberCreate(
        allocator: CFTypeRef,
        the_type: i32,
        value_ptr: *const c_void,
    ) -> CFNumberRef;
    pub fn CFNumberGetValue(number: CFNumberRef, the_type: i32, value_ptr: *mut c_void) -> u8;

    pub fn CFDataCreateMutable(allocator: CFTypeRef, capacity: CFIndex) -> CFMutableDataRef;
    pub fn CFDataGetLength(the_data: CFDataRef) -> CFIndex;
    pub fn CFDataGetBytePtr(the_data: CFDataRef) -> *const u8;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    pub fn CGPreflightScreenCaptureAccess() -> bool;
    pub fn CGRequestScreenCaptureAccess() -> bool;
    pub fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;

    pub fn CGGetActiveDisplayList(
        max_displays: u32,
        active_displays: *mut CGDirectDisplayID,
        display_count: *mut u32,
    ) -> i32;
    pub fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
    pub fn CGDisplayIsMain(display: CGDirectDisplayID) -> u32;
    pub fn CGDisplayCopyDisplayMode(display: CGDirectDisplayID) -> CGDisplayModeRef;
    pub fn CGDisplayModeGetPixelWidth(mode: CGDisplayModeRef) -> usize;
    pub fn CGDisplayModeGetPixelHeight(mode: CGDisplayModeRef) -> usize;
    pub fn CGDisplayModeRelease(mode: CGDisplayModeRef);

    pub fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CFArrayRef;
    pub fn CGRectMakeWithDictionaryRepresentation(dict: CFDictionaryRef, rect: *mut CGRect)
    -> bool;

    pub fn CGEventSourceCreate(state_id: i32) -> CGEventSourceRef;
    pub fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    pub fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    pub fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        mouse_type: u32,
        mouse_cursor_position: CGPoint,
        mouse_button: u32,
    ) -> CGEventRef;
    pub fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        virtual_key: u16,
        key_down: bool,
    ) -> CGEventRef;
    pub fn CGEventKeyboardSetUnicodeString(
        event: CGEventRef,
        string_length: usize,
        unicode_string: *const u16,
    );
    pub fn CGEventCreateScrollWheelEvent2(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    pub fn CGEventSetFlags(event: CGEventRef, flags: u64);
    pub fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    pub fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    pub fn CGEventPost(tap: u32, event: CGEventRef);
    pub fn CGEventPostToPid(pid: i32, event: CGEventRef);

    pub fn CGImageGetWidth(image: CGImageRef) -> usize;
    pub fn CGImageGetHeight(image: CGImageRef) -> usize;
    pub fn CGImageGetBitsPerComponent(image: CGImageRef) -> usize;
    pub fn CGImageGetBitsPerPixel(image: CGImageRef) -> usize;
    pub fn CGImageGetBytesPerRow(image: CGImageRef) -> usize;
    pub fn CGImageGetBitmapInfo(image: CGImageRef) -> u32;
    pub fn CGImageGetDataProvider(image: CGImageRef) -> CGDataProviderRef;
    pub fn CGDataProviderCopyData(provider: CGDataProviderRef) -> CFDataRef;
    pub fn CGColorSpaceCreateDeviceRGB() -> CGColorSpaceRef;
    pub fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: CGColorSpaceRef,
        bitmap_info: u32,
    ) -> CGContextRef;
    pub fn CGContextDrawImage(c: CGContextRef, rect: CGRect, image: CGImageRef);
    pub fn CGBitmapContextCreateImage(context: CGContextRef) -> CGImageRef;
    pub fn CGBitmapContextGetData(context: CGContextRef) -> *mut u8;
    pub fn CGBitmapContextGetBytesPerRow(context: CGContextRef) -> usize;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    pub fn AXIsProcessTrusted() -> bool;
    pub fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    pub fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    pub fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    pub fn AXUIElementSetMessagingTimeout(
        element: AXUIElementRef,
        timeout_in_seconds: f32,
    ) -> AXError;
    pub fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    pub fn AXUIElementCopyMultipleAttributeValues(
        element: AXUIElementRef,
        attributes: CFArrayRef,
        options: u32,
        values: *mut CFArrayRef,
    ) -> AXError;
    pub fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> AXError;
    pub fn AXUIElementCopyActionNames(element: AXUIElementRef, names: *mut CFArrayRef) -> AXError;
    pub fn AXUIElementPerformAction(element: AXUIElementRef, action: CFStringRef) -> AXError;
    pub fn AXUIElementGetPid(element: AXUIElementRef, pid: *mut i32) -> AXError;
    pub fn AXUIElementGetTypeID() -> CFTypeID;
    pub fn AXValueGetTypeID() -> CFTypeID;
    pub fn AXValueGetValue(value: AXValueRef, the_type: u32, value_ptr: *mut c_void) -> bool;
    pub fn AXObserverCreate(
        application: i32,
        callback: AXObserverCallback,
        out_observer: *mut AXObserverRef,
    ) -> AXError;
    pub fn AXObserverAddNotification(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: CFStringRef,
        refcon: *mut c_void,
    ) -> AXError;
    pub fn AXObserverRemoveNotification(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: CFStringRef,
    ) -> AXError;
    pub fn AXObserverGetRunLoopSource(observer: AXObserverRef) -> CFRunLoopSourceRef;
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    pub static kCGImageDestinationLossyCompressionQuality: CFStringRef;
    pub fn CGImageDestinationCreateWithData(
        data: CFMutableDataRef,
        uttype: CFStringRef,
        count: usize,
        options: CFDictionaryRef,
    ) -> CGImageDestinationRef;
    pub fn CGImageDestinationAddImage(
        idst: CGImageDestinationRef,
        image: CGImageRef,
        properties: CFDictionaryRef,
    );
    pub fn CGImageDestinationFinalize(idst: CGImageDestinationRef) -> bool;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    pub fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        assertion_level: u32,
        assertion_name: CFStringRef,
        assertion_id: *mut IOPMAssertionID,
    ) -> i32;
    pub fn IOPMAssertionRelease(assertion_id: IOPMAssertionID) -> i32;
    pub fn IOPMAssertionDeclareUserActivity(
        assertion_name: CFStringRef,
        user_type: u32,
        assertion_id: *mut IOPMAssertionID,
    ) -> i32;
}

/// RAII wrapper for any owned CoreFoundation / CoreGraphics / AXUIElement reference.
pub struct CfOwned {
    ptr: NonNull<c_void>,
}

// SAFETY: CoreFoundation immutable values (CFString, CFData, CFNumber, CFDictionary, CGImage)
// and AXUIElementRef are thread-safe to retain, pass across threads, and release per Apple docs.
unsafe impl Send for CfOwned {}
unsafe impl Sync for CfOwned {}

impl CfOwned {
    /// Wrap a pointer returned by a CoreFoundation "Create" or "Copy" rule function.
    ///
    /// # Safety
    /// `ptr` must be a valid +1 owned CF object (or null, in which case `None` is returned).
    pub unsafe fn from_owned(ptr: CFTypeRef) -> Option<Self> {
        NonNull::new(ptr.cast_mut()).map(|ptr| Self { ptr })
    }

    /// Retain a borrowed CoreFoundation pointer ("Get" rule) into an owned wrapper.
    ///
    /// # Safety
    /// `ptr` must be null or a valid live CF object pointer.
    pub unsafe fn from_borrowed(ptr: CFTypeRef) -> Option<Self> {
        if ptr.is_null() {
            return None;
        }
        // SAFETY: caller guarantees `ptr` is a valid CF object; `CFRetain` increments its refcount.
        let retained = unsafe { CFRetain(ptr) };
        NonNull::new(retained.cast_mut()).map(|ptr| Self { ptr })
    }

    #[must_use]
    pub fn as_ptr(&self) -> CFTypeRef {
        self.ptr.as_ptr().cast_const()
    }

    #[must_use]
    pub fn as_mut_ptr(&self) -> *mut c_void {
        self.ptr.as_ptr()
    }
}

impl Clone for CfOwned {
    fn clone(&self) -> Self {
        // SAFETY: `self.ptr` is a valid retained CF object.
        let retained = unsafe { CFRetain(self.as_ptr()) };
        Self {
            ptr: NonNull::new(retained.cast_mut()).expect("CFRetain returned null"),
        }
    }
}

impl Drop for CfOwned {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` owns one retain count from creation/clone.
        unsafe { CFRelease(self.as_ptr()) };
    }
}

/// Create an owned `CFString` from a Rust `&str`.
pub fn cf_string(s: &str) -> CfOwned {
    // SAFETY: `s.as_ptr()` is valid UTF-8 for `s.len()` bytes; `CFStringCreateWithBytes` copies it.
    let raw = unsafe {
        CFStringCreateWithBytes(
            ptr::null(),
            s.as_ptr(),
            s.len() as CFIndex,
            K_CF_STRING_ENCODING_UTF8,
            0,
        )
    };
    // SAFETY: `raw` is a newly created +1 CFString.
    unsafe { CfOwned::from_owned(raw) }.expect("CFStringCreateWithBytes failed")
}

/// Convert a borrowed `CFTypeRef` to a Rust `String` if its type is `CFString`.
///
/// # Safety
/// `raw` must be null or a valid CFTypeRef.
pub unsafe fn cf_to_string(raw: CFTypeRef) -> Option<String> {
    if raw.is_null() {
        return None;
    }
    // SAFETY: caller guarantees `raw` is a valid CF object.
    unsafe {
        if CFGetTypeID(raw) != CFStringGetTypeID() {
            return None;
        }
        let len = CFStringGetLength(raw);
        let max = CFStringGetMaximumSizeForEncoding(len, K_CF_STRING_ENCODING_UTF8) + 1;
        let mut buf = vec![0u8; max.max(1) as usize];
        if CFStringGetCString(raw, buf.as_mut_ptr(), max, K_CF_STRING_ENCODING_UTF8) == 0 {
            return None;
        }
        let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        Some(String::from_utf8_lossy(&buf[..nul]).into_owned())
    }
}

/// Convert a borrowed `CFTypeRef` to `bool` if its type is `CFBoolean`.
///
/// # Safety
/// `raw` must be null or a valid CFTypeRef.
pub unsafe fn cf_to_bool(raw: CFTypeRef) -> Option<bool> {
    if raw.is_null() {
        return None;
    }
    // SAFETY: caller guarantees `raw` is a valid CF object.
    unsafe {
        if CFGetTypeID(raw) != CFBooleanGetTypeID() {
            return None;
        }
        Some(CFBooleanGetValue(raw) != 0)
    }
}

/// Convert a borrowed `CFTypeRef` to `i64` if its type is `CFNumber`.
///
/// # Safety
/// `raw` must be null or a valid CFTypeRef.
pub unsafe fn cf_to_i64(raw: CFTypeRef) -> Option<i64> {
    if raw.is_null() {
        return None;
    }
    // SAFETY: caller guarantees `raw` is a valid CF object.
    unsafe {
        if CFGetTypeID(raw) != CFNumberGetTypeID() {
            return None;
        }
        let mut val: i64 = 0;
        if CFNumberGetValue(raw, K_CF_NUMBER_SINT64_TYPE, (&mut val as *mut i64).cast()) != 0 {
            Some(val)
        } else {
            None
        }
    }
}

/// Convert a borrowed `CFTypeRef` to `f64` if its type is `CFNumber`.
///
/// # Safety
/// `raw` must be null or a valid CFTypeRef.
pub unsafe fn cf_to_f64(raw: CFTypeRef) -> Option<f64> {
    if raw.is_null() {
        return None;
    }
    // SAFETY: caller guarantees `raw` is a valid CF object.
    unsafe {
        if CFGetTypeID(raw) != CFNumberGetTypeID() {
            return None;
        }
        let mut val: f64 = 0.0;
        if CFNumberGetValue(raw, K_CF_NUMBER_FLOAT64_TYPE, (&mut val as *mut f64).cast()) != 0 {
            Some(val)
        } else {
            None
        }
    }
}

/// Look up a key by string name in a `CFDictionaryRef`.
///
/// # Safety
/// `dict` must be a valid `CFDictionaryRef`.
pub unsafe fn cf_dict_get(dict: CFDictionaryRef, key: &str) -> CFTypeRef {
    let k = cf_string(key);
    // SAFETY: `dict` is valid per caller and `k` is a live CFString.
    unsafe { CFDictionaryGetValue(dict, k.as_ptr()) }
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// Returns `true` if a process with `pid` currently exists (including processes owned by another
/// user where `kill(pid, 0)` returns `EPERM`).
#[must_use]
pub fn is_pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: POSIX `kill(pid, 0)` checks process existence without delivering any signal.
    if unsafe { kill(pid, 0) } == 0 {
        return true;
    }
    const EPERM: i32 = 1;
    std::io::Error::last_os_error().raw_os_error() == Some(EPERM)
}
