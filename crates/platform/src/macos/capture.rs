//! Per-window capture via `SCStream` (ScreenCaptureKit).
//!
//! One `SCStream` per window, built on an
//! `SCContentFilter::initWithDesktopIndependentWindow` filter, so the
//! OS delivers frames for exactly that window. Frames arrive as
//! `CMSampleBuffer`s wrapping a `CVPixelBuffer` on a dedicated
//! serial dispatch queue; the output object copies the pixels out
//! (the OS recycles its buffers), optionally converts BGRA -> I420,
//! and invokes the caller's callback.
//!
//! UNVERIFIED ON HARDWARE -- see `mod.rs` and
//! `docs/MACOS_VERIFICATION.md`. In particular the `declare_class!`
//! output-object wiring and the `CVPixelBuffer` copy path have not
//! run on a Mac yet.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType,
};

use crate::convert::bgra_to_i420;
use crate::traits::{CaptureSession, FrameCallback};
use crate::types::{CaptureConfig, CapturedFrame, PixelFormat, PlatformError, Result, WindowId};

use super::enumerate::fetch_window;

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
/// Returns `None` for samples without an image buffer (e.g. status /
/// audio samples, which this crate never subscribes to).
///
/// Safety: `sample` must be a live `CMSampleBuffer` for the duration
/// of the call (guaranteed inside the SCStreamOutput callback).
unsafe fn copy_frame(
    sample: &CMSampleBuffer,
    window_id: WindowId,
    sequence: u64,
    want_i420: bool,
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
    let timestamp = if pts.timescale > 0 && pts.value >= 0 {
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
        if want_i420 {
            let (data, strides) = bgra_to_i420(bytes, stride, width, height);
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

pub struct OutputIvars {
    callback: Mutex<FrameCallback>,
    window_id: WindowId,
    sequence: AtomicU64,
    want_i420: bool,
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
            let frame =
                unsafe { copy_frame(sample_buffer, ivars.window_id, sequence, ivars.want_i420) };
            if let Some(frame) = frame {
                if let Ok(mut cb) = ivars.callback.lock() {
                    cb(frame);
                }
            }
        }
    }
);

impl StreamOutput {
    fn new(window_id: WindowId, want_i420: bool, callback: FrameCallback) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars {
            callback: Mutex::new(callback),
            window_id,
            sequence: AtomicU64::new(0),
            want_i420,
        });
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
    queue: dispatch2::DispatchRetained<DispatchQueue>,
    running: Arc<AtomicBool>,
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
        let window = fetch_window(window_id)?.0;

        // Resolve output size: config wins, else the window's frame
        // in points (ScreenCaptureKit scales to pixels itself).
        let (out_w, out_h) = if config.width > 0 && config.height > 0 {
            (config.width as usize, config.height as usize)
        } else {
            // SAFETY: property read on a live SCWindow.
            let frame = unsafe { window.frame() };
            (
                (frame.size.width as usize).max(1),
                (frame.size.height as usize).max(1),
            )
        };

        // SAFETY: the block of calls below follows the documented
        // ScreenCaptureKit setup sequence: filter -> configuration ->
        // stream -> add output -> start. All objects are retained by
        // the session for as long as the stream runs.
        unsafe {
            let filter = SCContentFilter::initWithDesktopIndependentWindow(
                SCContentFilter::alloc(),
                &window,
            );
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

            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &stream_config,
                None,
            );

            let output = StreamOutput::new(
                window_id,
                config.pixel_format == PixelFormat::I420,
                callback,
            );
            let queue = DispatchQueue::new("dev.window-stream.capture", None);
            let protocol_output =
                ProtocolObject::<dyn SCStreamOutput>::from_retained(output.clone());
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    &protocol_output,
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| {
                    PlatformError::Os(format!(
                        "SCStream addStreamOutput failed: {}",
                        e.localizedDescription()
                    ))
                })?;

            // startCapture is completion-handler based; bridge it to
            // a blocking wait so `start` only returns once capture
            // is actually running (or has failed).
            let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
            let start_block = RcBlock::new(move |error: *mut NSError| {
                let msg = if error.is_null() {
                    None
                } else {
                    Some((*error).localizedDescription().to_string())
                };
                let _ = tx.send(msg);
            });
            stream.startCaptureWithCompletionHandler(Some(&start_block));
            match rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(None) => {}
                Ok(Some(msg)) => {
                    return Err(PlatformError::PermissionDenied(format!(
                        "SCStream startCapture failed: {msg}. \
                         See docs/PERMISSIONS.md (Screen Recording permission)"
                    )));
                }
                Err(_) => {
                    return Err(PlatformError::Timeout(
                        "SCStream startCapture did not complete".into(),
                    ));
                }
            }

            Ok(Self {
                window_id,
                stream,
                output,
                queue,
                running: Arc::new(AtomicBool::new(true)),
            })
        }
    }
}

impl CaptureSession for MacCaptureSession {
    fn window_id(&self) -> WindowId {
        self.window_id
    }

    fn stop(&mut self) -> Result<()> {
        if !self.running.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let block = RcBlock::new(move |_error: *mut NSError| {
            let _ = tx.send(());
        });
        // SAFETY: stream is live; the handler only signals the channel.
        unsafe {
            self.stream.stopCaptureWithCompletionHandler(Some(&block));
        }
        let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
        Ok(())
    }
}

impl Drop for MacCaptureSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
