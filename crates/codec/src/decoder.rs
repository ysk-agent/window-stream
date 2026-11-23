//! libvpx VP9 decoder.
//!
//! Note for the Android client: there, hardware VP9 decode goes through
//! MediaCodec, not libvpx (see `docs/IMPLEMENTATION.md`). This decoder is
//! the desktop / reference implementation of the same trait.

use std::marker::PhantomData;

use crate::config::DecoderConfig;
use crate::encoder::check;
use crate::error::{CodecError, Result};
use crate::frame::{EncodedFrame, I420Frame};
use crate::sys;
use crate::VideoDecoder;

/// VP9 decoder backed by libvpx. One instance per stream (window).
pub struct Vp9Decoder {
    ctx: Box<sys::vpx_codec_ctx_t>,
    config: DecoderConfig,
    frames_decoded: u64,
    _not_sync: PhantomData<*mut ()>,
}

impl Vp9Decoder {
    /// Create and initialise a decoder.
    pub fn new(config: DecoderConfig) -> Result<Self> {
        unsafe {
            let iface = sys::vpx_codec_vp9_dx();
            if iface.is_null() {
                return Err(CodecError::Vpx {
                    code: -1,
                    message: "vpx_codec_vp9_dx() returned null".into(),
                });
            }
            let mut cfg: sys::vpx_codec_dec_cfg_t = std::mem::zeroed();
            cfg.threads = config.threads;
            // cfg.w / cfg.h stay 0: dimensions come from the bitstream.
            let mut ctx = Box::new(std::mem::zeroed::<sys::vpx_codec_ctx_t>());
            check(
                sys::vpx_codec_dec_init_ver(
                    &mut *ctx,
                    iface,
                    &cfg,
                    0,
                    sys::VPX_DECODER_ABI_VERSION as std::os::raw::c_int,
                ),
                Some(&ctx),
            )?;
            Ok(Self {
                ctx,
                config,
                frames_decoded: 0,
                _not_sync: PhantomData,
            })
        }
    }

    pub fn config(&self) -> &DecoderConfig {
        &self.config
    }

    /// Total frames successfully decoded by this instance.
    pub fn frames_decoded(&self) -> u64 {
        self.frames_decoded
    }
}

impl VideoDecoder for Vp9Decoder {
    fn decode(&mut self, packet: &EncodedFrame) -> Result<Vec<I420Frame>> {
        if packet.data.is_empty() {
            return Err(CodecError::Decode("empty packet".into()));
        }
        unsafe {
            check(
                sys::vpx_codec_decode(
                    &mut *self.ctx,
                    packet.data.as_ptr(),
                    packet.data.len() as u32,
                    std::ptr::null_mut(),
                    0,
                ),
                Some(&self.ctx),
            )
            .map_err(|e| match e {
                CodecError::Vpx { code, message } => {
                    CodecError::Decode(format!("libvpx ({code}): {message}"))
                }
                other => other,
            })?;

            let mut out = Vec::new();
            let mut iter: sys::vpx_codec_iter_t = std::ptr::null();
            loop {
                let img = sys::vpx_codec_get_frame(&mut *self.ctx, &mut iter);
                if img.is_null() {
                    break;
                }
                let img = &*img;
                if img.d_w == 0 || img.d_h == 0 {
                    return Err(CodecError::Decode(
                        "decoded frame has a zero dimension".into(),
                    ));
                }
                if img.fmt != sys::vpx_img_fmt_VPX_IMG_FMT_I420 {
                    return Err(CodecError::Decode(format!(
                        "expected I420 output, got fmt {}",
                        img.fmt
                    )));
                }
                // The packet's seq belongs to the first frame it yields;
                // extra frames (not produced by our realtime streams)
                // continue the sequence.
                let seq = packet.seq + out.len() as u64;
                let frame = I420Frame::from_strided(
                    img.d_w,
                    img.d_h,
                    seq,
                    std::slice::from_raw_parts(
                        img.planes[0],
                        (img.stride[0] as usize) * (img.d_h as usize - 1) + img.d_w as usize,
                    ),
                    img.stride[0] as usize,
                    std::slice::from_raw_parts(
                        img.planes[1],
                        (img.stride[1] as usize) * (img.d_h as usize / 2 - 1)
                            + img.d_w as usize / 2,
                    ),
                    img.stride[1] as usize,
                    std::slice::from_raw_parts(
                        img.planes[2],
                        (img.stride[2] as usize) * (img.d_h as usize / 2 - 1)
                            + img.d_w as usize / 2,
                    ),
                    img.stride[2] as usize,
                )?;
                self.frames_decoded += 1;
                out.push(frame);
            }
            Ok(out)
        }
    }
}

impl Drop for Vp9Decoder {
    fn drop(&mut self) {
        unsafe {
            sys::vpx_codec_destroy(&mut *self.ctx);
        }
    }
}
