//! `platform` crate for window-stream.
//!
//! OS adapters behind common traits (see [`traits`]): window
//! enumeration, per-window capture with a frame callback, and
//! keyboard / pointer input injection. The fixed design decisions
//! this crate encodes (repository `docs/IMPLEMENTATION.md`):
//!
//! * macOS capture is ScreenCaptureKit, per window.
//! * macOS input injection is CoreGraphics `CGEvent`.
//! * Everything OS-specific is `cfg`-gated inside this crate; the
//!   protocol / transport / codec crates only see the traits.
//!
//! On non-macOS targets a stub backend is provided so the crate
//! builds and its OS-independent parts (types, BGRA -> I420
//! conversion) can be tested anywhere. The stub never fakes frames:
//! capture and input return `Unsupported`, enumeration is empty.

pub mod convert;
pub mod traits;
pub mod types;

#[cfg(target_os = "macos")]
pub mod macos;

pub mod stub;

pub use traits::{
    CaptureSession, FrameCallback, InputInjector, PlatformBackend, WindowCapturer, WindowEnumerator,
};
pub use types::{
    CaptureConfig, CapturedFrame, KeyEvent, PixelFormat, PlatformError, PointerButton,
    PointerEvent, Rect, Result, WindowId, WindowInfo,
};

/// Create the backend for the current OS.
///
/// macOS: [`macos::MacBackend`]. Everywhere else: [`stub::StubBackend`].
pub fn create_backend() -> Box<dyn PlatformBackend> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacBackend)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(stub::StubBackend)
    }
}
