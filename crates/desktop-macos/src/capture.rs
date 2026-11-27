use std::ptr;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use desktop_core::{AnnotateBox, RawScreenshot};
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
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
};

use crate::ffi::{
    self, CGImageRef, CGRect, CfOwned, K_CF_NUMBER_FLOAT64_TYPE,
    K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST, cf_string,
};
use crate::windows;

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(6);

/// Capture a display, background/foreground window, app, or region in memory via ScreenCaptureKit.
pub fn capture(
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

    let content = fetch_shareable_content()?;
    let displays = windows::displays()?;

    let (filter, config, out_w, out_h, display_bounds) = match target {
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
            let sc_displays = unsafe { content.displays() };
            let sc_display = sc_displays
                .iter()
                .find(|d| unsafe { d.displayID() } == disp_info.id)
                .ok_or_else(|| {
                    RpcError::new(
                        ErrorCode::NotFound,
                        format!("ScreenCaptureKit display {} not found", disp_info.id),
                    )
                })?;
            let (w, h) = fit_screenshot(disp_info.pixel_width, disp_info.pixel_height);
            let empty_windows = NSArray::new();
            let filter = unsafe {
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &sc_display,
                    &empty_windows,
                )
            };
            let cfg = unsafe { SCStreamConfiguration::new() };
            unsafe {
                cfg.setWidth(w as usize);
                cfg.setHeight(h as usize);
                cfg.setShowsCursor(true);
            }
            (filter, cfg, w, h, disp_info.bounds)
        }
        ScreenshotTarget::Window { window_id } => {
            let plan = capture_window_filter(&content, &displays, *window_id, focus_mode)?;
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
            let first = wins.first().ok_or_else(|| {
                RpcError::with_next(
                    ErrorCode::NotFound,
                    format!("no visible window found for app {app:?}"),
                    "Open the app or call list_windows to inspect available windows.",
                )
            })?;
            let plan = capture_window_filter(&content, &displays, first.id, focus_mode)?;
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
    };

    let cg_image = capture_image(&filter, &config)?;
    let final_image = if annotate.is_empty() {
        cg_image
    } else {
        draw_annotations(&cg_image, display_bounds, annotate).unwrap_or(cg_image)
    };

    let bytes = encode_cg_image(&final_image, format)?;
    Ok(RawScreenshot {
        width: out_w,
        height: out_h,
        display_bounds,
        mime_type: format.mime_type().into(),
        bytes,
    })
}

struct CapturePlan {
    filter: Retained<SCContentFilter>,
    config: Retained<SCStreamConfiguration>,
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
    if focus_mode == FocusMode::Foreground {
        let _ = windows::focus_window(window_id);
    }
    let sc_windows = unsafe { content.windows() };
    let sc_win = sc_windows
        .iter()
        .find(|w| unsafe { w.windowID() } == window_id)
        .ok_or_else(|| {
            RpcError::with_next(
                ErrorCode::NotFound,
                format!("window w{window_id} is not available in ScreenCaptureKit"),
                "The window may be minimized or on another Space. Unminimize it or switch to its Space.",
            )
        })?;
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
        width: w,
        height: h,
        display_bounds: bounds,
    })
}

fn fetch_shareable_content() -> Result<Retained<SCShareableContent>, RpcError> {
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
        Ok(Ok(content)) => Ok(content),
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
                    &ffi::kCFTypeDictionaryKeyCallBacks,
                    &ffi::kCFTypeDictionaryValueCallBacks,
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

/// Draw crisp Set-of-Marks bounding boxes and `@eN` tags directly onto an RGBA bitmap copy.
fn draw_annotations(
    image: &CfOwned,
    display_bounds: Rect,
    boxes: &[AnnotateBox],
) -> Option<CfOwned> {
    if display_bounds.is_empty() {
        return None;
    }
    // SAFETY: Query dimensions of the valid CGImage.
    let img_w = unsafe { ffi::CGImageGetWidth(image.as_mut_ptr()) };
    let img_h = unsafe { ffi::CGImageGetHeight(image.as_mut_ptr()) };
    if img_w == 0 || img_h == 0 {
        return None;
    }
    let cs = unsafe { CfOwned::from_owned(ffi::CGColorSpaceCreateDeviceRGB().cast_const()) }?;
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

    let data = unsafe { ffi::CGBitmapContextGetData(ctx.as_mut_ptr()) };
    let bpr = unsafe { ffi::CGBitmapContextGetBytesPerRow(ctx.as_mut_ptr()) };
    if data.is_null() || bpr < img_w * 4 {
        return None;
    }
    // SAFETY: `data` points to `img_h * bpr` writable RGBA bytes owned by `ctx`.
    let pixels = unsafe { std::slice::from_raw_parts_mut(data, img_h * bpr) };

    let sx = img_w as f64 / display_bounds.width;
    let sy = img_h as f64 / display_bounds.height;

    for b in boxes {
        let x0 = ((b.bounds.x - display_bounds.x) * sx).round() as isize;
        let y0 = ((b.bounds.y - display_bounds.y) * sy).round() as isize;
        let x1 = ((b.bounds.x + b.bounds.width - display_bounds.x) * sx).round() as isize;
        let y1 = ((b.bounds.y + b.bounds.height - display_bounds.y) * sy).round() as isize;
        if x1 <= x0 || y1 <= y0 || x1 < 0 || y1 < 0 || x0 >= img_w as isize || y0 >= img_h as isize
        {
            continue;
        }
        draw_rect_outline(
            pixels,
            img_w,
            img_h,
            bpr,
            x0,
            y0,
            x1,
            y1,
            [255, 68, 58, 255],
        );
        draw_badge(pixels, img_w, img_h, bpr, x0, y0, &b.ref_id);
    }

    // SAFETY: Creates a new CGImage from the annotated bitmap context.
    unsafe { CfOwned::from_owned(ffi::CGBitmapContextCreateImage(ctx.as_mut_ptr()).cast_const()) }
}

#[allow(clippy::too_many_arguments)]
fn set_px(pixels: &mut [u8], w: usize, h: usize, bpr: usize, x: isize, y: isize, rgba: [u8; 4]) {
    if x < 0 || y < 0 || x >= w as isize || y >= h as isize {
        return;
    }
    let off = (y as usize) * bpr + (x as usize) * 4;
    if off + 4 <= pixels.len() {
        pixels[off..off + 4].copy_from_slice(&rgba);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_rect_outline(
    pixels: &mut [u8],
    w: usize,
    h: usize,
    bpr: usize,
    x0: isize,
    y0: isize,
    x1: isize,
    y1: isize,
    rgba: [u8; 4],
) {
    for t in 0..2 {
        for x in x0..=x1 {
            set_px(pixels, w, h, bpr, x, y0 + t, rgba);
            set_px(pixels, w, h, bpr, x, y1 - t, rgba);
        }
        for y in y0..=y1 {
            set_px(pixels, w, h, bpr, x0 + t, y, rgba);
            set_px(pixels, w, h, bpr, x1 - t, y, rgba);
        }
    }
}

fn draw_badge(
    pixels: &mut [u8],
    w: usize,
    h: usize,
    bpr: usize,
    x0: isize,
    y0: isize,
    label: &str,
) {
    let short = label.trim_start_matches('@');
    let bw = (short.len() as isize) * 6 + 6;
    let bh = 11isize;
    let bx = x0.max(0);
    let by = (y0 - bh).max(0);
    for dy in 0..bh {
        for dx in 0..bw {
            set_px(pixels, w, h, bpr, bx + dx, by + dy, [220, 38, 38, 255]);
        }
    }
    for (ci, ch) in short.bytes().enumerate() {
        let glyph = glyph_5x7(ch);
        let gx = bx + 3 + (ci as isize) * 6;
        let gy = by + 2;
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..5 {
                if (bits >> (4 - col)) & 1 != 0 {
                    set_px(
                        pixels,
                        w,
                        h,
                        bpr,
                        gx + col as isize,
                        gy + row as isize,
                        [255, 255, 255, 255],
                    );
                }
            }
        }
    }
}

/// Minimal 5x7 bitmap font for `e` and digits `0`-`9` used in Set-of-Marks badges.
fn glyph_5x7(ch: u8) -> [u8; 7] {
    match ch {
        b'e' | b'E' => [
            0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110, 0b00000,
        ],
        b'0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        b'1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        b'2' => [
            0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111,
        ],
        b'3' => [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        b'4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        b'5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        b'6' => [
            0b01110, 0b10000, 0b11110, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        b'7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        b'8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        b'9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110,
        ],
        _ => [0; 7],
    }
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

        let annotated = draw_annotations(
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
}
