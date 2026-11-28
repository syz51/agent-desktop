use std::ptr;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use block2::RcBlock;
use desktop_core::{AnnotateBox, RawScreenshot, VisualSample, visual_signature_from_pixels};
use desktop_proto::{
    ErrorCode, FocusMode, ImageFormat, Rect, RpcError, ScreenshotTarget, fit_screenshot,
};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::CGRect as ObjcCGRect;
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration, SCWindow,
};

use crate::ffi::{
    self, CGImageRef, CGRect, CfOwned, K_CF_NUMBER_FLOAT64_TYPE, K_CG_BITMAP_INFO_BGRA,
    K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST, cf_string,
};
use crate::stream::{StreamKey, WarmCaptureStream};
use crate::windows;

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(6);
const SHAREABLE_CONTENT_TTL: Duration = Duration::from_millis(150);

struct SendContent(Retained<SCShareableContent>);

// SAFETY: `SCShareableContent` is an immutable snapshot object delivered on background GCD queues.
unsafe impl Send for SendContent {}

static CONTENT_CACHE: OnceLock<Mutex<Option<(Instant, SendContent)>>> = OnceLock::new();

/// Clear the cached `SCShareableContent` snapshot after window/app changes.
pub fn invalidate_shareable_content_cache() {
    if let Some(cache) = CONTENT_CACHE.get()
        && let Ok(mut guard) = cache.lock()
    {
        *guard = None;
    }
}

/// Capture a display, background/foreground window, app, or region in memory via ScreenCaptureKit.
///
/// Uses the on-demand warm `SCStream` frame buffer for sub-15 ms display and background-window grabs
/// when a fresh post-mutation frame is ready, and caches `SCShareableContent` briefly otherwise.
pub fn capture(
    stream: &WarmCaptureStream,
    target: &ScreenshotTarget,
    format: ImageFormat,
    annotate: &[AnnotateBox],
    focus_mode: FocusMode,
) -> Result<RawScreenshot, RpcError> {
    if !crate::power::permissions().screen_recording {
        return Err(RpcError::with_next(
            ErrorCode::PermissionDenied,
            "Screen Recording permission is not granted",
            "Grant Screen Recording to AgentDesktop.app in System Settings > Privacy & Security.",
        ));
    }

    let displays = windows::displays()?;

    // Fast path: serve live warm SCStream BGRA frame directly in memory (~3-10 ms!) for Display
    // and non-foreground Window/App targets when live bounds still match (no RGBA swizzle).
    if focus_mode != FocusMode::Foreground
        && let Some((key, expected_bounds)) = resolve_stream_target(target, &displays)?
        && let Some(frame) = stream.latest_frame(key, expected_bounds)
    {
        return encode_bgra_frame(
            &frame.bgra,
            frame.width,
            frame.height,
            frame.display_bounds,
            format,
            annotate,
            frame.visual_sig,
        );
    }

    let force_fresh_content = focus_mode == FocusMode::Foreground;
    let content = fetch_shareable_content(force_fresh_content)?;

    let (filter, config, out_w, out_h, display_bounds) =
        build_capture_plan(stream, &content, &displays, target, focus_mode)?;

    let cg_image = capture_image(&filter, &config)?;
    let (final_image, visual_sig) = if annotate.is_empty() {
        let sig = compute_cg_image_signature(&cg_image);
        (cg_image, sig)
    } else {
        match draw_annotations(&cg_image, display_bounds, annotate) {
            Some((img, sig)) => (img, sig),
            None => (cg_image, None),
        }
    };

    let bytes = encode_cg_image(&final_image, format)?;
    Ok(RawScreenshot {
        width: out_w,
        height: out_h,
        display_bounds,
        mime_type: format.mime_type().into(),
        bytes,
        visual_sig,
    })
}

/// Capture ONLY the 8x8 luma visual signature for `wait(visual_change)`, skipping ImageIO JPEG
/// encoding and SHA-256 hashing whenever the target is >= 8x8 pixels.
pub fn capture_visual_signature(
    stream: &WarmCaptureStream,
    target: &ScreenshotTarget,
) -> Result<VisualSample, RpcError> {
    if !crate::power::permissions().screen_recording {
        return Err(RpcError::with_next(
            ErrorCode::PermissionDenied,
            "Screen Recording permission is not granted",
            "Grant Screen Recording to AgentDesktop.app in System Settings > Privacy & Security.",
        ));
    }

    let displays = windows::displays()?;
    if let Some((key, expected_bounds)) = resolve_stream_target(target, &displays)?
        && let Some(sig) = stream.latest_visual_sig(key, expected_bounds)
    {
        return Ok(VisualSample {
            sha: None,
            visual_sig: Some(sig),
        });
    }

    let content = fetch_shareable_content(false)?;
    let (filter, config, _, _, _) =
        build_capture_plan(stream, &content, &displays, target, FocusMode::Background)?;
    let cg_image = capture_image(&filter, &config)?;
    let visual_sig = compute_cg_image_signature(&cg_image);
    // Fallback digest only for tiny (< 8x8 px) targets where an 8x8 thumbnail cannot be formed.
    let sha = if visual_sig.is_none() {
        encode_cg_image(&cg_image, ImageFormat::Png)
            .ok()
            .map(|bytes| short_fnv_digest(&bytes))
    } else {
        None
    };
    Ok(VisualSample { sha, visual_sig })
}

fn short_fnv_digest(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Wait up to `timeout` for the warm `SCStream` backing `target` to push a fresh frame.
pub fn wait_for_visual_frame(
    stream: &WarmCaptureStream,
    target: &ScreenshotTarget,
    timeout: Duration,
    stop: &AtomicBool,
) {
    if let Ok(displays) = windows::displays()
        && let Ok(Some((key, _))) = resolve_stream_target(target, &displays)
    {
        let _ = stream.wait_for_frame(key, timeout, stop);
        return;
    }
    std::thread::sleep(timeout.min(Duration::from_millis(40)));
}

fn resolve_stream_target(
    target: &ScreenshotTarget,
    displays: &[desktop_proto::DisplayInfo],
) -> Result<Option<(StreamKey, Option<Rect>)>, RpcError> {
    match target {
        ScreenshotTarget::Display { index } => {
            let disp_info = match index {
                Some(i) => displays.get(*i as usize).ok_or_else(|| {
                    RpcError::new(
                        ErrorCode::NotFound,
                        format!("display {i} not found ({} displays active)", displays.len()),
                    )
                })?,
                None => displays
                    .iter()
                    .find(|d| d.main)
                    .or_else(|| displays.first())
                    .ok_or_else(|| RpcError::new(ErrorCode::NotFound, "no active displays"))?,
            };
            Ok(Some((
                StreamKey::Display(disp_info.id),
                Some(disp_info.bounds),
            )))
        }
        ScreenshotTarget::Window { window_id } => {
            let live_bounds = windows::windows(None)
                .ok()
                .and_then(|wins| wins.into_iter().find(|w| w.id == *window_id))
                .map(|w| w.bounds);
            Ok(Some((StreamKey::Window(*window_id), live_bounds)))
        }
        ScreenshotTarget::App { app } => {
            let wins = windows::windows(Some(app))?;
            let first = wins
                .iter()
                .find(|w| w.layer == 0)
                .or_else(|| wins.first())
                .ok_or_else(|| {
                    RpcError::with_next(
                        ErrorCode::NotFound,
                        format!("no visible window found for app {app:?}"),
                        "Open the app or call list_windows to inspect available windows.",
                    )
                })?;
            Ok(Some((StreamKey::Window(first.id), Some(first.bounds))))
        }
        ScreenshotTarget::Region { .. } => Ok(None),
    }
}

#[allow(clippy::type_complexity)]
fn build_capture_plan(
    stream: &WarmCaptureStream,
    content: &SCShareableContent,
    displays: &[desktop_proto::DisplayInfo],
    target: &ScreenshotTarget,
    focus_mode: FocusMode,
) -> Result<
    (
        Retained<SCContentFilter>,
        Retained<SCStreamConfiguration>,
        u32,
        u32,
        Rect,
    ),
    RpcError,
> {
    Ok(match target {
        ScreenshotTarget::Display { index } => {
            let disp_info = match index {
                Some(i) => displays.get(*i as usize).ok_or_else(|| {
                    RpcError::new(
                        ErrorCode::NotFound,
                        format!("display {i} not found ({} displays active)", displays.len()),
                    )
                })?,
                None => displays
                    .iter()
                    .find(|d| d.main)
                    .or_else(|| displays.first())
                    .ok_or_else(|| RpcError::new(ErrorCode::NotFound, "no active displays"))?,
            };
            // SAFETY: `content` is a valid retained SCShareableContent snapshot.
            let sc_displays = unsafe { content.displays() };
            let sc_display = sc_displays
                .iter()
                // SAFETY: `displayID` is a read-only property on valid SCDisplay.
                .find(|d| unsafe { d.displayID() } == disp_info.id)
                .ok_or_else(|| {
                    RpcError::new(
                        ErrorCode::NotFound,
                        format!("ScreenCaptureKit display {} not found", disp_info.id),
                    )
                })?;
            // Warm up the persistent low-FPS SCStream in the background for subsequent grabs.
            stream.ensure_started_display(disp_info, &sc_display);
            let (w, h) = fit_screenshot(disp_info.pixel_width, disp_info.pixel_height);
            let empty_windows = NSArray::new();
            // SAFETY: `sc_display` and `empty_windows` are valid ObjC objects.
            let filter = unsafe {
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &sc_display,
                    &empty_windows,
                )
            };
            // SAFETY: Allocates and configures a new SCStreamConfiguration.
            let cfg = unsafe { SCStreamConfiguration::new() };
            // SAFETY: Configures capture dimensions and cursor visibility.
            unsafe {
                cfg.setWidth(w as usize);
                cfg.setHeight(h as usize);
                cfg.setShowsCursor(true);
            }
            (filter, cfg, w, h, disp_info.bounds)
        }
        ScreenshotTarget::Window { window_id } => {
            let plan = capture_window_filter(content, displays, *window_id, focus_mode)?;
            stream.ensure_started_window(
                *window_id,
                plan.display_bounds,
                plan.width,
                plan.height,
                &plan.sc_win,
            );
            (
                plan.filter,
                plan.config,
                plan.width,
                plan.height,
                plan.display_bounds,
            )
        }
        ScreenshotTarget::App { app } => {
            let wins = windows::windows(Some(app))?;
            let first = wins
                .iter()
                .find(|w| w.layer == 0)
                .or_else(|| wins.first())
                .ok_or_else(|| {
                    RpcError::with_next(
                        ErrorCode::NotFound,
                        format!("no visible window found for app {app:?}"),
                        "Open the app or call list_windows to inspect available windows.",
                    )
                })?;
            let plan = capture_window_filter(content, displays, first.id, focus_mode)?;
            stream.ensure_started_window(
                first.id,
                plan.display_bounds,
                plan.width,
                plan.height,
                &plan.sc_win,
            );
            (
                plan.filter,
                plan.config,
                plan.width,
                plan.height,
                plan.display_bounds,
            )
        }
        ScreenshotTarget::Region {
            x,
            y,
            width,
            height,
            display,
        } => {
            let disp_info = match display {
                Some(i) => displays.get(*i as usize).ok_or_else(|| {
                    RpcError::new(ErrorCode::NotFound, format!("display {i} not found"))
                })?,
                None => displays
                    .iter()
                    .find(|d| d.main)
                    .or_else(|| displays.first())
                    .ok_or_else(|| RpcError::new(ErrorCode::NotFound, "no active displays"))?,
            };
            let sc_displays = unsafe { content.displays() };
            let sc_display = sc_displays
                .iter()
                .find(|d| unsafe { d.displayID() } == disp_info.id)
                .ok_or_else(|| {
                    RpcError::new(ErrorCode::NotFound, "ScreenCaptureKit display not found")
                })?;
            let empty_windows = NSArray::new();
            let filter = unsafe {
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &sc_display,
                    &empty_windows,
                )
            };
            let scale = disp_info.scale.max(1.0);
            let out_w = (width * scale).round().max(1.0) as u32;
            let out_h = (height * scale).round().max(1.0) as u32;
            let cfg = unsafe { SCStreamConfiguration::new() };
            unsafe {
                cfg.setSourceRect(ObjcCGRect {
                    origin: objc2_core_foundation::CGPoint {
                        x: x - disp_info.bounds.x,
                        y: y - disp_info.bounds.y,
                    },
                    size: objc2_core_foundation::CGSize {
                        width: *width,
                        height: *height,
                    },
                });
                cfg.setWidth(out_w as usize);
                cfg.setHeight(out_h as usize);
                cfg.setShowsCursor(true);
            }
            (
                filter,
                cfg,
                out_w,
                out_h,
                Rect {
                    x: *x,
                    y: *y,
                    width: *width,
                    height: *height,
                },
            )
        }
    })
}

fn encode_bgra_frame(
    bgra: &[u8],
    width: u32,
    height: u32,
    display_bounds: Rect,
    format: ImageFormat,
    annotate: &[AnnotateBox],
    visual_sig: Option<[u8; 64]>,
) -> Result<RawScreenshot, RpcError> {
    let w = width as usize;
    let h = height as usize;
    let bpr = w * 4;
    if w == 0 || h == 0 || bgra.len() < h * bpr {
        return Err(RpcError::new(
            ErrorCode::BackendError,
            "invalid warm stream BGRA frame dimensions",
        ));
    }
    // SAFETY: Creates an owned DeviceRGB color space.
    let cs = unsafe { CfOwned::from_owned(ffi::CGColorSpaceCreateDeviceRGB().cast_const()) }
        .ok_or_else(|| {
            RpcError::new(
                ErrorCode::BackendError,
                "CGColorSpaceCreateDeviceRGB failed",
            )
        })?;
    // SAFETY: Creates an owned 32-bit little-endian premultiplied BGRA8 bitmap context backed by
    // CoreGraphics memory (matching ScreenCaptureKit's native pixel order with zero swizzle).
    let ctx = unsafe {
        CfOwned::from_owned(
            ffi::CGBitmapContextCreate(
                ptr::null_mut(),
                w,
                h,
                8,
                bpr,
                cs.as_mut_ptr(),
                K_CG_BITMAP_INFO_BGRA,
            )
            .cast_const(),
        )
    }
    .ok_or_else(|| RpcError::new(ErrorCode::BackendError, "CGBitmapContextCreate failed"))?;

    // SAFETY: Queries the backing data pointer of the valid bitmap context.
    let data = unsafe { ffi::CGBitmapContextGetData(ctx.as_mut_ptr()) };
    // SAFETY: Queries the row stride of the valid bitmap context.
    let ctx_bpr = unsafe { ffi::CGBitmapContextGetBytesPerRow(ctx.as_mut_ptr()) };
    if data.is_null() || ctx_bpr < bpr {
        return Err(RpcError::new(
            ErrorCode::BackendError,
            "CGBitmapContext backing buffer unavailable",
        ));
    }
    // SAFETY: `data` points to `h * ctx_bpr` writable BGRA bytes owned by `ctx`.
    let pixels = unsafe { std::slice::from_raw_parts_mut(data, h * ctx_bpr) };
    if ctx_bpr == bpr {
        pixels[..h * bpr].copy_from_slice(&bgra[..h * bpr]);
    } else {
        for y in 0..h {
            pixels[y * ctx_bpr..y * ctx_bpr + bpr].copy_from_slice(&bgra[y * bpr..(y + 1) * bpr]);
        }
    }
    if !annotate.is_empty() {
        desktop_core::draw_annotations_bgra(pixels, w, h, ctx_bpr, display_bounds, annotate);
    }

    // SAFETY: Produces an owned CGImage snapshot of the populated bitmap context.
    let cg_img = unsafe {
        CfOwned::from_owned(ffi::CGBitmapContextCreateImage(ctx.as_mut_ptr()).cast_const())
    }
    .ok_or_else(|| RpcError::new(ErrorCode::BackendError, "CGBitmapContextCreateImage failed"))?;
    let bytes = encode_cg_image(&cg_img, format)?;
    Ok(RawScreenshot {
        width,
        height,
        display_bounds,
        mime_type: format.mime_type().into(),
        bytes,
        visual_sig,
    })
}

struct CapturePlan {
    filter: Retained<SCContentFilter>,
    config: Retained<SCStreamConfiguration>,
    sc_win: Retained<SCWindow>,
    width: u32,
    height: u32,
    display_bounds: Rect,
}

fn capture_window_filter(
    content: &SCShareableContent,
    displays: &[desktop_proto::DisplayInfo],
    window_id: u32,
    focus_mode: FocusMode,
) -> Result<CapturePlan, RpcError> {
    // If foreground focus is requested, focus the window first and refresh shareable content so
    // the returned window frame reflects its live post-focus geometry.
    let fresh_after_focus = if focus_mode == FocusMode::Foreground {
        let _ = windows::focus_window(window_id);
        Some(fetch_shareable_content(true)?)
    } else {
        None
    };
    let active_content = fresh_after_focus.as_deref().unwrap_or(content);
    // SAFETY: `active_content` is a valid retained SCShareableContent snapshot.
    let sc_windows = unsafe { active_content.windows() };
    let maybe_win = sc_windows
        .iter()
        // SAFETY: `windowID` is a read-only property on valid SCWindow.
        .find(|w| unsafe { w.windowID() } == window_id);

    // On a cached miss (e.g. a window opened within the 150 ms cache TTL), refresh once live.
    let refreshed;
    let sc_win = if let Some(found) = maybe_win {
        found
    } else if fresh_after_focus.is_none() {
        refreshed = fetch_shareable_content(true)?;
        // SAFETY: `refreshed` is a valid retained SCShareableContent snapshot.
        let live_windows = unsafe { refreshed.windows() };
        live_windows
            .iter()
            // SAFETY: `windowID` is a read-only property on valid SCWindow.
            .find(|w| unsafe { w.windowID() } == window_id)
            .ok_or_else(|| {
                RpcError::with_next(
                    ErrorCode::NotFound,
                    format!("window w{window_id} is not available in ScreenCaptureKit"),
                    "The window may be minimized or on another Space. Unminimize it or switch to its Space.",
                )
            })?
    } else {
        return Err(RpcError::with_next(
            ErrorCode::NotFound,
            format!("window w{window_id} is not available in ScreenCaptureKit"),
            "The window may be minimized or on another Space. Unminimize it or switch to its Space.",
        ));
    };

    // SAFETY: `frame` is a read-only property on valid SCWindow.
    let frame = unsafe { sc_win.frame() };
    let bounds = Rect {
        x: frame.origin.x,
        y: frame.origin.y,
        width: frame.size.width.max(1.0),
        height: frame.size.height.max(1.0),
    };
    let center = bounds.center();
    let scale = displays
        .iter()
        .find(|d| d.bounds.contains(center))
        .or_else(|| displays.first())
        .map_or(1.0, |d| d.scale.max(1.0));
    let (w, h) = fit_screenshot(
        (bounds.width * scale).round() as u32,
        (bounds.height * scale).round() as u32,
    );
    // `desktopIndependentWindow` captures occluded/background windows cleanly without focusing!
    let filter = unsafe {
        SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &sc_win)
    };
    let cfg = unsafe { SCStreamConfiguration::new() };
    unsafe {
        cfg.setWidth(w as usize);
        cfg.setHeight(h as usize);
        cfg.setScalesToFit(true);
        cfg.setShowsCursor(false);
    }
    Ok(CapturePlan {
        filter,
        config: cfg,
        sc_win,
        width: w,
        height: h,
        display_bounds: bounds,
    })
}

fn fetch_shareable_content(force_fresh: bool) -> Result<Retained<SCShareableContent>, RpcError> {
    let cache = CONTENT_CACHE.get_or_init(|| Mutex::new(None));
    if !force_fresh
        && let Ok(guard) = cache.lock()
        && let Some((ts, cached)) = guard.as_ref()
        && ts.elapsed() <= SHAREABLE_CONTENT_TTL
    {
        return Ok(cached.0.clone());
    }

    let (tx, rx) = mpsc::sync_channel::<Result<Retained<SCShareableContent>, String>>(1);
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let res = if !err.is_null() {
            // SAFETY: `err` is a non-null NSError passed by ScreenCaptureKit.
            let msg = unsafe { (&*err).localizedDescription().to_string() };
            Err(msg)
        } else if !content.is_null() {
            // SAFETY: Retain the borrowed SCShareableContent pointer across the thread boundary.
            unsafe { Retained::retain(content) }.ok_or_else(|| "null SCShareableContent".into())
        } else {
            Err("empty SCShareableContent callback".into())
        };
        let _ = tx.send(res);
    });

    // SAFETY: `block` is a valid heap-allocated RcBlock matching Apple's completion handler ABI.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false,
            true,
            &block,
        );
    }

    match rx.recv_timeout(CAPTURE_TIMEOUT) {
        Ok(Ok(content)) => {
            if let Ok(mut guard) = cache.lock() {
                *guard = Some((Instant::now(), SendContent(content.clone())));
            }
            Ok(content)
        }
        Ok(Err(msg)) => Err(RpcError::with_next(
            ErrorCode::BackendError,
            format!("SCShareableContent failed: {msg}"),
            "Ensure Screen Recording is granted and a display is active.",
        )),
        Err(_) => Err(RpcError::new(
            ErrorCode::Timeout,
            "timed out waiting for SCShareableContent",
        )),
    }
}

fn capture_image(
    filter: &SCContentFilter,
    config: &SCStreamConfiguration,
) -> Result<CfOwned, RpcError> {
    let (tx, rx) = mpsc::sync_channel::<Result<CfOwned, String>>(1);
    let block = RcBlock::new(move |image: *mut CGImage, err: *mut NSError| {
        let res = if !err.is_null() {
            // SAFETY: `err` is a valid NSError from SCScreenshotManager.
            let msg = unsafe { (&*err).localizedDescription().to_string() };
            Err(msg)
        } else if !image.is_null() {
            // SAFETY: Retain the CGImageRef returned to the callback so it outlives the callback.
            unsafe { CfOwned::from_borrowed(image.cast()) }
                .ok_or_else(|| "failed to retain CGImage".into())
        } else {
            Err("SCScreenshotManager returned null image".into())
        };
        let _ = tx.send(res);
    });

    // SAFETY: `filter`, `config`, and `block` are valid retained Objective-C objects.
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            filter,
            config,
            Some(&block),
        );
    }

    match rx.recv_timeout(CAPTURE_TIMEOUT) {
        Ok(Ok(img)) => Ok(img),
        Ok(Err(msg)) => Err(RpcError::with_next(
            ErrorCode::BackendError,
            format!("screen capture failed: {msg}"),
            "Ensure Screen Recording is granted and the target display/window is active (not minimized or locked).",
        )),
        Err(_) => Err(RpcError::new(
            ErrorCode::Timeout,
            "timed out waiting for SCScreenshotManager",
        )),
    }
}

/// Encode a `CGImageRef` directly into JPEG or PNG bytes in memory using Apple ImageIO.
fn encode_cg_image(image: &CfOwned, format: ImageFormat) -> Result<Vec<u8>, RpcError> {
    // SAFETY: Creates an empty mutable CFData buffer.
    let data =
        unsafe { CfOwned::from_owned(ffi::CFDataCreateMutable(ptr::null(), 0).cast_const()) }
            .ok_or_else(|| RpcError::new(ErrorCode::BackendError, "CFDataCreateMutable failed"))?;

    let uti = cf_string(match format {
        ImageFormat::Jpeg => "public.jpeg",
        ImageFormat::Png => "public.png",
    });

    // SAFETY: `data` and `uti` are valid CF refs.
    let dest = unsafe {
        CfOwned::from_owned(
            ffi::CGImageDestinationCreateWithData(data.as_mut_ptr(), uti.as_ptr(), 1, ptr::null())
                .cast_const(),
        )
    }
    .ok_or_else(|| {
        RpcError::new(
            ErrorCode::BackendError,
            "CGImageDestinationCreateWithData failed",
        )
    })?;

    let props = if format == ImageFormat::Jpeg {
        let quality: f64 = 0.82;
        // SAFETY: Creates a CFNumber wrapping f64 quality and a 1-entry CFDictionary using
        // ImageIO's official `kCGImageDestinationLossyCompressionQuality` key.
        let num = unsafe {
            CfOwned::from_owned(ffi::CFNumberCreate(
                ptr::null(),
                K_CF_NUMBER_FLOAT64_TYPE,
                (&quality as *const f64).cast(),
            ))
        };
        if let Some(num) = num {
            let key = unsafe { ffi::kCGImageDestinationLossyCompressionQuality };
            let keys = [key];
            let vals = [num.as_ptr()];
            unsafe {
                CfOwned::from_owned(ffi::CFDictionaryCreate(
                    ptr::null(),
                    keys.as_ptr(),
                    vals.as_ptr(),
                    1,
                    (&raw const ffi::kCFTypeDictionaryKeyCallBacks).cast(),
                    (&raw const ffi::kCFTypeDictionaryValueCallBacks).cast(),
                ))
            }
        } else {
            None
        }
    } else {
        None
    };

    let props_ptr = props.as_ref().map_or(ptr::null(), CfOwned::as_ptr);
    // SAFETY: `dest` and `image` are valid retained objects; `props_ptr` is null or valid CFDictionary.
    unsafe {
        ffi::CGImageDestinationAddImage(dest.as_mut_ptr(), image.as_mut_ptr(), props_ptr);
        if !ffi::CGImageDestinationFinalize(dest.as_mut_ptr()) {
            return Err(RpcError::new(
                ErrorCode::BackendError,
                "CGImageDestinationFinalize failed",
            ));
        }
        let len = ffi::CFDataGetLength(data.as_ptr()).max(0) as usize;
        let ptr = ffi::CFDataGetBytePtr(data.as_ptr());
        if ptr.is_null() || len == 0 {
            return Err(RpcError::new(
                ErrorCode::BackendError,
                "encoded image buffer is empty",
            ));
        }
        Ok(std::slice::from_raw_parts(ptr, len).to_vec())
    }
}

/// Compute the 8x8 area-averaged visual signature directly from `CGImage`'s backing pixel data
/// (avoiding full-frame `CGContextDrawImage` rasterization while preserving exact pixel-grid
/// parity with `WarmCaptureStream`'s `visual_signature_from_pixels`).
fn compute_cg_image_signature(image: &CfOwned) -> Option<[u8; 64]> {
    // SAFETY: Queries dimensions and pixel layout of the valid CGImage.
    let (w, h, bpc, bpp, bpr, info) = unsafe {
        (
            ffi::CGImageGetWidth(image.as_mut_ptr()),
            ffi::CGImageGetHeight(image.as_mut_ptr()),
            ffi::CGImageGetBitsPerComponent(image.as_mut_ptr()),
            ffi::CGImageGetBitsPerPixel(image.as_mut_ptr()),
            ffi::CGImageGetBytesPerRow(image.as_mut_ptr()),
            ffi::CGImageGetBitmapInfo(image.as_mut_ptr()),
        )
    };
    if w < 8 || h < 8 {
        return None;
    }

    // Fast direct-provider path for standard 32-bpp 8-bpc RGBA/BGRA images: identical math to
    // `WarmCaptureStream` without creating a bitmap context or rasterizing the full image.
    let alpha = info & 0x1F;
    let byte_order = info & 0x7000;
    let direct_is_bgra = if bpc == 8 && bpp == 32 && bpr >= w * 4 {
        if byte_order == ffi::K_CG_BITMAP_BYTE_ORDER_32_LITTLE && matches!(alpha, 2 | 4 | 6) {
            Some(true)
        } else if (byte_order == 0 || byte_order == 0x4000) && matches!(alpha, 1 | 3 | 5) {
            Some(false)
        } else {
            None
        }
    } else {
        None
    };

    if let Some(is_bgra) = direct_is_bgra {
        // SAFETY: `CGImageGetDataProvider` returns a borrowed provider valid while `image` lives;
        // `CGDataProviderCopyData` returns an owned +1 CFDataRef.
        let provider = unsafe { ffi::CGImageGetDataProvider(image.as_mut_ptr()) };
        if !provider.is_null()
            && let Some(data) =
                (unsafe { CfOwned::from_owned(ffi::CGDataProviderCopyData(provider)) })
        {
            // SAFETY: Queries byte length and pointer of the owned CFData.
            let len = unsafe { ffi::CFDataGetLength(data.as_ptr()).max(0) as usize };
            let ptr = unsafe { ffi::CFDataGetBytePtr(data.as_ptr()) };
            if !ptr.is_null() && len >= h * bpr {
                // SAFETY: `ptr` is valid for `len` bytes owned by `data`.
                let pixels = unsafe { std::slice::from_raw_parts(ptr, len) };
                if let Some(sig) = visual_signature_from_pixels(pixels, w, h, bpr, is_bgra) {
                    return Some(sig);
                }
            }
        }
    }

    // Fallback for uncommon pixel formats (e.g. 16-bit HDR or grayscale).
    let cs = unsafe { CfOwned::from_owned(ffi::CGColorSpaceCreateDeviceRGB().cast_const()) }?;
    let ctx = unsafe {
        CfOwned::from_owned(
            ffi::CGBitmapContextCreate(
                ptr::null_mut(),
                w,
                h,
                8,
                w * 4,
                cs.as_mut_ptr(),
                K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST,
            )
            .cast_const(),
        )
    }?;
    let rect = CGRect {
        origin: ffi::CGPoint { x: 0.0, y: 0.0 },
        size: ffi::CGSize {
            width: w as f64,
            height: h as f64,
        },
    };
    unsafe {
        ffi::CGContextDrawImage(ctx.as_mut_ptr(), rect, image.as_mut_ptr());
    }
    let data = unsafe { ffi::CGBitmapContextGetData(ctx.as_mut_ptr()) };
    let ctx_bpr = unsafe { ffi::CGBitmapContextGetBytesPerRow(ctx.as_mut_ptr()) };
    if data.is_null() || ctx_bpr < w * 4 {
        return None;
    }
    let pixels = unsafe { std::slice::from_raw_parts(data, h * ctx_bpr) };
    visual_signature_from_pixels(pixels, w, h, ctx_bpr, false)
}

/// Draw crisp Set-of-Marks bounding boxes and `@eN` tags directly onto an RGBA bitmap copy,
/// returning both the annotated `CGImage` and the pre-annotation 8x8 visual signature.
fn draw_annotations(
    image: &CfOwned,
    display_bounds: Rect,
    boxes: &[AnnotateBox],
) -> Option<(CfOwned, Option<[u8; 64]>)> {
    if display_bounds.is_empty() {
        return None;
    }
    // SAFETY: Query dimensions of the valid CGImage.
    let img_w = unsafe { ffi::CGImageGetWidth(image.as_mut_ptr()) };
    // SAFETY: Query dimensions of the valid CGImage.
    let img_h = unsafe { ffi::CGImageGetHeight(image.as_mut_ptr()) };
    if img_w == 0 || img_h == 0 {
        return None;
    }
    // SAFETY: Creates an owned DeviceRGB color space.
    let cs = unsafe { CfOwned::from_owned(ffi::CGColorSpaceCreateDeviceRGB().cast_const()) }?;
    // SAFETY: Creates an owned RGBA8 bitmap context matching the CGImage dimensions.
    let ctx = unsafe {
        CfOwned::from_owned(
            ffi::CGBitmapContextCreate(
                ptr::null_mut(),
                img_w,
                img_h,
                8,
                img_w * 4,
                cs.as_mut_ptr(),
                K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST,
            )
            .cast_const(),
        )
    }?;

    // Draw original image into RGBA bitmap.
    let full = CGRect {
        origin: ffi::CGPoint { x: 0.0, y: 0.0 },
        size: ffi::CGSize {
            width: img_w as f64,
            height: img_h as f64,
        },
    };
    // SAFETY: `ctx` and `image` are valid CoreGraphics objects.
    unsafe {
        ffi::CGContextDrawImage(ctx.as_mut_ptr(), full, image.as_mut_ptr());
    }

    // SAFETY: Queries the writable backing buffer pointer owned by `ctx`.
    let data = unsafe { ffi::CGBitmapContextGetData(ctx.as_mut_ptr()) };
    // SAFETY: Queries row stride of the valid bitmap context.
    let bpr = unsafe { ffi::CGBitmapContextGetBytesPerRow(ctx.as_mut_ptr()) };
    if data.is_null() || bpr < img_w * 4 {
        return None;
    }
    // SAFETY: `data` points to `img_h * bpr` writable RGBA bytes owned by `ctx`.
    let pixels = unsafe { std::slice::from_raw_parts_mut(data, img_h * bpr) };
    let visual_sig = visual_signature_from_pixels(pixels, img_w, img_h, bpr, false);
    desktop_core::draw_annotations_rgba(pixels, img_w, img_h, bpr, display_bounds, boxes);

    // SAFETY: Creates a new CGImage from the annotated bitmap context.
    let out = unsafe {
        CfOwned::from_owned(ffi::CGBitmapContextCreateImage(ctx.as_mut_ptr()).cast_const())
    }?;
    Some((out, visual_sig))
}

#[allow(dead_code)]
fn _keep_anyobject(_: Option<&AnyObject>, _: Option<CGImageRef>) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_solid_image(w: usize, h: usize, rgba: [u8; 4]) -> CfOwned {
        let cs = unsafe { CfOwned::from_owned(ffi::CGColorSpaceCreateDeviceRGB().cast_const()) }
            .unwrap();
        let ctx = unsafe {
            CfOwned::from_owned(
                ffi::CGBitmapContextCreate(
                    ptr::null_mut(),
                    w,
                    h,
                    8,
                    w * 4,
                    cs.as_mut_ptr(),
                    K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST,
                )
                .cast_const(),
            )
        }
        .unwrap();
        let data = unsafe { ffi::CGBitmapContextGetData(ctx.as_mut_ptr()) };
        let bpr = unsafe { ffi::CGBitmapContextGetBytesPerRow(ctx.as_mut_ptr()) };
        let buf = unsafe { std::slice::from_raw_parts_mut(data, h * bpr) };
        for y in 0..h {
            for x in 0..w {
                let off = y * bpr + x * 4;
                buf[off..off + 4].copy_from_slice(&rgba);
            }
        }
        unsafe {
            CfOwned::from_owned(ffi::CGBitmapContextCreateImage(ctx.as_mut_ptr()).cast_const())
        }
        .unwrap()
    }

    #[test]
    fn encodes_jpeg_and_png_in_memory_and_draws_top_left_annotations() {
        let img = make_solid_image(100, 100, [20, 40, 80, 255]);

        let jpeg = encode_cg_image(&img, ImageFormat::Jpeg).unwrap();
        assert!(jpeg.len() > 100);
        assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF], "valid JPEG SOI header");

        let png = encode_cg_image(&img, ImageFormat::Png).unwrap();
        assert!(png.len() > 100);
        assert_eq!(
            &png[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "valid PNG signature"
        );

        let (annotated, _) = draw_annotations(
            &img,
            Rect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
            &[AnnotateBox {
                ref_id: "@e1".into(),
                bounds: Rect {
                    x: 10.0,
                    y: 20.0,
                    width: 30.0,
                    height: 20.0,
                },
            }],
        )
        .expect("annotation succeeded");

        // Verify annotated bytes differ from plain solid image and remain valid PNG.
        let ann_png = encode_cg_image(&annotated, ImageFormat::Png).unwrap();
        assert_ne!(ann_png, png);
        assert_eq!(&ann_png[..4], &[0x89, b'P', b'N', b'G']);
    }

    #[test]
    fn encodes_native_bgra_frame_and_downscaled_signature_detects_change() {
        let w = 64u32;
        let h = 64u32;
        let bgra = [80u8, 40, 20, 255].repeat((w * h) as usize);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            width: 64.0,
            height: 64.0,
        };
        let shot = encode_bgra_frame(
            &bgra,
            w,
            h,
            bounds,
            ImageFormat::Png,
            &[AnnotateBox {
                ref_id: "@e1".into(),
                bounds: Rect {
                    x: 8.0,
                    y: 16.0,
                    width: 24.0,
                    height: 16.0,
                },
            }],
            Some([30u8; 64]),
        )
        .unwrap();
        assert_eq!(&shot.bytes[..4], &[0x89, b'P', b'N', b'G']);
        assert_eq!(shot.visual_sig, Some([30u8; 64]));

        let dark = make_solid_image(128, 128, [30, 30, 30, 255]);
        let bright = make_solid_image(128, 128, [220, 220, 220, 255]);
        let sig_dark = compute_cg_image_signature(&dark).unwrap();
        let sig_bright = compute_cg_image_signature(&bright).unwrap();
        assert!(desktop_core::max_visual_delta(&sig_dark, &sig_bright) >= 150);

        // Verify direct-provider CGImage signature matches raw pixel signature with 0 delta even
        // on sharp 1-pixel high-contrast stripes (preventing SCStream vs one-shot false diffs).
        let sw = 128usize;
        let sh = 128usize;
        let cs = unsafe { CfOwned::from_owned(ffi::CGColorSpaceCreateDeviceRGB().cast_const()) }
            .unwrap();
        let ctx = unsafe {
            CfOwned::from_owned(
                ffi::CGBitmapContextCreate(
                    ptr::null_mut(),
                    sw,
                    sh,
                    8,
                    sw * 4,
                    cs.as_mut_ptr(),
                    K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST,
                )
                .cast_const(),
            )
        }
        .unwrap();
        let data = unsafe { ffi::CGBitmapContextGetData(ctx.as_mut_ptr()) };
        let bpr = unsafe { ffi::CGBitmapContextGetBytesPerRow(ctx.as_mut_ptr()) };
        let buf = unsafe { std::slice::from_raw_parts_mut(data, sh * bpr) };
        for y in 0..sh {
            for x in 0..sw {
                let v = if (x + y) % 2 == 0 { 245u8 } else { 15u8 };
                let off = y * bpr + x * 4;
                buf[off..off + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let raw_sig = visual_signature_from_pixels(buf, sw, sh, bpr, false).unwrap();
        let cg_striped = unsafe {
            CfOwned::from_owned(ffi::CGBitmapContextCreateImage(ctx.as_mut_ptr()).cast_const())
        }
        .unwrap();
        let cg_sig = compute_cg_image_signature(&cg_striped).unwrap();
        assert_eq!(
            desktop_core::max_visual_delta(&raw_sig, &cg_sig),
            0,
            "CGImage and raw pixel visual signatures must match identically"
        );
    }
}
