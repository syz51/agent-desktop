use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
    SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType,
};

/// FourCC `'BGRA'` (`kCVPixelFormatType_32BGRA`).
const K_CV_PIXEL_FORMAT_32BGRA: u32 = 0x4247_5241;
/// Maximum age of a warm streamed frame before falling back to one-shot `SCScreenshotManager`.
const MAX_STREAM_FRAME_AGE: Duration = Duration::from_millis(250);
/// Minimum delay after a UI mutation before accepting a queued SCStream frame as post-mutation.
const POST_MUTATION_SETTLE: Duration = Duration::from_millis(40);

/// Latest complete frame pushed by macOS ScreenCaptureKit's persistent low-FPS `SCStream`.
#[derive(Clone)]
pub struct WarmFrame {
    pub width: u32,
    pub height: u32,
    pub display_bounds: Rect,
    /// Tight RGBA8 pixel buffer ready for immediate annotation & ImageIO encoding.
    pub rgba: Vec<u8>,
    pub visual_sig: Option<[u8; 64]>,
    pub arrived_at: Instant,
    pub mutation_gen: u64,
}

struct SharedFrameSlot {
    display_bounds: Rect,
    mutation_gen: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
    latest: Mutex<Option<WarmFrame>>,
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

            // Stamp arrival time and mutation generation BEFORE pixel conversion so an in-flight
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
                let mut rgba = Vec::with_capacity(w * h * 4);
                for y in 0..h {
                    let row = &src[y * bpr..y * bpr + w * 4];
                    for px in row.as_chunks::<4>().0 {
                        rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                    }
                }
                // Drop frame if a mutation or stop occurred while converting pixels.
                if !ivars.slot.stopped.load(Ordering::SeqCst)
                    && ivars.slot.mutation_gen.load(Ordering::SeqCst) == gen_before
                {
                    let frame = WarmFrame {
                        width: w as u32,
                        height: h as u32,
                        display_bounds: ivars.slot.display_bounds,
                        rgba,
                        visual_sig,
                        arrived_at,
                        mutation_gen: gen_before,
                    };
                    if let Ok(mut guard) = ivars.slot.latest.lock() {
                        *guard = Some(frame);
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
    display_id: u32,
    stream: Arc<SendStream>,
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
    active: Arc<Mutex<Option<ActiveStream>>>,
    next_stream_id: AtomicU64,
    mutation_gen: Arc<AtomicU64>,
    last_mutation_at: Mutex<Instant>,
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

    /// Return a fresh complete frame for `display_id` captured safely after the last mutation.
    pub fn latest_frame(&self, display_id: u32) -> Option<WarmFrame> {
        let cur_gen = self.mutation_gen.load(Ordering::SeqCst);
        let last_mut = self
            .last_mutation_at
            .lock()
            .map(|g| *g)
            .unwrap_or_else(|_| Instant::now());
        let guard = self.active.lock().ok()?;
        let active = guard.as_ref()?;
        if active.display_id != display_id || active.slot.stopped.load(Ordering::SeqCst) {
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

    /// Lazily start (or switch display for) the low-FPS warm `SCStream` without blocking capture.
    pub fn ensure_started(&self, disp_info: &DisplayInfo, sc_display: &SCDisplay) {
        let mut guard = match self.active.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(existing) = guard.as_ref()
            && existing.display_id == disp_info.id
            && !existing.slot.stopped.load(Ordering::SeqCst)
        {
            return;
        }
        if let Some(prev) = guard.take() {
            prev.slot.stopped.store(true, Ordering::SeqCst);
            stop_stream_async(&prev.stream.0);
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
        // SAFETY: Allocates a default `SCStreamConfiguration` instance.
        let cfg = unsafe { SCStreamConfiguration::new() };
        // SAFETY: Configures standard width/height/cursor/pixel-format/frame-interval properties.
        unsafe {
            cfg.setWidth(w as usize);
            cfg.setHeight(h as usize);
            cfg.setShowsCursor(true);
            cfg.setPixelFormat(K_CV_PIXEL_FORMAT_32BGRA);
            cfg.setQueueDepth(2);
            cfg.setMinimumFrameInterval(CMTime::new(1, 10));
        }

        let stream_id = self.next_stream_id.fetch_add(1, Ordering::Relaxed);
        let stopped = Arc::new(AtomicBool::new(false));
        let slot = Arc::new(SharedFrameSlot {
            display_bounds: disp_info.bounds,
            mutation_gen: Arc::clone(&self.mutation_gen),
            stopped: Arc::clone(&stopped),
            latest: Mutex::new(None),
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

        tracing::debug!(
            display_id = disp_info.id,
            w,
            h,
            "started on-demand warm SCStream (10 fps)"
        );
        *guard = Some(ActiveStream {
            stream_id,
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
            prev.slot.stopped.store(true, Ordering::SeqCst);
            stop_stream_async(&prev.stream.0);
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
