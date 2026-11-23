//! Platform-common traits.
//!
//! Everything OS-specific in window-stream sits behind these traits
//! (adapter form, see `docs/IMPLEMENTATION.md` in the repository root).
//! The core protocol, transport and codec crates never see an OS type:
//! they enumerate [`WindowInfo`], receive [`CapturedFrame`]s through a
//! callback, and inject [`KeyEvent`] / [`PointerEvent`]s.
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
    /// with [`PlatformError::PermissionDenied`] when Screen Recording
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
pub trait CaptureSession: Send {
    fn window_id(&self) -> WindowId;
    fn stop(&mut self) -> Result<()>;
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
