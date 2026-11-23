//! Per-window capture via `SCStream` (ScreenCaptureKit).
//!
//! One `SCStream` per window, built on a display filter that
//! includes exactly that window
//! (`SCContentFilter::initWithDisplay:includingWindows:` -- the
//! documented path for ordinary app windows; the earlier
//! `initWithDesktopIndependentWindow` filter targets windows that
//! are not attached to a desktop/space, which a normal app window
//! is, so it was replaced -- see `docs/MACOS_VERIFICATION.md`).
//! The display is the one containing most of the window, and the
//! default output size is the window's point size multiplied by
//! that content's backing scale factor (`SCShareableContentInfo
//! .pointPixelScale`), so Retina windows capture at full pixel
//! resolution instead of half.
//!
//! Frames arrive as `CMSampleBuffer`s wrapping a `CVPixelBuffer` on
//! a dedicated serial dispatch queue. The output object copies the
//! pixels out on that queue (the OS recycles its buffers) and hands
//! the owned frame to a worker thread; any BGRA -> I420 conversion
//! and the caller callback run on the worker, never on the SCK
//! queue, so a slow encoder-side conversion cannot stall capture.
//! A panic in the caller callback is caught (`catch_unwind`) and
//! logged instead of crossing the FFI boundary and aborting the
//! process.
//!
//! Stream death is observed through an `SCStreamDelegate`: its
//! `stream:didStopWithError:` is recorded and exposed via
//! [`CaptureSession::stream_error`], so the host can tell a dead
//! stream apart from mere frame silence.
//!
//! UNVERIFIED ON HARDWARE -- see `mod.rs` and
//! `docs/MACOS_VERIFICATION.md`. In particular the `define_class!`
//! output/delegate wiring and the `CVPixelBuffer` copy path have
//! not run on a Mac yet.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamOutput, SCStreamOutputType,
};

use super::enumerate::{fetch_window_with_display, QUERY_TIMEOUT};
use super::error::{classify_block_error, classify_ns_error};
use crate::convert::frame_to_i420;
use crate::traits::{CaptureSession, FrameCallback};
use crate::types::{CaptureConfig, CapturedFrame, PixelFormat, PlatformError, Result, WindowId};

// ---------------------------------------------------------------------------
// Minimal CoreVideo / CoreMedia FFI for the pixel-buffer copy path.
//
// The generated `objc2-core-video` / `objc2-core-media` bindings model
// these objects as CoreFoundation types whose retained-pointer
// plumbing differs between crate versions; declaring the five C
// entry points we need (all documented, stable since macOS 10.x)
// keeps this copy path small and auditable. The objects themselves
// still come from the generated ScreenCaptureKit bindings above.
#[repr(C)]
#[derive(Clone, Copy)]
struct CMTimeRaw {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

/// `kCMTimeFlags_Valid` (bit 0): a presentation timestamp is only
/// meaningful when this flag is set; an invalid/zero timestamp must
/// not be reported as a real frame time.
const CMTIME_VALID: u32 = 1;

extern "C" {
    fn CMSampleBufferGetImageBuffer(sample: *const c_void) -> *mut c_void;
    fn CMSampleBufferGetPresentationTimeStamp(sample: *const c_void) -> CMTimeRaw;
    fn CVPixelBufferLockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(buffer: *mut c_void) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRow(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetWidth(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetHeight(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetPixelFormatType(buffer: *mut c_void) -> u32;
    fn CVPixelBufferGetBaseAddressOfPlane(buffer: *mut c_void, plane: usize) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRowOfPlane(buffer: *mut c_void, plane: usize) -> usize;
    fn CVPixelBufferGetPlaneCount(buffer: *mut c_void) -> usize;
}

const CV_LOCK_READ_ONLY: u64 = 0x0001;

/// Copy one sample buffer into an owned [`CapturedFrame`].
///
/// The frame is returned in the pixel format the OS delivered
/// (BGRA, or I420 when the configuration asked for it); BGRA -> I420
/// conversion, if requested, happens later on the worker thread.
///
/// Returns `None` for samples without an image buffer (e.g. status /
/// audio samples, which this crate never subscribes to).
///
/// Safety: `sample` must be a live `CMSampleBuffer` for the duration
/// of the call (guaranteed inside the SCStreamOutput callback).
unsafe fn copy_frame(
    sample: &CMSampleBuffer,
    window_id: WindowId,
    sequence: u64,
) -> Option<CapturedFrame> {
    let sample_ptr = sample as *const CMSampleBuffer as *const c_void;
    let buffer = CMSampleBufferGetImageBuffer(sample_ptr);
    if buffer.is_null() {
        return None;
    }
    if CVPixelBufferLockBaseAddress(buffer, CV_LOCK_READ_ONLY) != 0 {
        return None;
    }
    let width = CVPixelBufferGetWidth(buffer) as u32;
    let height = CVPixelBufferGetHeight(buffer) as u32;
    let format_type = CVPixelBufferGetPixelFormatType(buffer);
    let pts = CMSampleBufferGetPresentationTimeStamp(sample_ptr);
    let timestamp = if pts.timescale > 0 && pts.value >= 0 && (pts.flags & CMTIME_VALID) != 0 {
        Some(std::time::Duration::from_secs_f64(
            pts.value as f64 / pts.timescale as f64,
        ))
    } else {
        None
    };

    let frame = if format_type == PixelFormat::macos_bgra_ostype() {
        let stride = CVPixelBufferGetBytesPerRow(buffer);
        let base = CVPixelBufferGetBaseAddress(buffer);
        if base.is_null() {
            CVPixelBufferUnlockBaseAddress(buffer, CV_LOCK_READ_ONLY);
            return None;
        }
        let bytes = std::slice::from_raw_parts(base as *const u8, stride * height as usize);
        CapturedFrame {
            window_id,
            width,
            height,
            format: PixelFormat::Bgra8888,
            strides: vec![stride],
            data: bytes.to_vec(),
            timestamp,
            sequence,
        }
    } else if format_type == PixelFormat::macos_i420_ostype() {
        // Planar path: concatenate Y, U, V planes.
        let planes = CVPixelBufferGetPlaneCount(buffer);
        let mut data = Vec::new();
        let mut strides = Vec::new();
        for plane in 0..planes.min(3) {
            let base = CVPixelBufferGetBaseAddressOfPlane(buffer, plane);
            let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, plane);
            if base.is_null() {
                CVPixelBufferUnlockBaseAddress(buffer, CV_LOCK_READ_ONLY);
                return None;
            }
            let plane_height = if plane == 0 {
                height as usize
            } else {
                (height as usize).div_ceil(2)
            };
            let slice = std::slice::from_raw_parts(base as *const u8, stride * plane_height);
            data.extend_from_slice(slice);
            strides.push(stride);
        }
        CapturedFrame {
            window_id,
            width,
            height,
            format: PixelFormat::I420,
            strides,
            data,
            timestamp,
            sequence,
        }
    } else {
        CVPixelBufferUnlockBaseAddress(buffer, CV_LOCK_READ_ONLY);
        tracing::warn!(%window_id, format_type, "unexpected pixel format from ScreenCaptureKit; frame dropped");
        return None;
    };
    CVPixelBufferUnlockBaseAddress(buffer, CV_LOCK_READ_ONLY);
    Some(frame)
}

// ---------------------------------------------------------------------------
// SCStreamOutput implementation object.
//
// The output object only copies pixels and forwards the owned frame
// to the worker thread. Conversion and the caller callback run there.

pub struct OutputIvars {
    sender: std::sync::mpsc::Sender<CapturedFrame>,
    window_id: WindowId,
    sequence: AtomicU64,
    /// Logged once if the worker thread has gone away.
    worker_gone_logged: AtomicBool,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and
    // StreamOutput does not implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "WindowStreamStreamOutput"]
    #[ivars = OutputIvars]
    pub struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        #[allow(non_snake_case)]
        fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            let ivars = self.ivars();
            let sequence = ivars.sequence.fetch_add(1, Ordering::Relaxed);
            // SAFETY: sample_buffer is live for this callback.
            let frame = unsafe { copy_frame(sample_buffer, ivars.window_id, sequence) };
            if let Some(frame) = frame {
                if ivars.sender.send(frame).is_err()
                    && !ivars.worker_gone_logged.swap(true, Ordering::Relaxed)
                {
                    tracing::error!(
                        window_id = %ivars.window_id,
                        "capture worker thread is gone; frames are being dropped"
                    );
                }
            }
        }
    }
);

impl StreamOutput {
    /// Create the output object and its worker thread. The worker
    /// converts (BGRA -> I420, when requested) and invokes the
    /// caller callback; it exits when the output object is dropped
    /// (the channel sender goes away with it).
    fn new(
        window_id: WindowId,
        want_i420: bool,
        callback: FrameCallback,
    ) -> Result<Retained<Self>> {
        let (tx, rx) = std::sync::mpsc::channel::<CapturedFrame>();
        let mut callback = callback;
        let spawn = std::thread::Builder::new()
            .name("window-stream-convert".into())
            .spawn(move || {
                while let Ok(frame) = rx.recv() {
                    let frame = if want_i420 {
                        frame_to_i420(&frame)
                    } else {
                        frame
                    };
                    // A panic in the caller callback must not cross
                    // the FFI boundary (that aborts the process) and
                    // must not kill capture: catch it, log it loudly,
                    // and keep delivering later frames.
                    let sequence = frame.sequence;
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        callback(frame);
                    }))
                    .is_err()
                    {
                        tracing::error!(
                            window_id = %window_id,
                            sequence,
                            "frame callback panicked; continuing with the next frame"
                        );
                    }
                }
            });
        if let Err(e) = spawn {
            return Err(PlatformError::Os(format!(
                "failed to spawn capture worker thread: {e}"
            )));
        }
        let this = Self::alloc().set_ivars(OutputIvars {
            sender: tx,
            window_id,
            sequence: AtomicU64::new(0),
            worker_gone_logged: AtomicBool::new(false),
        });
        // SAFETY: NSObject init on a freshly allocated object.
        let initialized: Retained<Self> = unsafe { msg_send![super(this), init] };
        Ok(initialized)
    }
}

// ---------------------------------------------------------------------------
// SCStreamDelegate implementation object.
//
// Records stream death so the host can distinguish "the OS stopped
// the stream" from "no frames arrived yet".

pub struct DelegateIvars {
    error: Arc<Mutex<Option<PlatformError>>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and
    // StreamDelegate does not implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "WindowStreamStreamDelegate"]
    #[ivars = DelegateIvars]
    pub struct StreamDelegate;

    unsafe impl NSObjectProtocol for StreamDelegate {}

    unsafe impl SCStreamDelegate for StreamDelegate {
        #[unsafe(method(stream:didStopWithError:))]
        #[allow(non_snake_case)]
        fn stream_didStopWithError(&self, _stream: &SCStream, error: &NSError) {
            let classified = classify_ns_error(error, None);
            tracing::error!("SCStream stopped with error: {classified}");
            if let Ok(mut guard) = self.ivars().error.lock() {
                *guard = Some(classified);
            }
        }

        #[unsafe(method(streamDidBecomeActive:))]
        #[allow(non_snake_case)]
        fn streamDidBecomeActive(&self, _stream: &SCStream) {
            tracing::info!("SCStream became active again");
        }

        #[unsafe(method(streamDidBecomeInactive:))]
        #[allow(non_snake_case)]
        fn streamDidBecomeInactive(&self, _stream: &SCStream) {
            tracing::warn!("SCStream became inactive (all captured windows closed?)");
        }
    }
);

impl StreamDelegate {
    fn new(error: Arc<Mutex<Option<PlatformError>>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { error });
        // SAFETY: NSObject init on a freshly allocated object.
        let initialized: Retained<Self> = unsafe { msg_send![super(this), init] };
        initialized
    }
}

// ---------------------------------------------------------------------------
// Session.

pub struct MacCaptureSession {
    window_id: WindowId,
    // Kept alive for the lifetime of the capture; dropping the
    // session drops these, after stop() has run.
    stream: Retained<SCStream>,
    #[allow(dead_code)]
    output: Retained<StreamOutput>,
    #[allow(dead_code)]
    delegate: Retained<StreamDelegate>,
    #[allow(dead_code)]
    queue: dispatch2::DispatchRetained<DispatchQueue>,
    running: Arc<AtomicBool>,
    stream_error: Arc<Mutex<Option<PlatformError>>>,
}

// SAFETY: `CaptureSession` requires `Send`. The retained
// ScreenCaptureKit objects inside are not auto-`Send` (NSObject
// carries no thread guarantee in the bindings), but SCStream is
// documented by Apple as usable from any thread, start/stop are
// serialised through `&mut self` / the session's own state, and the
// frame callback is only ever invoked by ScreenCaptureKit on the
// session's serial dispatch queue. Moving the session between
// threads (the `Send` use) never accesses it concurrently.
unsafe impl Send for MacCaptureSession {}

impl MacCaptureSession {
    pub fn start(
        window_id: WindowId,
        config: CaptureConfig,
        callback: FrameCallback,
    ) -> Result<Self> {
        let (window, display) = fetch_window_with_display(window_id)?;
        let window = window.0;
        let display = display.0;

        // SAFETY: the block of calls below follows the documented
        // ScreenCaptureKit setup sequence: filter -> scale ->
        // configuration -> stream -> add output -> start. All objects
        // are retained by the session for as long as the stream runs.
        unsafe {
            // Display filter including exactly this window: the
            // documented path for ordinary app windows.
            let filter = SCContentFilter::initWithDisplay_includingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::from_retained_slice(std::slice::from_ref(&window)),
            );

            // Default output size: the window's point size scaled to
            // pixels. An explicit config size is already in pixels
            // and is used as-is.
            let (out_w, out_h) = if config.width > 0 && config.height > 0 {
                (config.width as usize, config.height as usize)
            } else {
                let frame = window.frame();
                let scale = {
                    let info = SCShareableContent::infoForFilter(&filter);
                    let s = info.pointPixelScale() as f64;
                    if s > 0.0 && s.is_finite() {
                        s
                    } else {
                        tracing::warn!(
                            "SCShareableContentInfo pointPixelScale was {s}; assuming 1.0"
                        );
                        1.0
                    }
                };
                (
                    ((frame.size.width * scale).round() as usize).max(1),
                    ((frame.size.height * scale).round() as usize).max(1),
                )
            };

            let stream_config = SCStreamConfiguration::new();
            stream_config.setWidth(out_w);
            stream_config.setHeight(out_h);
            stream_config.setPixelFormat(match config.pixel_format {
                PixelFormat::Bgra8888 => PixelFormat::macos_bgra_ostype(),
                PixelFormat::I420 => PixelFormat::macos_i420_ostype(),
            });
            stream_config.setShowsCursor(config.show_cursor);
            stream_config.setQueueDepth(5);
            if config.max_fps > 0 {
                stream_config.setMinimumFrameInterval(CMTime::new(1, config.max_fps as i32));
            }

            let stream_error: Arc<Mutex<Option<PlatformError>>> = Arc::new(Mutex::new(None));
            let delegate = StreamDelegate::new(Arc::clone(&stream_error));
            let delegate_obj =
                ProtocolObject::<dyn SCStreamDelegate>::from_retained(delegate.clone());
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &stream_config,
                Some(&delegate_obj),
            );

            let output = StreamOutput::new(
                window_id,
                config.pixel_format == PixelFormat::I420,
                callback,
            )?;
            let queue = DispatchQueue::new("dev.window-stream.capture", None);
            let protocol_output =
                ProtocolObject::<dyn SCStreamOutput>::from_retained(output.clone());
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    &protocol_output,
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| classify_ns_error(&e, Some(window_id)))?;

            // startCapture is completion-handler based; bridge it to
            // a blocking wait so `start` only returns once capture
            // is actually running (or has failed). 60s, because the
            // first run can trigger the Screen Recording prompt.
            let (tx, rx) = std::sync::mpsc::channel::<Option<PlatformError>>();
            let start_block = RcBlock::new(move |error: *mut NSError| {
                let err = if error.is_null() {
                    None
                } else {
                    // SAFETY: error is a valid NSError inside this
                    // completion handler (outer unsafe block).
                    Some(classify_block_error(error, Some(window_id)))
                };
                let _ = tx.send(err);
            });
            stream.startCaptureWithCompletionHandler(Some(&start_block));
            match rx.recv_timeout(QUERY_TIMEOUT) {
                Ok(None) => {}
                Ok(Some(e)) => {
                    return Err(e);
                }
                Err(_) => {
                    return Err(PlatformError::Timeout(format!(
                        "SCStream startCapture did not complete within {QUERY_TIMEOUT:?}"
                    )));
                }
            }

            Ok(Self {
                window_id,
                stream,
                output,
                delegate,
                queue,
                running: Arc::new(AtomicBool::new(true)),
                stream_error,
            })
        }
    }
}

impl CaptureSession for MacCaptureSession {
    fn window_id(&self) -> WindowId {
        self.window_id
    }

    fn stream_error(&self) -> Option<PlatformError> {
        self.stream_error.lock().ok().and_then(|g| g.clone())
    }

    fn stop(&mut self) -> Result<()> {
        if !self.running.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        let (tx, rx) = std::sync::mpsc::channel::<Option<PlatformError>>();
        let block = RcBlock::new(move |error: *mut NSError| {
            let err = if error.is_null() {
                None
            } else {
                // SAFETY: error is a valid NSError inside this
                // completion handler.
                Some(unsafe { classify_block_error(error, None) })
            };
            let _ = tx.send(err);
        });
        // SAFETY: stream is live; the handler only signals the channel.
        unsafe {
            self.stream.stopCaptureWithCompletionHandler(Some(&block));
        }
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Some(e)) => tracing::warn!("SCStream stopCapture reported an error: {e}"),
            Ok(None) => {}
            Err(_) => tracing::warn!(
                window_id = %self.window_id,
                "SCStream stopCapture did not complete within 5s; continuing teardown"
            ),
        }
        Ok(())
    }
}

impl Drop for MacCaptureSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
