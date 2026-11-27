use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use desktop_core::dhash_from_pixels;
use desktop_proto::{DisplayInfo, Rect, fit_screenshot};
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType,
};

/// FourCC `'BGRA'` (`kCVPixelFormatType_32BGRA`).
const K_CV_PIXEL_FORMAT_32BGRA: u32 = 0x4247_5241;
/// Maximum age of a warm streamed frame before falling back to one-shot `SCScreenshotManager`.
const MAX_STREAM_FRAME_AGE: Duration = Duration::from_millis(250);

/// Latest frame pushed by macOS ScreenCaptureKit's persistent low-FPS `SCStream`.
#[derive(Clone)]
pub struct WarmFrame {
    pub width: u32,
    pub height: u32,
    pub display_bounds: Rect,
    /// Tight RGBA8 pixel buffer ready for immediate annotation & ImageIO encoding.
    pub rgba: Vec<u8>,
    pub dhash: Option<u64>,
    pub captured_at: Instant,
}

struct SharedFrameSlot {
    display_bounds: Rect,
    latest: Mutex<Option<WarmFrame>>,
}

struct OutputIvars {
    slot: Arc<SharedFrameSlot>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "AgentDesktopStreamOutput"]
    #[ivars = OutputIvars]
    struct StreamOutputHandler;

    unsafe impl NSObjectProtocol for StreamOutputHandler {}

    unsafe impl SCStreamOutput for StreamOutputHandler {
        #[allow(non_snake_case)]
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            output_type: SCStreamOutputType,
        ) {
            if output_type != SCStreamOutputType::Screen {
                return;
            }
            // SAFETY: `sample_buffer` is a valid CMSampleBuffer delivered by SCStream.
            let Some(image_buf) = (unsafe { sample_buffer.image_buffer() }) else {
                return;
            };
            // CVImageBuffer is toll-free equivalent to CVPixelBuffer for screen capture outputs.
            let pixel_buf = &*image_buf;
            let lock_status = unsafe {
                CVPixelBufferLockBaseAddress(pixel_buf, CVPixelBufferLockFlags::ReadOnly)
            };
            if lock_status != 0 {
                return;
            }

            let w = CVPixelBufferGetWidth(pixel_buf);
            let h = CVPixelBufferGetHeight(pixel_buf);
            let bpr = CVPixelBufferGetBytesPerRow(pixel_buf);
            let base = CVPixelBufferGetBaseAddress(pixel_buf);

            if !base.is_null() && w > 0 && h > 0 && bpr >= w * 4 {
                // SAFETY: Base address is locked read-only and holds `h * bpr` BGRA8 bytes.
                let src = unsafe { std::slice::from_raw_parts(base.cast::<u8>(), h * bpr) };
                let dhash = dhash_from_pixels(src, w, h, bpr, true);
                let mut rgba = Vec::with_capacity(w * h * 4);
                for y in 0..h {
                    let row = &src[y * bpr..y * bpr + w * 4];
                    for px in row.as_chunks::<4>().0 {
                        rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                    }
                }
                let ivars = self.ivars();
                let frame = WarmFrame {
                    width: w as u32,
                    height: h as u32,
                    display_bounds: ivars.slot.display_bounds,
                    rgba,
                    dhash,
                    captured_at: Instant::now(),
                };
                if let Ok(mut guard) = ivars.slot.latest.lock() {
                    *guard = Some(frame);
                }
            }

            unsafe {
                CVPixelBufferUnlockBaseAddress(pixel_buf, CVPixelBufferLockFlags::ReadOnly);
            }
        }
    }
);

impl StreamOutputHandler {
    fn new(slot: Arc<SharedFrameSlot>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { slot });
        // SAFETY: Standard NSObject `init` on freshly allocated class.
        unsafe { msg_send![super(this), init] }
    }
}

struct ActiveStream {
    display_id: u32,
    stream: Retained<SCStream>,
    _output: Retained<StreamOutputHandler>,
    _queue: DispatchRetained<DispatchQueue>,
    slot: Arc<SharedFrameSlot>,
}

// SAFETY: `ActiveStream` is strictly accessed under `WarmDisplayStream`'s `Mutex`, and all
// ScreenCaptureKit start/stop methods are thread-safe ObjC calls.
unsafe impl Send for ActiveStream {}

/// On-demand persistent 10 FPS `SCStream` display ring buffer.
///
/// Lazily starts when display screenshots run during an active session and automatically stops
/// when the session/lease ends so macOS shows no idle recording indicator.
pub struct WarmDisplayStream {
    active: Mutex<Option<ActiveStream>>,
    last_mutation: Mutex<Instant>,
}

impl Default for WarmDisplayStream {
    fn default() -> Self {
        Self::new()
    }
}

impl WarmDisplayStream {
    #[must_use]
    pub fn new() -> Self {
        Self {
            active: Mutex::new(None),
            last_mutation: Mutex::new(Instant::now()),
        }
    }

    /// Record a UI mutation so pre-mutation buffered frames are not served for post-action grabs.
    pub fn mark_mutation(&self) {
        if let Ok(mut guard) = self.last_mutation.lock() {
            *guard = Instant::now();
        }
    }

    /// Return a fresh warm frame for `display_id` if available and captured after `last_mutation`.
    pub fn latest_frame(&self, display_id: u32) -> Option<WarmFrame> {
        let last_mut = self
            .last_mutation
            .lock()
            .map(|g| *g)
            .unwrap_or_else(|_| Instant::now());
        let guard = self.active.lock().ok()?;
        let active = guard.as_ref()?;
        if active.display_id != display_id {
            return None;
        }
        let frame = active.slot.latest.lock().ok()?.clone()?;
        if frame.captured_at >= last_mut && frame.captured_at.elapsed() <= MAX_STREAM_FRAME_AGE {
            Some(frame)
        } else {
            None
        }
    }

    /// Lazily start (or switch display for) the low-FPS warm `SCStream` without blocking capture.
    pub fn ensure_started(&self, disp_info: &DisplayInfo, sc_display: &SCDisplay) {
        let mut guard = match self.active.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(existing) = guard.as_ref()
            && existing.display_id == disp_info.id
        {
            return;
        }
        if let Some(prev) = guard.take() {
            stop_stream_async(&prev.stream);
        }

        let (w, h) = fit_screenshot(disp_info.pixel_width, disp_info.pixel_height);
        let empty_windows = NSArray::new();
        // SAFETY: `sc_display` and `empty_windows` are valid ScreenCaptureKit/Foundation objects.
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                sc_display,
                &empty_windows,
            )
        };
        let cfg = unsafe { SCStreamConfiguration::new() };
        unsafe {
            cfg.setWidth(w as usize);
            cfg.setHeight(h as usize);
            cfg.setShowsCursor(true);
            cfg.setPixelFormat(K_CV_PIXEL_FORMAT_32BGRA);
            cfg.setQueueDepth(2);
            cfg.setMinimumFrameInterval(CMTime::new(1, 10));
        }

        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &cfg, None)
        };
        let slot = Arc::new(SharedFrameSlot {
            display_bounds: disp_info.bounds,
            latest: Mutex::new(None),
        });
        let output = StreamOutputHandler::new(Arc::clone(&slot));
        let queue = DispatchQueue::new("com.syz.agent-desktop.stream", DispatchQueueAttr::SERIAL);
        let proto: &ProtocolObject<dyn SCStreamOutput> = ProtocolObject::from_ref(&*output);

        let add_res = unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                proto,
                SCStreamOutputType::Screen,
                Some(&queue),
            )
        };
        if let Err(err) = add_res {
            tracing::debug!(
                ?err,
                "could not attach SCStreamOutput; staying on one-shot capture"
            );
            return;
        }

        let start_block = RcBlock::new(|err: *mut NSError| {
            if !err.is_null() {
                // SAFETY: `err` is a valid NSError passed by ScreenCaptureKit.
                let msg = unsafe { (&*err).localizedDescription().to_string() };
                tracing::debug!(%msg, "warm SCStream start returned error");
            }
        });
        // SAFETY: `stream` and `start_block` are valid retained objects.
        unsafe {
            stream.startCaptureWithCompletionHandler(Some(&start_block));
        }

        tracing::debug!(
            display_id = disp_info.id,
            w,
            h,
            "started on-demand warm SCStream (10 fps)"
        );
        *guard = Some(ActiveStream {
            display_id: disp_info.id,
            stream,
            _output: output,
            _queue: queue,
            slot,
        });
    }

    /// Stop any active warm `SCStream` immediately (called when lease ends / daemon idles).
    pub fn stop(&self) {
        if let Ok(mut guard) = self.active.lock()
            && let Some(prev) = guard.take()
        {
            stop_stream_async(&prev.stream);
            tracing::debug!(
                display_id = prev.display_id,
                "stopped on-demand warm SCStream"
            );
        }
    }
}

impl Drop for WarmDisplayStream {
    fn drop(&mut self) {
        self.stop();
    }
}

fn stop_stream_async(stream: &SCStream) {
    let block = RcBlock::new(|_err: *mut NSError| {});
    // SAFETY: `stream` and `block` are valid retained objects.
    unsafe {
        stream.stopCaptureWithCompletionHandler(Some(&block));
    }
}
