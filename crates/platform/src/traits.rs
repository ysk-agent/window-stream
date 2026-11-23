//! Platform-common traits.
//!
//! Everything OS-specific in window-stream sits behind these traits
//! (adapter form, see `docs/IMPLEMENTATION.md` in the repository root).
//! The core protocol, transport and codec crates never see an OS type:
//! they enumerate [`crate::types::WindowInfo`], receive
//! [`crate::types::CapturedFrame`]s through a callback, and inject
//! [`crate::types::KeyEvent`] / [`crate::types::PointerEvent`]s.
//!
//! Threading: capture backends invoke the frame callback on their own
//! (OS-owned) queue/thread. Callbacks must therefore be `Send`, must
//! not block, and -- per the implementation policy -- feed a depth-1
//! queue: a stale frame is dropped, never queued.

use crate::types::{
    CaptureConfig, CapturedFrame, KeyEvent, PointerEvent, Result, WindowId, WindowInfo,
};

/// Callback invoked for every captured frame.
pub type FrameCallback = Box<dyn FnMut(CapturedFrame) + Send>;

/// Enumerates the top-level windows that could be captured.
pub trait WindowEnumerator: Send + Sync {
    /// List all shareable / capturable windows.
    ///
    /// On macOS this is one `SCShareableContent` query. It may fail
    /// with [`crate::types::PlatformError::PermissionDenied`] when Screen Recording
    /// permission has not been granted (see `docs/PERMISSIONS.md`).
    fn list_windows(&self) -> Result<Vec<WindowInfo>>;

    /// Convenience: look up one window by id.
    fn window(&self, id: WindowId) -> Result<Option<WindowInfo>> {
        Ok(self.list_windows()?.into_iter().find(|w| w.id == id))
    }
}

/// A running per-window capture session.
///
/// Dropping the session (or calling [`CaptureSession::stop`]) stops
/// the underlying OS capture. There is at most one session per window
/// in v1.
///
/// v1 has no push notification for geometry changes or for a stream
/// dying, so the host must (a) poll [`WindowEnumerator::list_windows`]
/// to notice window moves / resizes / closes, and (b) treat frame
/// silence -- no frame for more than a few frame intervals -- as a
/// failure, and check [`CaptureSession::stream_error`] to see whether
/// the backend observed the stream stop (macOS reports the
/// `SCStreamDelegate` error there). A future version may add an
/// event channel; until then polling + silence detection is the
/// contract, not a workaround.
pub trait CaptureSession: Send {
    fn window_id(&self) -> WindowId;
    fn stop(&mut self) -> Result<()>;

    /// The error the backend observed if the OS stream died
    /// (macOS: `SCStreamDelegate stream:didStopWithError:`), or
    /// `None` if no stream death has been observed. Poll this when
    /// frames go silent; backends that cannot observe stream death
    /// return `None` and frame silence is the only signal.
    fn stream_error(&self) -> Option<crate::types::PlatformError> {
        None
    }
}

/// Starts per-window captures.
pub trait WindowCapturer: Send + Sync {
    /// Start capturing `window_id` and deliver frames to `callback`.
    ///
    /// Frames are delivered in the `config.pixel_format` layout
    /// (`Bgra8888` or `I420`). The returned session owns the OS
    /// resources; capture runs until the session is stopped/dropped.
    fn start_capture(
        &self,
        window_id: WindowId,
        config: CaptureConfig,
        callback: FrameCallback,
    ) -> Result<Box<dyn CaptureSession>>;
}

/// Injects synthetic keyboard / pointer input into the OS.
pub trait InputInjector: Send + Sync {
    fn inject_key(&self, event: KeyEvent) -> Result<()>;
    fn inject_pointer(&self, event: PointerEvent) -> Result<()>;

    /// Release every key/button. Used on session teardown so a lost
    /// key-up can never leave a stuck key on the host (the protocol
    /// additionally sends periodic input-state snapshots).
    ///
    /// Backends that track pressed state (macOS does, process-wide)
    /// synthesise the matching key-up / button-up events here.
    /// Backends that cannot track state document that this is a
    /// no-op and the session layer is responsible for explicit
    /// releases instead.
    fn release_all(&self) -> Result<()> {
        Ok(())
    }
}

/// One platform backend: enumeration + capture + input.
///
/// `host` builds exactly one of these at startup via
/// [`crate::create_backend`].
pub trait PlatformBackend: WindowEnumerator + WindowCapturer + InputInjector {
    /// Human-readable backend name, e.g. `"macos-screencapturekit"`
    /// or `"stub"`. Used in logs and in `platform_probe`.
    fn name(&self) -> &'static str;
}

// Concrete backends implement the three component traits above plus
// PlatformBackend (for the name) on a single type.
