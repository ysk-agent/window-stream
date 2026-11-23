//! window-stream client library.
//!
//! The compositor and window types are a library target so the transport /
//! codec tracks can feed frames without going through the demo binary.
//! The seam is [`sink::FrameSink`]: `upsert_window` / `remove_window` /
//! `submit_frame`, implemented by [`compositor::Compositor`].

pub mod compositor;
pub mod demo;
pub mod input;
pub mod remote_window;
pub mod sink;
