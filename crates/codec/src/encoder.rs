//! libvpx VP9 encoder, realtime configuration.
//!
//! Settings (mirroring WebRTC's and RustDesk's realtime VP9 usage):
//! * `g_lag_in_frames = 0`, deadline `VPX_DL_REALTIME` — no frame is held
//!   back, so there is no B-frame equivalent and no encoder-side latency.
//! * Rate control: CBR (`VPX_CBR`). For a realtime stream the per-frame
//!   byte budget must be predictable (radio wake time is per packet, and
//!   the protocol drops rather than queues); VBR's quality smoothing buys
//!   nothing when frames may not wait.
//! * `VP8E_SET_CPUUSED` from config (default 6), ARNR off (it filters
//!   against *future* frames, which do not exist yet in realtime),
//!   auto-alt-ref off (an invisible future-reference frame is a B-frame
//!   equivalent), error resilience on.
//! * Keyframes: periodic keyframes are disabled by default
//!   (`keyframe_interval = 0`); keyframes happen on start, on
//!   [`VideoEncoder::force_keyframe`] and on reference invalidation —
//!   recovery is driven by the client's `FrameAck`, not a timer.

use std::ffi::CStr;
use std::marker::PhantomData;
use std::os::raw::{c_int, c_ulong};

use crate::config::{EncoderConfig, ReferenceMode};
use crate::error::{CodecError, Result};
use crate::frame::{EncodedFrame, I420Frame};
use crate::sys;
use crate::VideoEncoder;

/// VP9 encoder backed by libvpx. One instance per stream (window).
///
/// # Reference-frame control: what libvpx can and cannot do
///
/// The protocol rule is "a frame may only reference frames the client has
/// acknowledged". libvpx has **no API that expresses that rule directly**:
///
/// * There is no per-reference "sequence number" or "only use the buffer
///   holding frame N" knob in the one-pass realtime path. Reference
///   management is by buffer *slot* (last / golden / alt-ref), not by the
///   caller's frame numbering.
/// * `VP9E_SET_SVC_REF_FRAME_CONFIG` comes closest — it picks buffer
///   indices and per-reference update/reference switches per frame — but
///   it is only honoured in SVC mode (`VP9E_SET_SVC`), i.e. it requires
///   buying into libvpx's spatial/temporal layer machinery, and it still
///   speaks in slot indices the caller must track against libvpx's own
///   buffer assignment. It is exposed here as
///   [`Vp9Encoder::set_svc_ref_frame_config_raw`] for experiments, and is
///   deliberately not the default path.
/// * The generic encode flags `VP8_EFLAG_NO_REF_LAST/GF/ARF` and
///   `VP8_EFLAG_NO_UPD_LAST/GF/ARF` (declared in `<vpx/vp8cx.h>`, shared
///   by the VP9 encoder) do work per frame, and
///   `VPX_EFLAG_FORCE_KF` forces a keyframe.
///
/// So [`ReferenceMode::AckGated`] is implemented as the composition that
/// *is* expressible, and it is important to be precise about it:
///
/// 1. Golden / alt-ref are **never updated by inter frames**
///    (`NO_UPD_GF | NO_UPD_ARF` on every inter frame). The only frames
///    that refresh the long-term references are keyframes.
/// 2. The `last` reference necessarily advances frame to frame — VP9
///    prediction needs a previous frame and libvpx offers no way to pin
///    it to "the last acked frame" while still emitting a frame per
///    input. A lost delta therefore still breaks the chain, exactly as
///    in any VP9 stream.
/// 3. Recovery is by escalation, not repair: when acknowledgements stall
///    or the caller learns a frame was lost,
///    [`VideoEncoder::invalidate_reference`] forces the next frame to be
///    a keyframe, and `encode()` itself forces a keyframe once acks lag
///    more than `EncoderConfig::ack_stall_threshold_frames` behind the
///    frame being encoded (AckGated only). A lost keyframe is replaced
///    by a *new* keyframe, never retransmitted — matching the protocol's
///    loss policy.
///
/// In other words: AckGated bounds how much state an unacknowledged frame
/// can contaminate (long-term references stay keyframe-clean), and makes
/// recovery one forced keyframe away. It does not — and with libvpx
/// cannot — make every delta depend only on acknowledged frames.
pub struct Vp9Encoder {
    ctx: Box<sys::vpx_codec_ctx_t>,
    img: Box<sys::vpx_image_t>,
    config: EncoderConfig,
    force_next_keyframe: bool,
    last_acked_seq: Option<u64>,
    frames_encoded: u64,
    /// Seq of the most recently encoded frame; `flush()` uses
    /// `last_encoded_seq + 1` as its pts so it can never collide with a
    /// real frame's pts (frame seqs need not start at 0 or be dense).
    last_encoded_seq: u64,
    /// Seq of the last keyframe forced by ack-stall escalation. The
    /// escalation re-arms only after another full threshold has passed,
    /// so a permanently stalled ack yields a periodic keyframe (one per
    /// threshold interval) instead of turning every frame into one.
    last_stall_keyframe_seq: Option<u64>,
    keyframes_encoded: u64,
    invalidations: u64,
    /// Non-fatal libvpx control calls that failed during setup, kept for
    /// diagnostics (e.g. row-MT rejected at a tiny resolution).
    pub control_warnings: Vec<String>,
    _not_sync: PhantomData<*mut ()>,
}

/// Apply the keyframe fields of an [`EncoderConfig`] to a raw libvpx
/// config. Shared by `new()` and `set_bitrate_kbps()` so a runtime
/// bitrate change cannot silently drop the keyframe schedule.
fn apply_keyframe_settings(raw: &mut sys::vpx_codec_enc_cfg_t, config: &EncoderConfig) {
    if config.keyframe_interval > 0 {
        raw.kf_mode = sys::vpx_kf_mode_VPX_KF_AUTO;
        raw.kf_min_dist = 0;
        raw.kf_max_dist = config.keyframe_interval;
    } else {
        raw.kf_mode = sys::vpx_kf_mode_VPX_KF_DISABLED;
        raw.kf_min_dist = 0;
        raw.kf_max_dist = 0;
    }
    if config.reference_mode == ReferenceMode::KeyframesOnly {
        raw.kf_mode = sys::vpx_kf_mode_VPX_KF_AUTO;
        raw.kf_min_dist = 0;
        raw.kf_max_dist = 1;
    }
}

impl Vp9Encoder {
    /// Create and initialise an encoder.
    pub fn new(config: EncoderConfig) -> Result<Self> {
        config.validate()?;
        unsafe {
            let iface = sys::vpx_codec_vp9_cx();
            if iface.is_null() {
                return Err(CodecError::Vpx {
                    code: -1,
                    message: "vpx_codec_vp9_cx() returned null".into(),
                });
            }
            let mut cfg: sys::vpx_codec_enc_cfg_t = std::mem::zeroed();
            check(sys::vpx_codec_enc_config_default(iface, &mut cfg, 0), None)?;

            cfg.g_w = config.width;
            cfg.g_h = config.height;
            cfg.g_timebase.num = 1;
            cfg.g_timebase.den = config.fps as c_int;
            cfg.g_threads = config.threads;
            cfg.g_lag_in_frames = 0;
            cfg.g_pass = sys::vpx_enc_pass_VPX_RC_ONE_PASS;
            cfg.g_error_resilient = if config.error_resilient {
                sys::VPX_ERROR_RESILIENT_DEFAULT
            } else {
                0
            };
            cfg.rc_end_usage = sys::vpx_rc_mode_VPX_CBR;
            cfg.rc_target_bitrate = config.target_bitrate_kbps;
            cfg.rc_dropframe_thresh = config.dropframe_thresh;
            apply_keyframe_settings(&mut cfg, &config);

            let mut ctx = Box::new(std::mem::zeroed::<sys::vpx_codec_ctx_t>());
            check(
                sys::vpx_codec_enc_init_ver(
                    &mut *ctx,
                    iface,
                    &cfg,
                    0,
                    sys::VPX_ENCODER_ABI_VERSION as c_int,
                ),
                Some(&ctx),
            )?;

            let mut enc = Self {
                ctx,
                img: Box::new(std::mem::zeroed()),
                config: config.clone(),
                force_next_keyframe: false,
                last_acked_seq: None,
                frames_encoded: 0,
                last_encoded_seq: 0,
                last_stall_keyframe_seq: None,
                keyframes_encoded: 0,
                invalidations: 0,
                control_warnings: Vec::new(),
                _not_sync: PhantomData,
            };

            // Realtime controls. Failures here degrade speed/quality, not
            // correctness, so they are recorded as warnings.
            enc.control_warn(sys::VP8E_SET_CPUUSED, config.cpu_used, "VP8E_SET_CPUUSED");
            enc.control_warn(
                sys::VP8E_SET_ENABLEAUTOALTREF,
                0,
                "VP8E_SET_ENABLEAUTOALTREF",
            );
            // ARNR filters the alt-ref against future frames: off.
            enc.control_warn(sys::VP8E_SET_ARNR_MAXFRAMES, 0, "VP8E_SET_ARNR_MAXFRAMES");
            enc.control_warn(sys::VP8E_SET_ARNR_STRENGTH, 0, "VP8E_SET_ARNR_STRENGTH");
            if config.threads != 1 {
                enc.control_warn(sys::VP9E_SET_ROW_MT, 1, "VP9E_SET_ROW_MT");
            }

            // Scratch image, allocated once and refilled per frame.
            let img_ptr = sys::vpx_img_alloc(
                &mut *enc.img,
                sys::vpx_img_fmt_VPX_IMG_FMT_I420,
                config.width,
                config.height,
                1,
            );
            if img_ptr.is_null() {
                return Err(CodecError::Vpx {
                    code: -1,
                    message: "vpx_img_alloc failed".into(),
                });
            }
            Ok(enc)
        }
    }

    /// The configuration this encoder was created with.
    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }

    /// Sequence number of the last frame the client acknowledged, as last
    /// set by [`VideoEncoder::set_reference_constraint`].
    pub fn last_acked_seq(&self) -> Option<u64> {
        self.last_acked_seq
    }

    /// (frames encoded, keyframes encoded, reference invalidations)
    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.frames_encoded,
            self.keyframes_encoded,
            self.invalidations,
        )
    }

    /// Change the target bitrate at runtime (`vpx_codec_enc_config_set`).
    pub fn set_bitrate_kbps(&mut self, kbps: u32) -> Result<()> {
        if kbps == 0 {
            return Err(CodecError::InvalidConfig("bitrate must be > 0".into()));
        }
        unsafe {
            let iface = sys::vpx_codec_vp9_cx();
            // libvpx has no config getter, so rebuild from stored config.
            let mut new_cfg = self.config.clone();
            new_cfg.target_bitrate_kbps = kbps;
            let mut raw: sys::vpx_codec_enc_cfg_t = std::mem::zeroed();
            check(sys::vpx_codec_enc_config_default(iface, &mut raw, 0), None)?;
            raw.g_w = new_cfg.width;
            raw.g_h = new_cfg.height;
            raw.g_timebase.num = 1;
            raw.g_timebase.den = new_cfg.fps as c_int;
            raw.g_threads = new_cfg.threads;
            raw.g_lag_in_frames = 0;
            raw.g_pass = sys::vpx_enc_pass_VPX_RC_ONE_PASS;
            raw.g_error_resilient = if new_cfg.error_resilient {
                sys::VPX_ERROR_RESILIENT_DEFAULT
            } else {
                0
            };
            raw.rc_end_usage = sys::vpx_rc_mode_VPX_CBR;
            raw.rc_target_bitrate = kbps;
            raw.rc_dropframe_thresh = new_cfg.dropframe_thresh;
            // Keyframe settings are part of the saved config, not of the
            // library defaults: re-apply them exactly as `new()` does, or
            // a bitrate change would silently drop periodic keyframes
            // (and KeyframesOnly mode).
            apply_keyframe_settings(&mut raw, &new_cfg);
            check(
                sys::vpx_codec_enc_config_set(&mut *self.ctx, &raw),
                Some(&self.ctx),
            )?;
            self.config = new_cfg;
            Ok(())
        }
    }

    /// Experimental escape hatch: forward a raw
    /// `VP9E_SET_SVC_REF_FRAME_CONFIG` to libvpx.
    ///
    /// Only honoured by libvpx in SVC mode; outside it the call fails and
    /// the error is returned. Kept so the SVC route can be evaluated
    /// without another FFI rewrite — see the type-level documentation.
    pub fn set_svc_ref_frame_config_raw(
        &mut self,
        cfg: &sys::vpx_svc_ref_frame_config_t,
    ) -> Result<()> {
        unsafe {
            check(
                sys::vpx_codec_control_(
                    &mut *self.ctx,
                    sys::VP9E_SET_SVC_REF_FRAME_CONFIG,
                    cfg as *const sys::vpx_svc_ref_frame_config_t,
                ),
                Some(&self.ctx),
            )
        }
    }

    fn control_warn(&mut self, id: i32, value: i32, name: &str) {
        unsafe {
            let res = sys::vpx_codec_control_(&mut *self.ctx, id, value);
            if res != sys::vpx_codec_err_t_VPX_CODEC_OK {
                self.control_warnings
                    .push(format!("{name}={value}: {}", err_string(&self.ctx, res)));
            }
        }
    }

    fn encode_flags(&self) -> sys::vpx_enc_frame_flags_t {
        let mut flags: sys::vpx_enc_frame_flags_t = 0;
        if self.force_next_keyframe || self.config.reference_mode == ReferenceMode::KeyframesOnly {
            flags |= sys::VPX_EFLAG_FORCE_KF as sys::vpx_enc_frame_flags_t;
        } else if self.config.reference_mode == ReferenceMode::AckGated {
            // Inter frames must not refresh the long-term references:
            // golden / alt-ref stay exactly as the last keyframe left
            // them until the next keyframe (see type documentation).
            flags |= sys::VP8_EFLAG_NO_UPD_GF as sys::vpx_enc_frame_flags_t;
            flags |= sys::VP8_EFLAG_NO_UPD_ARF as sys::vpx_enc_frame_flags_t;
        }
        flags
    }

    fn copy_frame_into_img(&mut self, frame: &I420Frame) {
        unsafe {
            let planes: [&[u8]; 3] = [frame.y(), frame.u(), frame.v()];
            let dims: [(usize, usize); 3] = [
                (frame.width() as usize, frame.height() as usize),
                (frame.width() as usize / 2, frame.height() as usize / 2),
                (frame.width() as usize / 2, frame.height() as usize / 2),
            ];
            for p in 0..3 {
                let dst = self.img.planes[p];
                let stride = self.img.stride[p] as usize;
                let (w, h) = dims[p];
                let src = planes[p];
                for row in 0..h {
                    std::ptr::copy_nonoverlapping(
                        src.as_ptr().add(row * w),
                        dst.add(row * stride),
                        w,
                    );
                }
            }
        }
    }

    fn drain(&mut self, fallback_seq: u64) -> Result<Vec<EncodedFrame>> {
        let mut out: Vec<EncodedFrame> = Vec::new();
        unsafe {
            let mut iter: sys::vpx_codec_iter_t = std::ptr::null();
            loop {
                let pkt = sys::vpx_codec_get_cx_data(&mut *self.ctx, &mut iter);
                if pkt.is_null() {
                    break;
                }
                let pkt = &*pkt;
                if pkt.kind != sys::vpx_codec_cx_pkt_kind_VPX_CODEC_CX_FRAME_PKT {
                    continue;
                }
                let fp = &pkt.data.frame;
                let data = std::slice::from_raw_parts(fp.buf as *const u8, fp.sz).to_vec();
                let is_keyframe = fp.flags & sys::VPX_FRAME_IS_KEY != 0;
                let seq = if fp.pts >= 0 {
                    fp.pts as u64
                } else {
                    fallback_seq
                };
                if is_keyframe {
                    self.keyframes_encoded += 1;
                }
                // Packets belonging to one input frame (a visible frame
                // plus an invisible alt-ref companion) are concatenated:
                // that byte string is exactly the frame's superframe, and
                // the realtime config disables alt-ref, so in practice
                // this loop yields one packet per frame.
                match out.last_mut() {
                    Some(prev) if prev.seq == seq => {
                        let prev: &mut EncodedFrame = prev;
                        prev.data.extend_from_slice(&data);
                        prev.is_keyframe = prev.is_keyframe || is_keyframe;
                    }
                    _ => out.push(EncodedFrame {
                        data,
                        is_keyframe,
                        seq,
                    }),
                }
            }
        }
        Ok(out)
    }
}

impl VideoEncoder for Vp9Encoder {
    fn encode(&mut self, frame: &I420Frame) -> Result<Vec<EncodedFrame>> {
        if frame.width() != self.config.width || frame.height() != self.config.height {
            return Err(CodecError::InvalidFrame(format!(
                "encoder is {}x{}, got {}x{}",
                self.config.width,
                self.config.height,
                frame.width(),
                frame.height()
            )));
        }
        self.copy_frame_into_img(frame);
        // Ack-stall escalation (AckGated only): if the client's acks have
        // stalled more than `ack_stall_threshold_frames` behind the frame
        // being encoded, the client's reference chain is presumed broken
        // and this frame is forced to be a keyframe. This is a safety
        // net for unnoticed loss; known loss must still be escalated
        // promptly via `invalidate_reference()` (see the trait docs).
        // `last_acked_seq == None` (no ack yet) never escalates: the
        // first frames are keyframed by libvpx anyway. The escalation
        // re-arms only after another full threshold interval, so a
        // permanently stalled ack yields one keyframe per interval
        // instead of turning every frame into a keyframe.
        if self.config.reference_mode == ReferenceMode::AckGated {
            if let Some(acked) = self.last_acked_seq {
                let threshold = self.config.ack_stall_threshold_frames as u64;
                if threshold > 0 && frame.seq().saturating_sub(acked) > threshold {
                    let rearmed = match self.last_stall_keyframe_seq {
                        None => true,
                        Some(kf_seq) => frame.seq().saturating_sub(kf_seq) > threshold,
                    };
                    if rearmed {
                        self.force_next_keyframe = true;
                        self.last_stall_keyframe_seq = Some(frame.seq());
                    }
                }
            }
        }
        let flags = self.encode_flags();
        unsafe {
            check(
                sys::vpx_codec_encode(
                    &mut *self.ctx,
                    &*self.img,
                    frame.seq() as sys::vpx_codec_pts_t,
                    1,
                    flags,
                    sys::VPX_DL_REALTIME as c_ulong,
                ),
                Some(&self.ctx),
            )?;
        }
        self.force_next_keyframe = false;
        self.frames_encoded += 1;
        self.last_encoded_seq = frame.seq();
        self.drain(frame.seq())
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        unsafe {
            check(
                sys::vpx_codec_encode(
                    &mut *self.ctx,
                    std::ptr::null(),
                    self.last_encoded_seq.saturating_add(1) as sys::vpx_codec_pts_t,
                    1,
                    0,
                    sys::VPX_DL_REALTIME as c_ulong,
                ),
                Some(&self.ctx),
            )?;
        }
        self.drain(self.last_encoded_seq.saturating_add(1))
    }

    fn force_keyframe(&mut self) {
        self.force_next_keyframe = true;
    }

    fn set_reference_constraint(&mut self, acked_seq: Option<u64>) {
        self.last_acked_seq = acked_seq;
    }

    fn invalidate_reference(&mut self) {
        self.invalidations += 1;
        self.force_next_keyframe = true;
    }
}

impl Drop for Vp9Encoder {
    fn drop(&mut self) {
        unsafe {
            sys::vpx_img_free(&mut *self.img);
            sys::vpx_codec_destroy(&mut *self.ctx);
        }
    }
}

pub(crate) fn check(res: sys::vpx_codec_err_t, ctx: Option<&sys::vpx_codec_ctx_t>) -> Result<()> {
    if res == sys::vpx_codec_err_t_VPX_CODEC_OK {
        Ok(())
    } else {
        Err(CodecError::Vpx {
            code: res as i32,
            message: match ctx {
                Some(c) => err_string(c, res),
                None => err_string_no_ctx(res),
            },
        })
    }
}

pub(crate) fn err_string(ctx: &sys::vpx_codec_ctx_t, res: sys::vpx_codec_err_t) -> String {
    unsafe {
        let ptr = sys::vpx_codec_error(ctx);
        if !ptr.is_null() {
            let detail = CStr::from_ptr(ptr).to_string_lossy().into_owned();
            if !detail.is_empty() {
                return detail;
            }
        }
        err_string_no_ctx(res)
    }
}

pub(crate) fn err_string_no_ctx(res: sys::vpx_codec_err_t) -> String {
    unsafe {
        let ptr = sys::vpx_codec_err_to_string(res);
        if ptr.is_null() {
            format!("libvpx error {res}")
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}
