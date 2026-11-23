//! Frame-ingress seam for the transport / codec tracks.
//!
//! A decoded VP9 frame arrives as an I420 frame plus its window id. The
//! sink owns the window/texture lifecycle: windows can be created, resized
//! and destroyed at any time, and a frame whose size differs from the
//! current texture is never a panic — the texture is recreated (see
//! `Compositor::submit_frame`).

use crate::demo::I420Frame;
use crate::input::WindowId;
use crate::remote_window::WindowGeometry;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("unknown window id {0}")]
    UnknownWindow(WindowId),
    #[error("invalid I420 frame for {width}x{height}: {reason}")]
    InvalidFrame { width: u32, height: u32, reason: String },
}

/// What a frame producer (transport+codec, or the demo generator) needs.
pub trait FrameSink {
    /// Create a window, or update an existing one's frame size / geometry.
    /// A frame-size change recreates the window's textures.
    fn upsert_window(
        &mut self,
        id: WindowId,
        frame_width: u32,
        frame_height: u32,
        geometry: WindowGeometry,
        title: &str,
    ) -> Result<(), FrameError>;

    /// Destroy a window and its textures. Returns true if it existed.
    fn remove_window(&mut self, id: WindowId) -> bool;

    /// Submit one I420 frame. If the frame size differs from the window's
    /// current texture size, the texture is recreated at the new size
    /// (host windows resize; this must never panic).
    fn submit_frame(&mut self, id: WindowId, frame: &I420Frame) -> Result<(), FrameError>;
}
