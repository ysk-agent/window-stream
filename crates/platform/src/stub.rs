//! Stub backend for every non-macOS target.
//!
//! It exists so the crate (traits, types, conversion, the probe
//! binary) builds and can be unit-tested on Linux CI. It deliberately
//! does *not* fake success: enumeration returns an empty list,
//! capture and input return [`PlatformError::Unsupported`], so a
//! caller can never mistake the stub for a working backend.

use crate::traits::CaptureSession;
use crate::traits::{
    FrameCallback, InputInjector, PlatformBackend, WindowCapturer, WindowEnumerator,
};
use crate::types::{
    CaptureConfig, KeyEvent, PlatformError, PointerEvent, Result, WindowId, WindowInfo,
};

#[derive(Debug, Default)]
pub struct StubBackend;

impl WindowEnumerator for StubBackend {
    fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        Ok(Vec::new())
    }
}

impl WindowCapturer for StubBackend {
    fn start_capture(
        &self,
        window_id: WindowId,
        _config: CaptureConfig,
        _callback: FrameCallback,
    ) -> Result<Box<dyn CaptureSession>> {
        Err(PlatformError::Unsupported(format!(
            "window capture is not implemented on this OS (window {window_id}); \
             the macOS backend is the only real backend in v1"
        )))
    }
}

impl InputInjector for StubBackend {
    fn inject_key(&self, _event: KeyEvent) -> Result<()> {
        Err(PlatformError::Unsupported(
            "input injection is not implemented on this OS".into(),
        ))
    }
    fn inject_pointer(&self, _event: PointerEvent) -> Result<()> {
        Err(PlatformError::Unsupported(
            "input injection is not implemented on this OS".into(),
        ))
    }
}

impl PlatformBackend for StubBackend {
    fn name(&self) -> &'static str {
        "stub"
    }
}
