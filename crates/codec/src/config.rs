//! Encoder / decoder configuration.

use crate::error::{CodecError, Result};

/// How the encoder may use reference frames, in protocol terms.
///
/// window-stream never retransmits a frame. The client reports the last
/// frame it decoded (`FrameAck`); the host must not let a new frame depend
/// on anything the client has not acknowledged. libvpx cannot express that
/// rule directly (see [`crate::encoder::Vp9Encoder::set_reference_constraint`]
/// and the crate README for the full findings), so the modes below are the
/// honest, implementable subset:
///
/// * [`ReferenceMode::Normal`] — ordinary VP9 inter prediction: last /
///   golden / alt-ref reference frames, periodic keyframes. Fine on a
///   lossless link; a lost delta corrupts everything until the next
///   keyframe.
/// * [`ReferenceMode::AckGated`] — inter prediction stays on, but the
///   long-lived references (golden / alt-ref) are never *updated* by an
///   inter frame — only keyframes refresh them, so an unacknowledged
///   delta cannot contaminate them. Inter frames may still *reference*
///   golden / alt-ref (they hold the last keyframe), and every delta
///   chains on the `last` reference, which advances frame to frame.
///   So this mode does **not** make a delta depend only on acknowledged
///   frames; on any loss the caller must escalate to a forced keyframe
///   before the chain heals. This is the mode the protocol is designed
///   around.
/// * [`ReferenceMode::KeyframesOnly`] — every frame is a keyframe. No inter
///   dependency at all, so loss can never propagate; costs several times
///   the bitrate. A safe fallback and a useful measurement baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReferenceMode {
    /// Ordinary inter prediction with periodic keyframes.
    Normal,
    /// Inter prediction gated on client acknowledgements (default).
    #[default]
    AckGated,
    /// Every frame a keyframe; no reference frames are ever used.
    KeyframesOnly,
}

/// VP9 encoder configuration. Defaults are the realtime profile described in
/// the crate README (WebRTC / RustDesk style): no lag, no B-frame
/// equivalent, error resilience on, speed over compression efficiency.
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    /// Target bitrate in kbit/s.
    pub target_bitrate_kbps: u32,
    /// Nominal frame rate; used for the timebase and the keyframe interval.
    pub fps: u32,
    /// `VP8E_SET_CPUUSED` speed setting, 0 (slowest/best) ..= 9.
    /// Realtime VP9 typically uses 5..=8; the default is 6.
    pub cpu_used: i32,
    /// Threads handed to libvpx (`g_threads`). 0 lets the default apply;
    /// VP9 row-based multithreading is enabled separately when threads > 1.
    pub threads: u32,
    /// Automatic keyframe interval in frames. 0 disables periodic keyframes
    /// (keyframes only on start / explicit request), which is the protocol's
    /// preferred shape: loss recovery is driven by `FrameAck`, not a timer.
    pub keyframe_interval: u32,
    /// Reference-frame policy, see [`ReferenceMode`].
    pub reference_mode: ReferenceMode,
    /// Error-resilient mode (`g_error_resilient`): partitions the bitstream
    /// so some decode errors do not propagate across frames. On by default.
    pub error_resilient: bool,
    /// Frame-drop threshold percent (`rc_dropframe_thresh`): under extreme
    /// bitrate pressure libvpx may drop frames rather than blow the budget.
    /// 0 (default) disables dropping — window-stream drops stale frames at
    /// its own queue (depth 1), not inside the encoder.
    pub dropframe_thresh: u32,
    /// Ack-stall escalation threshold, in frames (AckGated mode only).
    /// If the newest encoded frame's seq is more than this many frames
    /// past the last acknowledged seq, `encode()` forces a keyframe:
    /// acks that stall that long mean the client's reference chain is
    /// presumed broken. The escalation then re-arms only after another
    /// full threshold interval, so a permanently stalled ack yields one
    /// keyframe per interval, not one per frame. 0 disables the
    /// escalation. Default: 30 (see
    /// [`DEFAULT_ACK_STALL_THRESHOLD_FRAMES`]).
    pub ack_stall_threshold_frames: u32,
}

/// Default [`EncoderConfig::ack_stall_threshold_frames`]: force a
/// keyframe once acks lag the encoder by more than 30 frames (~1 s at
/// 30 fps). A safety net for ack stall, not a substitute for calling
/// `invalidate_reference()` promptly when a loss is known.
pub const DEFAULT_ACK_STALL_THRESHOLD_FRAMES: u32 = 30;

impl EncoderConfig {
    /// Realtime defaults for `width`x`height` at `target_bitrate_kbps`.
    pub fn realtime(width: u32, height: u32, target_bitrate_kbps: u32) -> Self {
        Self {
            width,
            height,
            target_bitrate_kbps,
            fps: 30,
            cpu_used: 6,
            threads: 0,
            keyframe_interval: 0,
            reference_mode: ReferenceMode::default(),
            error_resilient: true,
            dropframe_thresh: 0,
            ack_stall_threshold_frames: DEFAULT_ACK_STALL_THRESHOLD_FRAMES,
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.width == 0 || self.height == 0 || self.width % 2 != 0 || self.height % 2 != 0 {
            return Err(CodecError::InvalidConfig(format!(
                "dimensions must be non-zero and even, got {}x{}",
                self.width, self.height
            )));
        }
        if self.target_bitrate_kbps == 0 {
            return Err(CodecError::InvalidConfig("bitrate must be > 0".into()));
        }
        if self.fps == 0 {
            return Err(CodecError::InvalidConfig("fps must be > 0".into()));
        }
        if !(0..=9).contains(&self.cpu_used) {
            return Err(CodecError::InvalidConfig(format!(
                "cpu_used must be in 0..=9, got {}",
                self.cpu_used
            )));
        }
        Ok(())
    }
}

/// VP9 decoder configuration. Deliberately minimal in v1.
#[derive(Debug, Clone, Default)]
pub struct DecoderConfig {
    /// Threads handed to libvpx (`g_threads`). 0 = libvpx default.
    pub threads: u32,
}
