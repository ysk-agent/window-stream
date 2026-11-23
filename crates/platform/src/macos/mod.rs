//! macOS backend.
//!
//! * Enumeration and capture: ScreenCaptureKit via the `objc2`
//!   family (`objc2-screen-capture-kit`), per window -- never a
//!   full-display capture cut up afterwards.
//! * Input injection: CoreGraphics `CGEvent` via the `core-graphics`
//!   crate.
//!
//! UNVERIFIED ON HARDWARE: this code was written on a Linux VM. The
//! macOS configuration type-checks there via
//! `cargo check --target {x86_64,aarch64}-apple-darwin` against the
//! real `objc2` bindings (see `docs/MACOS_VERIFICATION.md` for the
//! results recorded at commit time), but it has never been linked
//! against a macOS SDK or run on a Mac. Runtime behaviour -- and
//! anything the type checker cannot see -- is therefore still
//! unverified and is called out in that document.

pub mod capture;
pub mod enumerate;
pub mod error;
pub mod input;

use crate::traits::CaptureSession;
use crate::traits::FrameCallback;
use crate::traits::{InputInjector, PlatformBackend, WindowCapturer, WindowEnumerator};
use crate::types::{CaptureConfig, KeyEvent, PointerEvent, Result, WindowId, WindowInfo};

/// The macOS backend. Enumeration and capture are stateless: every
/// enumeration performs a fresh `SCShareableContent` query and every
/// capture builds its own `SCStream`, so there is no cached window
/// state to go stale. Input injection is the exception: pressed
/// keys/buttons are tracked process-wide (see `input.rs`) so that
/// `release_all` can synthesise the matching releases.
#[derive(Debug, Default)]
pub struct MacBackend;

impl WindowEnumerator for MacBackend {
    fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        enumerate::list_windows()
    }
}

impl WindowCapturer for MacBackend {
    fn start_capture(
        &self,
        window_id: WindowId,
        config: CaptureConfig,
        callback: FrameCallback,
    ) -> Result<Box<dyn CaptureSession>> {
        let session = capture::MacCaptureSession::start(window_id, config, callback)?;
        Ok(Box::new(session))
    }
}

impl InputInjector for MacBackend {
    fn inject_key(&self, event: KeyEvent) -> Result<()> {
        input::inject_key(event)
    }
    fn inject_pointer(&self, event: PointerEvent) -> Result<()> {
        input::inject_pointer(event)
    }
    fn release_all(&self) -> Result<()> {
        input::release_all()
    }
}

impl PlatformBackend for MacBackend {
    fn name(&self) -> &'static str {
        "macos-screencapturekit"
    }
}
