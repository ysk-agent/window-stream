//! Frame types: I420 input/output frames and encoded VP9 packets.

use crate::error::{CodecError, Result};

/// An 8-bit I420 (YUV 4:2:0 planar) frame.
///
/// This is the only pixel format the codec track accepts: VP9 is fixed to
/// 8-bit 4:2:0 by the window-stream design (see the repository README).
/// Width and height must both be even, as the chroma planes are subsampled
/// by two in each dimension.
///
/// Planes are stored tightly packed (stride == row width) in the owned
/// representation; [`I420Frame::from_strided`] copies strided source data
/// (e.g. a capture buffer or a libvpx image) into this layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct I420Frame {
    width: u32,
    height: u32,
    /// Presentation timestamp / sequence number, in frame units.
    /// The protocol layer maps this to the `Frame` message sequence number.
    seq: u64,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl I420Frame {
    /// Create a frame from tightly packed planes.
    ///
    /// # Errors
    /// [`CodecError::InvalidFrame`] if a dimension is zero/odd or a plane has
    /// the wrong length.
    pub fn new(
        width: u32,
        height: u32,
        seq: u64,
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
    ) -> Result<Self> {
        check_dims(width, height)?;
        let (yw, cw, ch) = plane_sizes(width, height);
        if y.len() != yw || u.len() != cw * ch || v.len() != cw * ch {
            return Err(CodecError::InvalidFrame(format!(
                "plane sizes for {width}x{height} must be y={yw}, u=v={} (got y={}, u={}, v={})",
                cw * ch,
                y.len(),
                u.len(),
                v.len()
            )));
        }
        Ok(Self {
            width,
            height,
            seq,
            y,
            u,
            v,
        })
    }

    /// Create a zero-filled frame.
    pub fn blank(width: u32, height: u32, seq: u64) -> Result<Self> {
        check_dims(width, height)?;
        let (yw, cw, ch) = plane_sizes(width, height);
        Ok(Self {
            width,
            height,
            seq,
            y: vec![0; yw],
            u: vec![0; cw * ch],
            v: vec![0; cw * ch],
        })
    }

    /// Copy a frame out of strided planes (stride >= row width, in bytes).
    ///
    /// # Errors
    /// [`CodecError::InvalidFrame`] on bad dimensions or short buffers.
    #[allow(clippy::too_many_arguments)]
    pub fn from_strided(
        width: u32,
        height: u32,
        seq: u64,
        y: &[u8],
        y_stride: usize,
        u: &[u8],
        u_stride: usize,
        v: &[u8],
        v_stride: usize,
    ) -> Result<Self> {
        check_dims(width, height)?;
        let copy = |src: &[u8], stride: usize, w: usize, h: usize, name: &str| -> Result<Vec<u8>> {
            if stride < w {
                return Err(CodecError::InvalidFrame(format!(
                    "{name} stride {stride} < row width {w}"
                )));
            }
            let need = stride
                .checked_mul(h - 1)
                .and_then(|n| n.checked_add(w))
                .ok_or_else(|| CodecError::InvalidFrame("plane size overflow".into()))?;
            if src.len() < need {
                return Err(CodecError::InvalidFrame(format!(
                    "{name} plane too short: need {need} bytes, got {}",
                    src.len()
                )));
            }
            let mut out = Vec::with_capacity(w * h);
            for row in 0..h {
                out.extend_from_slice(&src[row * stride..row * stride + w]);
            }
            Ok(out)
        };
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = (w / 2, h / 2);
        Ok(Self {
            width,
            height,
            seq,
            y: copy(y, y_stride, w, h, "y")?,
            u: copy(u, u_stride, cw, ch, "u")?,
            v: copy(v, v_stride, cw, ch, "v")?,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    /// Sequence number / presentation timestamp in frame units.
    pub fn seq(&self) -> u64 {
        self.seq
    }
    pub fn set_seq(&mut self, seq: u64) {
        self.seq = seq;
    }
    /// Tightly packed luma plane, `width * height` bytes.
    pub fn y(&self) -> &[u8] {
        &self.y
    }
    /// Tightly packed Cb plane, `(width/2) * (height/2)` bytes.
    pub fn u(&self) -> &[u8] {
        &self.u
    }
    /// Tightly packed Cr plane, `(width/2) * (height/2)` bytes.
    pub fn v(&self) -> &[u8] {
        &self.v
    }
    pub fn y_mut(&mut self) -> &mut [u8] {
        &mut self.y
    }
    pub fn u_mut(&mut self) -> &mut [u8] {
        &mut self.u
    }
    pub fn v_mut(&mut self) -> &mut [u8] {
        &mut self.v
    }
}

/// One encoded VP9 packet (one frame worth of bitstream, as produced by a
/// single `vpx_codec_encode` drain for one input frame in realtime mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// Raw VP9 bitstream for this frame (one IVF frame / one protocol
    /// `Frame` message payload).
    pub data: Vec<u8>,
    /// True if this is a keyframe (independently decodable).
    pub is_keyframe: bool,
    /// Sequence number copied from the input [`I420Frame::seq`].
    pub seq: u64,
}

fn check_dims(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(CodecError::InvalidFrame(format!(
            "zero dimension {width}x{height}"
        )));
    }
    if width % 2 != 0 || height % 2 != 0 {
        return Err(CodecError::InvalidFrame(format!(
            "I420 requires even dimensions, got {width}x{height}"
        )));
    }
    Ok(())
}

/// (luma bytes, chroma width, chroma height)
fn plane_sizes(width: u32, height: u32) -> (usize, usize, usize) {
    (
        width as usize * height as usize,
        width as usize / 2,
        height as usize / 2,
    )
}

/// Mean luma PSNR in dB between two same-sized frames.
/// Returns `f64::INFINITY` for identical luma planes, and `f64::NAN`
/// if the frames have different dimensions (no panic: this is a public
/// API and a size mismatch is a caller error, not a logic error).
pub fn luma_psnr(a: &I420Frame, b: &I420Frame) -> f64 {
    if (a.width, a.height) != (b.width, b.height) {
        return f64::NAN;
    }
    let mut sse: u64 = 0;
    for (x, y) in a.y.iter().zip(b.y.iter()) {
        let d = i64::from(*x) - i64::from(*y);
        sse += (d * d) as u64;
    }
    if sse == 0 {
        return f64::INFINITY;
    }
    let mse = sse as f64 / a.y.len() as f64;
    10.0 * (255.0f64 * 255.0 / mse).log10()
}
