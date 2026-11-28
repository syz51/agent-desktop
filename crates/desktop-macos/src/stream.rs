use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use desktop_core::visual_signature_from_pixels;
use desktop_proto::{DisplayInfo, Rect, fit_screenshot};
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CFDictionary;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSError, NSNumber};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType, SCWindow,
};

/// FourCC `'BGRA'` (`kCVPixelFormatType_32BGRA`).
const K_CV_PIXEL_FORMAT_32BGRA: u32 = 0x4247_5241;
/// Maximum age of a warm streamed frame before falling back to one-shot `SCScreenshotManager`.
const MAX_STREAM_FRAME_AGE: Duration = Duration::from_millis(250);
/// Minimum delay after a UI mutation before accepting a queued SCStream frame as post-mutation.
const POST_MUTATION_SETTLE: Duration = Duration::from_millis(40);

/// Identifies which macOS display or background window a warm `SCStream` is tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKey {
    Display(u32),
    Window(u32),
}

/// Latest complete frame pushed by macOS ScreenCaptureKit's persistent low-FPS `SCStream`.
///
/// Keeps pixels in ScreenCaptureKit's native `32BGRA` format so arrival callbacks perform zero
/// per-pixel RGBA swizzling when frames are unused or encoded directly via CoreGraphics BGRA.
#[derive(Clone)]
pub struct WarmFrame {
    pub width: u32,
    pub height: u32,
    pub display_bounds: Rect,
    /// Tight BGRA8 pixel buffer (`width * height * 4` bytes).
    pub bgra: Vec<u8>,
    pub visual_sig: Option<[u8; 64]>,
    pub arrived_at: Instant,
    pub mutation_gen: u64,
}

struct SharedFrameSlot {
    display_bounds: Rect,
    mutation_gen: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
    frame_seq: AtomicU64,
    latest: Mutex<Option<WarmFrame>>,
    frame_cv: Condvar,
}

struct OutputIvars {
    slot: Arc<SharedFrameSlot>,
    active_ref: Arc<Mutex<Option<ActiveStream>>>,
    stream_id: u64,
}

/// Verify that ScreenCaptureKit marked this sample buffer `SCFrameStatus::Complete` (rejecting
/// `Blank`, `Idle`, `Suspended`, `Started`, or `Stopped` buffers).
fn is_complete_frame(sample_buffer: &CMSampleBuffer) -> bool {
    // SAFETY: `sample_buffer` is a valid CMSampleBuffer passed by ScreenCaptureKit; `false` does
    // not mutate the sample buffer.
    let Some(attachments) = (unsafe { sample_buffer.sample_attachments_array(false) }) else {
        return false;
    };
    if attachments.is_empty() {
        return false;
    }
    // SAFETY: Index 0 is in bounds because `attachments` is non-empty, and ScreenCaptureKit
    // attaches a CFDictionary<SCStreamFrameInfo, id> as element 0.
    let dict_ptr = unsafe { attachments.value_at_index(0) }.cast::<CFDictionary>();
    if dict_ptr.is_null() {
        return false;
    }
    // SAFETY: `SCStreamFrameInfoStatus` is a static Objective-C NSString/CFString key.
    let status_key = unsafe { SCStreamFrameInfoStatus };
    let key_ptr = (status_key as *const objc2_foundation::NSString).cast::<std::ffi::c_void>();
    // SAFETY: `dict_ptr` is a valid CFDictionary borrowed from `attachments` for this call.
    let val_ptr = unsafe { (*dict_ptr).value(key_ptr) };
    if val_ptr.is_null() {
        return false;
    }
    // SAFETY: `SCStreamFrameInfoStatus` value is an `NSNumber` wrapping `SCFrameStatus` (`NSInteger`).
    let status_num = unsafe { &*(val_ptr.cast::<NSNumber>()) };
    SCFrameStatus(status_num.integerValue()) == SCFrameStatus::Complete
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "AgentDesktopStreamOutput"]
    #[ivars = OutputIvars]
    struct StreamOutputHandler;

    unsafe impl NSObjectProtocol for StreamOutputHandler {}

    unsafe impl SCStreamDelegate for StreamOutputHandler {
        #[allow(non_snake_case)]
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stream_didStopWithError(&self, _stream: &SCStream, error: &NSError) {
            let ivars = self.ivars();
            ivars.slot.stopped.store(true, Ordering::SeqCst);
            ivars.slot.frame_cv.notify_all();
            if let Ok(mut guard) = ivars.active_ref.lock()
                && guard
                    .as_ref()
                    .is_some_and(|a| a.stream_id == ivars.stream_id)
            {
                *guard = None;
            }
            let msg = error.localizedDescription().to_string();
            tracing::debug!(%msg, "warm SCStream stopped externally; cleared active handle");
        }
    }

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
            let ivars = self.ivars();
            if ivars.slot.stopped.load(Ordering::SeqCst) {
                return;
            }
            if !is_complete_frame(sample_buffer) {
                return;
            }

            // Stamp arrival time and mutation generation BEFORE pixel copy so an in-flight
            // pre-mutation callback can never slip past a concurrent `mark_mutation()`.
            let arrived_at = Instant::now();
            let gen_before = ivars.slot.mutation_gen.load(Ordering::SeqCst);

            // SAFETY: `sample_buffer` is a valid CMSampleBuffer delivered by SCStream.
            let Some(image_buf) = (unsafe { sample_buffer.image_buffer() }) else {
                return;
            };
            // CVImageBuffer is toll-free equivalent to CVPixelBuffer for screen capture outputs.
            let pixel_buf = &*image_buf;
            // SAFETY: `pixel_buf` is a retained valid CVPixelBuffer for the duration of this scope.
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
                let visual_sig = visual_signature_from_pixels(src, w, h, bpr, true);
                let row_bytes = w * 4;
                // Copy native BGRA rows directly via memcpy (no CPU BGRA->RGBA swizzle).
                let bgra = if bpr == row_bytes {
                    src[..h * row_bytes].to_vec()
                } else {
                    let mut buf = Vec::with_capacity(h * row_bytes);
                    for y in 0..h {
                        buf.extend_from_slice(&src[y * bpr..y * bpr + row_bytes]);
                    }
                    buf
                };
                // Drop frame if a mutation or stop occurred while copying pixels.
                if !ivars.slot.stopped.load(Ordering::SeqCst)
                    && ivars.slot.mutation_gen.load(Ordering::SeqCst) == gen_before
                {
                    let frame = WarmFrame {
                        width: w as u32,
                        height: h as u32,
                        display_bounds: ivars.slot.display_bounds,
                        bgra,
                        visual_sig,
                        arrived_at,
                        mutation_gen: gen_before,
                    };
                    if let Ok(mut guard) = ivars.slot.latest.lock() {
                        *guard = Some(frame);
                        ivars.slot.frame_seq.fetch_add(1, Ordering::AcqRel);
                        ivars.slot.frame_cv.notify_all();
                    }
                }
            }

            // SAFETY: Matches the successful `CVPixelBufferLockBaseAddress` above.
            unsafe {
                CVPixelBufferUnlockBaseAddress(pixel_buf, CVPixelBufferLockFlags::ReadOnly);
            }
        }
    }
);

impl StreamOutputHandler {
    fn new(
        slot: Arc<SharedFrameSlot>,
        active_ref: Arc<Mutex<Option<ActiveStream>>>,
        stream_id: u64,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars {
            slot,
            active_ref,
            stream_id,
        });
        // SAFETY: Standard NSObject `init` on freshly allocated class.
        unsafe { msg_send![super(this), init] }
    }
}

struct SendStream(Retained<SCStream>);

// SAFETY: `SCStream` start/stop calls are thread-safe Objective-C methods.
unsafe impl Send for SendStream {}
// SAFETY: `SendStream` is only used to dispatch thread-safe `stopCaptureWithCompletionHandler`.
unsafe impl Sync for SendStream {}

struct ActiveStream {
    stream_id: u64,
    key: StreamKey,
    width: u32,
    height: u32,
    stream: Arc<SendStream>,
    _output: Retained<StreamOutputHandler>,
    _queue: DispatchRetained<DispatchQueue>,
    slot: Arc<SharedFrameSlot>,
}

// SAFETY: `ActiveStream` is strictly accessed under `WarmCaptureStream`'s `Mutex`, and all
// ScreenCaptureKit start/stop methods are thread-safe ObjC calls.
unsafe impl Send for ActiveStream {}

/// On-demand persistent 10 FPS `SCStream` capture buffer supporting both full displays and
/// background `desktopIndependentWindow` targets.
///
/// Lazily starts when captures run during an active session and automatically stops when the
/// session/lease ends so macOS shows no idle recording indicator.
pub struct WarmCaptureStream {
    active: Arc<Mutex<Option<ActiveStream>>>,
    next_stream_id: AtomicU64,
    mutation_gen: Arc<AtomicU64>,
    last_mutation_at: Mutex<Instant>,
}

impl Default for WarmCaptureStream {
    fn default() -> Self {
        Self::new()
    }
}

impl WarmCaptureStream {
    #[must_use]
    pub fn new() -> Self {
        Self {
            active: Arc::new(Mutex::new(None)),
            next_stream_id: AtomicU64::new(1),
            mutation_gen: Arc::new(AtomicU64::new(1)),
            last_mutation_at: Mutex::new(Instant::now()),
        }
    }

    /// Record a UI mutation so pre-mutation buffered frames are not served for post-action grabs.
    pub fn mark_mutation(&self) {
        self.mutation_gen.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut guard) = self.last_mutation_at.lock() {
            *guard = Instant::now();
        }
        if let Ok(guard) = self.active.lock()
            && let Some(active) = guard.as_ref()
            && let Ok(mut latest) = active.slot.latest.lock()
        {
            *latest = None;
        }
    }

    /// Return a fresh complete frame for `key` captured safely after the last mutation.
    pub fn latest_frame(&self, key: StreamKey) -> Option<WarmFrame> {
        let cur_gen = self.mutation_gen.load(Ordering::SeqCst);
        let last_mut = self
            .last_mutation_at
            .lock()
            .map(|g| *g)
            .unwrap_or_else(|_| Instant::now());
        let guard = self.active.lock().ok()?;
        let active = guard.as_ref()?;
        if active.key != key || active.slot.stopped.load(Ordering::SeqCst) {
            return None;
        }
        let frame = active.slot.latest.lock().ok()?.clone()?;
        let post_settle = frame.arrived_at >= last_mut + POST_MUTATION_SETTLE;
        if frame.mutation_gen == cur_gen
            && post_settle
            && frame.arrived_at.elapsed() <= MAX_STREAM_FRAME_AGE
        {
            Some(frame)
        } else {
            None
        }
    }

    /// Return only the 8x8 visual signature from the latest fresh frame without cloning pixels.
    pub fn latest_visual_sig(&self, key: StreamKey) -> Option<[u8; 64]> {
        let cur_gen = self.mutation_gen.load(Ordering::SeqCst);
        let last_mut = self
            .last_mutation_at
            .lock()
            .map(|g| *g)
            .unwrap_or_else(|_| Instant::now());
        let guard = self.active.lock().ok()?;
        let active = guard.as_ref()?;
        if active.key != key || active.slot.stopped.load(Ordering::SeqCst) {
            return None;
        }
        let latest = active.slot.latest.lock().ok()?;
        let frame = latest.as_ref()?;
        let post_settle = frame.arrived_at >= last_mut + POST_MUTATION_SETTLE;
        if frame.mutation_gen == cur_gen
            && post_settle
            && frame.arrived_at.elapsed() <= MAX_STREAM_FRAME_AGE
        {
            frame.visual_sig
        } else {
            None
        }
    }

    /// Block up to `timeout` waiting for the next complete frame on `key` (or fall back to a
    /// brief sleep if no matching stream is running). Returns `true` if a new frame arrived.
    pub fn wait_for_frame(&self, key: StreamKey, timeout: Duration, stop: &AtomicBool) -> bool {
        let slot = {
            let Ok(guard) = self.active.lock() else {
                std::thread::sleep(timeout.min(Duration::from_millis(40)));
                return false;
            };
            guard
                .as_ref()
                .filter(|a| a.key == key && !a.slot.stopped.load(Ordering::SeqCst))
                .map(|a| Arc::clone(&a.slot))
        };
        let Some(slot) = slot else {
            std::thread::sleep(timeout.min(Duration::from_millis(40)));
            return false;
        };

        let start_seq = slot.frame_seq.load(Ordering::Acquire);
        let deadline = Instant::now() + timeout;
        let Ok(mut guard) = slot.latest.lock() else {
            return false;
        };
        while !stop.load(Ordering::Acquire) && !slot.stopped.load(Ordering::SeqCst) {
            if slot.frame_seq.load(Ordering::Acquire) != start_seq {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let slice = deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(40));
            match slot.frame_cv.wait_timeout(guard, slice) {
                Ok((g, _)) => guard = g,
                Err(_) => return false,
            }
        }
        false
    }

    /// Lazily start (or switch target for) the low-FPS warm `SCStream` on a display.
    pub fn ensure_started_display(&self, disp_info: &DisplayInfo, sc_display: &SCDisplay) {
        let (w, h) = fit_screenshot(disp_info.pixel_width, disp_info.pixel_height);
        let key = StreamKey::Display(disp_info.id);
        let empty_windows = NSArray::new();
        // SAFETY: `sc_display` and `empty_windows` are valid ScreenCaptureKit/Foundation objects.
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                sc_display,
                &empty_windows,
            )
        };
        self.ensure_started_with_filter(key, disp_info.bounds, w, h, true, false, filter);
    }

    /// Lazily start (or switch target for) a background `desktopIndependentWindow` warm `SCStream`.
    pub fn ensure_started_window(
        &self,
        window_id: u32,
        bounds: Rect,
        w: u32,
        h: u32,
        sc_win: &SCWindow,
    ) {
        let key = StreamKey::Window(window_id);
        // SAFETY: `sc_win` is a valid retained SCWindow from SCShareableContent.
        let filter = unsafe {
            SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), sc_win)
        };
        self.ensure_started_with_filter(key, bounds, w, h, false, true, filter);
    }

    #[allow(clippy::too_many_arguments)]
    fn ensure_started_with_filter(
        &self,
        key: StreamKey,
        display_bounds: Rect,
        w: u32,
        h: u32,
        shows_cursor: bool,
        scales_to_fit: bool,
        filter: Retained<SCContentFilter>,
    ) {
        let mut guard = match self.active.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(existing) = guard.as_ref()
            && existing.key == key
            && existing.width == w
            && existing.height == h
            && !existing.slot.stopped.load(Ordering::SeqCst)
        {
            return;
        }
        if let Some(prev) = guard.take() {
            prev.slot.stopped.store(true, Ordering::SeqCst);
            prev.slot.frame_cv.notify_all();
            stop_stream_async(&prev.stream.0);
        }

        // SAFETY: Allocates a default `SCStreamConfiguration` instance.
        let cfg = unsafe { SCStreamConfiguration::new() };
        // SAFETY: Configures standard width/height/cursor/pixel-format/frame-interval properties.
        unsafe {
            cfg.setWidth(w as usize);
            cfg.setHeight(h as usize);
            cfg.setShowsCursor(shows_cursor);
            if scales_to_fit {
                cfg.setScalesToFit(true);
            }
            cfg.setPixelFormat(K_CV_PIXEL_FORMAT_32BGRA);
            cfg.setQueueDepth(2);
            cfg.setMinimumFrameInterval(CMTime::new(1, 10));
        }

        let stream_id = self.next_stream_id.fetch_add(1, Ordering::Relaxed);
        let stopped = Arc::new(AtomicBool::new(false));
        let slot = Arc::new(SharedFrameSlot {
            display_bounds,
            mutation_gen: Arc::clone(&self.mutation_gen),
            stopped: Arc::clone(&stopped),
            frame_seq: AtomicU64::new(0),
            latest: Mutex::new(None),
            frame_cv: Condvar::new(),
        });
        let output =
            StreamOutputHandler::new(Arc::clone(&slot), Arc::clone(&self.active), stream_id);
        let delegate_proto: &ProtocolObject<dyn SCStreamDelegate> =
            ProtocolObject::from_ref(&*output);
        // SAFETY: `filter`, `cfg`, and `delegate_proto` are valid retained Objective-C objects.
        let stream = Arc::new(SendStream(unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &cfg,
                Some(delegate_proto),
            )
        }));
        let queue = DispatchQueue::new("com.syz.agent-desktop.stream", DispatchQueueAttr::SERIAL);
        let output_proto: &ProtocolObject<dyn SCStreamOutput> = ProtocolObject::from_ref(&*output);

        // SAFETY: `output_proto` implements `SCStreamOutput` and `queue` is a valid serial GCD queue.
        let add_res = unsafe {
            stream.0.addStreamOutput_type_sampleHandlerQueue_error(
                output_proto,
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

        let active_for_cb = Arc::clone(&self.active);
        let stopped_for_cb = Arc::clone(&stopped);
        let stream_for_cb = Arc::clone(&stream);
        let start_block = RcBlock::new(move |err: *mut NSError| {
            if !err.is_null() {
                // SAFETY: `err` is a valid NSError passed by ScreenCaptureKit.
                let msg = unsafe { (&*err).localizedDescription().to_string() };
                tracing::debug!(%msg, "warm SCStream start returned error; clearing active handle");
                stopped_for_cb.store(true, Ordering::SeqCst);
                if let Ok(mut g) = active_for_cb.lock()
                    && g.as_ref().is_some_and(|a| a.stream_id == stream_id)
                {
                    *g = None;
                }
                return;
            }
            // If `stop()` ran while `startCapture` was still in flight, stop immediately now so
            // no orphaned SCStream or macOS recording indicator is ever left running.
            if stopped_for_cb.load(Ordering::SeqCst) {
                stop_stream_async(&stream_for_cb.0);
            }
        });
        // SAFETY: `stream` and `start_block` are valid retained objects.
        unsafe {
            stream
                .0
                .startCaptureWithCompletionHandler(Some(&start_block));
        }

        tracing::debug!(?key, w, h, "started on-demand warm SCStream (10 fps)");
        *guard = Some(ActiveStream {
            stream_id,
            key,
            width: w,
            height: h,
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
            prev.slot.stopped.store(true, Ordering::SeqCst);
            prev.slot.frame_cv.notify_all();
            stop_stream_async(&prev.stream.0);
            tracing::debug!(key = ?prev.key, "stopped on-demand warm SCStream");
        }
    }
}

impl Drop for WarmCaptureStream {
    fn drop(&mut self) {
        self.stop();
    }
}

fn stop_stream_async(stream: &SCStream) {
    let block = RcBlock::new(|err: *mut NSError| {
        if !err.is_null() {
            // SAFETY: `err` is a valid NSError passed by ScreenCaptureKit.
            let msg = unsafe { (&*err).localizedDescription().to_string() };
            tracing::debug!(%msg, "SCStream stopCaptureWithCompletionHandler returned error");
        }
    });
    // SAFETY: `stream` and `block` are valid retained objects.
    unsafe {
        stream.stopCaptureWithCompletionHandler(Some(&block));
    }
}
