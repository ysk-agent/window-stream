//! window-stream codec track: VP9 encoding / decoding over libvpx.
//!
//! Fixed by the project design (see the repository `README.md` and
//! `docs/IMPLEMENTATION.md`): VP9 only, 8-bit 4:2:0 (I420), realtime
//! settings, no codec negotiation, no retransmission — loss is handled by
//! *reference invalidation* driven by the client's `FrameAck`, which is
//! what the encoder's reference-control API exists for.
//!
//! Layout:
//! * [`frame`] — [`I420Frame`] in, [`EncodedFrame`] out.
//! * [`config`] — [`EncoderConfig`] / [`DecoderConfig`] / [`ReferenceMode`].
//! * [`encoder::Vp9Encoder`], [`decoder::Vp9Decoder`] — libvpx FFI behind
//!   the [`VideoEncoder`] / [`VideoDecoder`] traits.
//! * [`ivf`] — IVF dump/read for debugging bitstreams outside the protocol.
//! * [`pattern`] — deterministic synthetic frames for tests and benches.
//!
//! This crate is deliberately standalone (no workspace yet); see the crate
//! README for how it joins the future workspace.

pub mod config;
pub mod decoder;
pub mod encoder;
pub mod error;
pub mod frame;
pub mod ivf;
pub mod pattern;
pub mod sys;

pub use config::{DecoderConfig, EncoderConfig, ReferenceMode};
pub use decoder::Vp9Decoder;
pub use encoder::Vp9Encoder;
pub use error::{CodecError, Result};
pub use frame::{luma_psnr, EncodedFrame, I420Frame};
pub use ivf::{IvfReader, IvfWriter};

/// A VP9 encoder: I420 frames in, VP9 packets out.
///
/// Implementations are single-threaded objects: one instance per stream
/// (per window), driven from that stream's encode thread.
pub trait VideoEncoder {
    /// Encode one frame. Returns the packets libvpx emitted for it
    /// (realtime configuration: normally exactly one; may be empty if
    /// libvpx dropped the frame under `rc_dropframe_thresh`).
    fn encode(&mut self, frame: &I420Frame) -> Result<Vec<EncodedFrame>>;

    /// Drain any frames still held inside the encoder. With the realtime
    /// configuration (`g_lag_in_frames = 0`) this normally returns nothing.
    fn flush(&mut self) -> Result<Vec<EncodedFrame>>;

    /// Force the next encoded frame to be a keyframe.
    fn force_keyframe(&mut self);

    /// Record the client's acknowledgement: `acked_seq` is the sequence
    /// number of the last frame the client successfully decoded (the
    /// protocol's `FrameAck`). See the implementation's documentation for
    /// exactly how this constrains reference use — libvpx cannot express
    /// "reference only frames <= ack" directly.
    fn set_reference_constraint(&mut self, acked_seq: Option<u64>);

    /// The stream's reference chain may be broken (a frame was lost):
    /// invalidate all references. The next frame is encoded as a keyframe.
    fn invalidate_reference(&mut self);
}

/// A VP9 decoder: VP9 packets in, I420 frames out.
pub trait VideoDecoder {
    /// Decode one packet. Returns the frames that became available
    /// (normally exactly one for this crate's realtime streams).
    fn decode(&mut self, packet: &EncodedFrame) -> Result<Vec<I420Frame>>;
}
