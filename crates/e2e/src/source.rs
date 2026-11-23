//! Frame sources for the e2e host pipeline.
//!
//! The host used to call `codec::pattern::moving_rectangle` directly.
//! That hard-wired the dummy test pattern into the VP9 / WebTransport
//! path, so real capture could only ever be a second, parallel
//! pipeline. The `FrameSource` trait is the seam that fixes that:
//! the host pulls `I420Frame`s from a source, encodes them, and sends
//! them — it never knows (or cares) whether the pixels came from a
//! synthetic pattern or from a real window.
//!
//! Sources in v1:
//!
//! * [`PatternSource`] — the deterministic moving-rectangle pattern.
//!   Kept as the default and as the only source that can run on this
//!   VM / in CI. It is also the only source with a deterministic
//!   reference frame, so only it feeds the in-process PSNR check.
//! * [`CapturedFrameSource`] — pulls platform [`CapturedFrame`]s
//!   (BGRA from ScreenCaptureKit, or I420 when the OS delivers it)
//!   off a bounded channel and converts them to I420. The channel
//!   is depth 1: a stale frame is dropped, never queued (the
//!   implementation policy in `docs/IMPLEMENTATION.md`).
//! * [`RealCaptureSource`] — [`CapturedFrameSource`] fed by a live
//!   platform capture session (`platform::create_backend()`), i.e.
//!   ScreenCaptureKit on macOS. On any other OS the stub backend
//!   makes `start` fail fast instead of faking frames.
//!
//! Conversion notes (BGRA -> I420, Retina included):
//!
//! * ScreenCaptureKit delivers BGRA at the window's *pixel* size —
//!   on a Retina display that is the point size times the backing
//!   scale (2x), e.g. a 1680x1050 pt window arrives as 3360x2100 px.
//!   The conversion in `platform::convert` is size-agnostic, so no
//!   special-casing is needed here; the encoder is sized from the
//!   first frame that arrives, so Retina sizes flow through
//!   automatically.
//! * VP9 / I420 needs even dimensions. A window whose pixel size is
//!   odd is cropped by one row / column (the bottom row and the
//!   right column are dropped) in [`captured_frame_to_i420`]; the
//!   alternative — padding — would invent pixels at the edge.
//! * A mid-stream size change (window resized) is *not* handled by
//!   the source: the host bails with an error and the operator
//!   restarts the host. The client already recreates textures on
//!   size change, but the encoder is fixed-size in v1.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use codec::pattern::moving_rectangle;
use codec::I420Frame;
use tracing::info;
use window_stream_platform::{
    convert::frame_to_i420, create_backend, CaptureConfig, CaptureSession, CapturedFrame,
    PixelFormat, PlatformError, WindowId,
};

use crate::{E2E_HEIGHT, E2E_TITLE, E2E_WIDTH, E2E_WINDOW_ID};

/// How long a captured-frame source waits for the next frame before
/// reporting a stall. ScreenCaptureKit delivers at up to the
/// configured fps (30 by default), so 5 s of silence means the
/// stream is dead or the window is gone — not a slow frame.
pub const CAPTURE_FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// A deterministic reference frame for the in-process PSNR check:
/// given the frame size and sequence, reproduce the exact I420 frame
/// the source *should* have produced.
pub type ReferenceFn = fn(u32, u32, u64) -> Option<I420Frame>;

/// A pull source of I420 frames for the host pipeline.
///
/// `next_frame` is blocking: the host runs on its own dedicated
/// thread with a current-thread runtime (see `start_host`), so a
/// blocking wait for the next captured frame does not stall any
/// other task; FrameAcks are collected in the pacing window after
/// each encode. The returned frame's `seq` is the source's own
/// sequence (pattern index, or the capture session's sequence).
pub trait FrameSource {
    fn next_frame(&mut self) -> Result<I420Frame>;
    fn window_id(&self) -> u64;
    fn title(&self) -> &str;

    /// Optional deterministic reference for the in-process PSNR
    /// check: given the frame size and sequence, reproduce the exact
    /// I420 frame the source *should* have produced. `None` (the
    /// default) means the content is not reproducible — real
    /// captured pixels — so no PSNR is computed for it. Returned as
    /// a plain function pointer (not a closure over `self`) so the
    /// host can hold it while also mutably pulling frames.
    fn reference_fn(&self) -> Option<ReferenceFn> {
        None
    }
}

fn pattern_reference(width: u32, height: u32, seq: u64) -> Option<I420Frame> {
    moving_rectangle(width, height, seq).ok()
}

// ---------------------------------------------------------------------------
// Pattern (dummy) source
// ---------------------------------------------------------------------------

/// The deterministic moving-rectangle test pattern, as a
/// [`FrameSource`]. Same pixels as the old hard-wired call, now
/// behind the trait so the host loop is source-agnostic.
pub struct PatternSource {
    seq: u64,
}

impl PatternSource {
    pub fn new() -> Self {
        Self { seq: 0 }
    }
}

impl Default for PatternSource {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameSource for PatternSource {
    fn next_frame(&mut self) -> Result<I420Frame> {
        let frame = moving_rectangle(E2E_WIDTH, E2E_HEIGHT, self.seq).context("pattern frame")?;
        self.seq += 1;
        Ok(frame)
    }

    fn window_id(&self) -> u64 {
        E2E_WINDOW_ID
    }

    fn title(&self) -> &str {
        E2E_TITLE
    }

    fn reference_fn(&self) -> Option<ReferenceFn> {
        Some(pattern_reference)
    }
}

// ---------------------------------------------------------------------------
// CapturedFrame -> I420 conversion (the BGRA / Retina step)
// ---------------------------------------------------------------------------

/// Convert one platform captured frame to a codec [`I420Frame`].
///
/// BGRA frames go through `platform::convert::frame_to_i420`
/// (BT.601 limited-range); I420 frames pass through unchanged.
/// Odd widths / heights are cropped to even (see the module docs).
/// Strided planes (padded rows from the OS) are handled by
/// `I420Frame::from_strided`, which copies only the visible columns.
pub fn captured_frame_to_i420(frame: &CapturedFrame) -> Result<I420Frame> {
    let converted = frame_to_i420(frame);
    let width = converted.width & !1;
    let height = converted.height & !1;
    if width == 0 || height == 0 {
        return Err(anyhow!(
            "captured frame {0}x{1} is too small to crop to even I420 dimensions",
            converted.width,
            converted.height
        ));
    }
    if converted.strides.len() < 3 {
        return Err(anyhow!(
            "I420 frame needs 3 plane strides, got {:?}",
            converted.strides
        ));
    }
    // Plane offsets come from the *converted* (uncropped) size: the
    // U plane starts after the full Y plane, even when the height is
    // cropped by one row below.
    let oh = converted.height as usize;
    let y_len = converted.strides[0]
        .checked_mul(oh)
        .context("Y plane size overflow")?;
    let ch = oh.div_ceil(2);
    let u_len = converted.strides[1]
        .checked_mul(ch)
        .context("U plane size overflow")?;
    let v_len = converted.strides[2]
        .checked_mul(ch)
        .context("V plane size overflow")?;
    let total = y_len
        .checked_add(u_len)
        .and_then(|n| n.checked_add(v_len))
        .context("captured frame size overflow")?;
    if converted.data.len() < total {
        return Err(anyhow!(
            "captured frame data too short: need {total} bytes, got {}",
            converted.data.len()
        ));
    }
    let y = &converted.data[..y_len];
    let u = &converted.data[y_len..y_len + u_len];
    let v = &converted.data[y_len + u_len..];
    I420Frame::from_strided(
        width,
        height,
        frame.sequence,
        y,
        converted.strides[0],
        u,
        converted.strides[1],
        v,
        converted.strides[2],
    )
    .context("captured frame is not a valid I420 frame")
}

// ---------------------------------------------------------------------------
// Channel-fed captured-frame source
// ---------------------------------------------------------------------------

/// A [`FrameSource`] fed by [`CapturedFrame`]s arriving on a
/// bounded (depth-1) channel. This is the testable half of real
/// capture: tests (and any future non-ScreenCaptureKit backend) can
/// push synthetic BGRA frames in and exercise the exact conversion
/// + encode + send path the real source uses, with no OS involved.
pub struct CapturedFrameSource {
    rx: Receiver<CapturedFrame>,
    window_id: u64,
    title: String,
}

impl CapturedFrameSource {
    pub fn new(rx: Receiver<CapturedFrame>, window_id: u64, title: impl Into<String>) -> Self {
        Self {
            rx,
            window_id,
            title: title.into(),
        }
    }

    /// Create a depth-1 channel and the source reading from it.
    /// Senders use `SyncSender::send` (blocks when a stale frame is
    /// still unconsumed) or `try_send` (drops the stale frame) —
    /// see `RealCaptureSource` for the drop-stale choice.
    pub fn channel(window_id: u64, title: impl Into<String>) -> (SyncSender<CapturedFrame>, Self) {
        let (tx, rx) = sync_channel::<CapturedFrame>(1);
        (tx, Self::new(rx, window_id, title))
    }
}

impl FrameSource for CapturedFrameSource {
    fn next_frame(&mut self) -> Result<I420Frame> {
        let frame = self.rx.recv_timeout(CAPTURE_FRAME_TIMEOUT).map_err(|e| {
            anyhow!(
                "timed out after {CAPTURE_FRAME_TIMEOUT:?} waiting for a captured frame \
                 (capture stalled or window closed; {e})"
            )
        })?;
        captured_frame_to_i420(&frame)
    }

    fn window_id(&self) -> u64 {
        self.window_id
    }

    fn title(&self) -> &str {
        &self.title
    }
}

// ---------------------------------------------------------------------------
// Live platform capture source
// ---------------------------------------------------------------------------

/// A [`FrameSource`] backed by a live platform capture session.
///
/// On macOS this is ScreenCaptureKit per-window capture
/// (`platform::create_backend()`); on every other OS the stub
/// backend makes [`RealCaptureSource::start`] fail fast with a clear
/// error instead of producing fake frames. The session is held for
/// the source's lifetime: dropping the source stops the OS stream.
pub struct RealCaptureSource {
    inner: CapturedFrameSource,
    session: Box<dyn CaptureSession>,
}

impl RealCaptureSource {
    /// Start capturing `window_id` at up to `max_fps` fps.
    ///
    /// The window is looked up first (so a bad id fails before any
    /// stream is created), then capture starts in BGRA — the layout
    /// ScreenCaptureKit produces most cheaply — and the callback
    /// feeds the depth-1 channel, dropping stale frames.
    pub fn start(window_id: u32, max_fps: u32) -> Result<Self> {
        let backend = create_backend();
        info!(
            backend = backend.name(),
            window_id, max_fps, "starting real capture"
        );
        let info = backend
            .list_windows()
            .context("list windows for real capture")?
            .into_iter()
            .find(|w| w.id == WindowId(window_id))
            .ok_or_else(|| {
                anyhow!(
                    "window {window_id} not found (backend {}; list the capturable windows \
                     with `platform_probe --list` on the Mac)",
                    backend.name()
                )
            })?;
        let title = info
            .title
            .clone()
            .unwrap_or_else(|| format!("window {window_id}"));
        let (tx, rx) = sync_channel::<CapturedFrame>(1);
        let callback = Box::new(move |frame: CapturedFrame| {
            // Depth-1 policy: if the host is behind, drop this
            // (stale) frame rather than blocking the capture thread.
            if let Err(TrySendError::Disconnected(_)) = tx.try_send(frame) {
                // The source is gone; nothing left to do.
            }
        });
        let config = CaptureConfig {
            width: 0,
            height: 0,
            max_fps,
            pixel_format: PixelFormat::Bgra8888,
            show_cursor: false,
        };
        let session = backend
            .start_capture(WindowId(window_id), config, callback)
            .with_context(|| {
                format!(
                    "start_capture for window {window_id} failed \
                     (on macOS this is usually the Screen Recording permission; \
                     see crates/platform/docs/PERMISSIONS.md)"
                )
            })?;
        Ok(Self {
            inner: CapturedFrameSource::new(rx, u64::from(window_id), title),
            session,
        })
    }

    /// The error the backend observed if the OS stream died
    /// (macOS: `SCStreamDelegate stream:didStopWithError:`), or
    /// `None` if no stream death has been observed.
    pub fn stream_error(&self) -> Option<PlatformError> {
        self.session.stream_error()
    }
}

impl FrameSource for RealCaptureSource {
    fn next_frame(&mut self) -> Result<I420Frame> {
        self.inner.next_frame().map_err(|e| {
            if let Some(stream_err) = self.session.stream_error() {
                anyhow!("{e}; capture stream error: {stream_err}")
            } else {
                e
            }
        })
    }

    fn window_id(&self) -> u64 {
        self.inner.window_id()
    }

    fn title(&self) -> &str {
        self.inner.title()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use window_stream_platform::types::WindowId as PlatformWindowId;

    fn bgra_frame(width: u32, height: u32, seq: u64, fill: [u8; 4]) -> CapturedFrame {
        let mut data = Vec::with_capacity(width as usize * height as usize * 4);
        for _ in 0..width * height {
            data.extend_from_slice(&fill);
        }
        CapturedFrame {
            window_id: PlatformWindowId(7),
            width,
            height,
            format: PixelFormat::Bgra8888,
            strides: vec![width as usize * 4],
            data,
            timestamp: None,
            sequence: seq,
        }
    }

    #[test]
    fn bgra_solid_red_converts_to_i420() {
        // Pure red BGRA (B=0,G=0,R=255): BT.601 limited Y=82, U=90, V=240.
        let frame = bgra_frame(4, 4, 3, [0, 0, 255, 255]);
        let out = captured_frame_to_i420(&frame).expect("convert red");
        assert_eq!((out.width(), out.height()), (4, 4));
        assert_eq!(out.seq(), 3);
        assert!(out.y().iter().all(|&v| v == 82));
        assert!(out.u().iter().all(|&v| v == 90));
        assert!(out.v().iter().all(|&v| v == 240));
    }

    #[test]
    fn odd_size_is_cropped_to_even() {
        // 5x3 BGRA (odd in both axes) -> 4x2 I420; the dropped row
        // and column are the bottom / right edge.
        let frame = bgra_frame(5, 3, 0, [0, 0, 0, 255]);
        let out = captured_frame_to_i420(&frame).expect("convert odd size");
        assert_eq!((out.width(), out.height()), (4, 2));
        assert_eq!(out.y().len(), 8);
        assert_eq!(out.u().len(), 2);
    }

    #[test]
    fn i420_with_padded_stride_passes_through() {
        // I420 with stride padding (Y stride 8 for width 4): the
        // padding bytes must not leak into the packed output.
        let width = 4u32;
        let height = 2u32;
        let mut data = Vec::new();
        for _ in 0..2 {
            data.extend_from_slice(&[10, 20, 30, 40, 99, 99, 99, 99]);
        }
        data.extend_from_slice(&[50, 60, 77, 77]);
        data.extend_from_slice(&[70, 80, 88, 88]);
        let frame = CapturedFrame {
            window_id: PlatformWindowId(7),
            width,
            height,
            format: PixelFormat::I420,
            strides: vec![8, 4, 4],
            data,
            timestamp: None,
            sequence: 9,
        };
        let out = captured_frame_to_i420(&frame).expect("convert strided I420");
        assert_eq!((out.width(), out.height()), (4, 2));
        assert_eq!(out.y(), &[10, 20, 30, 40, 10, 20, 30, 40]);
        assert_eq!(out.u(), &[50, 60]);
        assert_eq!(out.v(), &[70, 80]);
    }

    #[test]
    fn one_pixel_frame_is_too_small() {
        let frame = bgra_frame(1, 1, 0, [0, 0, 0, 255]);
        assert!(captured_frame_to_i420(&frame).is_err());
    }

    #[test]
    fn pattern_source_counts_and_references() {
        let mut src = PatternSource::new();
        assert_eq!(src.window_id(), E2E_WINDOW_ID);
        let first = src.next_frame().expect("first pattern frame");
        assert_eq!(first.seq(), 0);
        let second = src.next_frame().expect("second pattern frame");
        assert_eq!(second.seq(), 1);
        let reference = src.reference_fn().expect("pattern has a reference");
        let reproduced = reference(E2E_WIDTH, E2E_HEIGHT, 0).expect("reproduce frame 0");
        assert_eq!(reproduced, first);
    }

    #[test]
    fn captured_source_times_out_with_no_frames() {
        let (_tx, mut src) = CapturedFrameSource::channel(7, "test");
        // No sender activity: recv_timeout fires. (The sender is
        // dropped with `_tx` at the end of this scope; the timeout
        // path is what is being exercised, not the disconnect.)
        let err = src.next_frame().expect_err("no frames: must time out");
        assert!(err.to_string().contains("timed out"), "{err}");
    }
}
