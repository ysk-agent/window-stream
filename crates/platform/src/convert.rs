//! Pure-Rust pixel conversion used by backends and tests.
//!
//! ScreenCaptureKit is asked for BGRA (its cheapest layout); the
//! protocol's VP9 encoder wants I420. This conversion is BT.601
//! limited-range, integer arithmetic, no SIMD -- correctness first,
//! it is not on the latency-critical path in v1 (a backend may also
//! ask the OS for I420 directly, in which case this is unused).

use crate::types::{CapturedFrame, PixelFormat};

fn clamp(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Convert one BGRA frame to I420.
///
/// `src_stride` is the BGRA row stride in bytes (may include padding).
/// Chroma is sampled from the top-left pixel of each 2x2 block
/// (nearest, not averaged) -- acceptable for a v1 reference conversion
/// and documented so nobody mistakes it for a quality-tuned filter.
/// Odd widths/heights are handled by clamping the last sample.
pub fn bgra_to_i420(
    src: &[u8],
    src_stride: usize,
    width: u32,
    height: u32,
) -> (Vec<u8>, Vec<usize>) {
    let w = width as usize;
    let h = height as usize;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let y_stride = w;
    let c_stride = cw;
    let mut out = vec![0u8; y_stride * h + 2 * c_stride * ch];

    let (y_plane, rest) = out.split_at_mut(y_stride * h);
    let (u_plane, v_plane) = rest.split_at_mut(c_stride * ch);

    for row in 0..h {
        let src_row = &src[row * src_stride..row * src_stride + w * 4];
        for col in 0..w {
            let px = &src_row[col * 4..col * 4 + 4];
            let (b, g, r) = (px[0] as i32, px[1] as i32, px[2] as i32);
            // BT.601 limited range.
            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            y_plane[row * y_stride + col] = clamp(y);
            if row % 2 == 0 && col % 2 == 0 {
                let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                let ci = (row / 2) * c_stride + col / 2;
                u_plane[ci] = clamp(u);
                v_plane[ci] = clamp(v);
            }
        }
    }
    (out, vec![y_stride, c_stride, c_stride])
}

/// Convert a whole [`CapturedFrame`]. BGRA frames are converted;
/// I420 frames are returned unchanged.
pub fn frame_to_i420(frame: &CapturedFrame) -> CapturedFrame {
    match frame.format {
        PixelFormat::I420 => frame.clone(),
        PixelFormat::Bgra8888 => {
            let stride = frame
                .strides
                .first()
                .copied()
                .unwrap_or(frame.width as usize * 4);
            let (data, strides) = bgra_to_i420(&frame.data, stride, frame.width, frame.height);
            CapturedFrame {
                format: PixelFormat::I420,
                strides,
                data,
                ..frame.clone()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn black_is_limited_range() {
        // 2x2 black BGRA -> Y=16, U=V=128.
        let src = [0u8; 16];
        let (out, strides) = bgra_to_i420(&src, 8, 2, 2);
        assert_eq!(strides, vec![2, 1, 1]);
        assert_eq!(&out[0..4], &[16, 16, 16, 16]);
        assert_eq!(out[4], 128);
        assert_eq!(out[5], 128);
    }

    #[test]
    fn white_is_limited_range() {
        let mut src = [0u8; 16];
        for px in src.chunks_exact_mut(4) {
            px[0] = 255;
            px[1] = 255;
            px[2] = 255;
            px[3] = 255;
        }
        let (out, _) = bgra_to_i420(&src, 8, 2, 2);
        assert_eq!(&out[0..4], &[235, 235, 235, 235]);
        assert_eq!(out[4], 128);
    }

    #[test]
    fn padded_stride_is_respected() {
        // width 1, stride 8 (one padding pixel), height 1, red pixel.
        let src = [0, 0, 255, 255, 9, 9, 9, 9];
        let (out, _) = bgra_to_i420(&src, 8, 1, 1);
        // Pure red BT.601 limited: Y=82, U=90, V=240.
        assert_eq!(out[0], 82);
        assert_eq!(out[1], 90);
        assert_eq!(out[2], 240);
    }
}
