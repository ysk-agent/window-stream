//! Locally generated test frames in I420 (YCbCr 4:2:0 planar).
//!
//! This stands in for the transport + VP9 decode path, which are other
//! tracks. It produces *encoded-format* frames (separate Y/U/V planes), not
//! RGB: the I420 -> RGB conversion happens only in the shader, exactly as it
//! will for decoded VP9 frames. Generation here is pattern synthesis, not
//! colour conversion.

pub struct I420Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

/// A deterministic animated pattern: gradient background, a moving solid
/// rectangle (the classic "moving box" test), and a moving diagonal stripe.
/// `seed` shifts hue/phase per window so windows are visually distinct.
pub fn generate(width: u32, height: u32, frame_index: u64, seed: u8) -> I420Frame {
    let (w, h) = (width as usize, height as usize);
    let (cw, ch) = (w / 2, h / 2);
    let mut y = vec![0u8; w * h];
    let mut u = vec![128u8; cw * ch];
    let mut v = vec![128u8; cw * ch];

    let t = frame_index as f32;
    // Moving box, bouncing horizontally.
    let box_w = (w / 4).max(8);
    let box_h = (h / 4).max(8);
    let span = (w - box_w).max(1) as f32;
    let phase = (t * 2.0 / span).sin() * 0.5 + 0.5;
    let box_x = (phase * span) as usize;
    let box_y = (((t * 0.03).sin() * 0.5 + 0.5) * ((h - box_h).max(1) as f32)) as usize;

    for row in 0..h {
        for col in 0..w {
            // Gradient in luma only.
            let mut luma = 40 + ((col * 180 / w.max(1)) as u8);
            if (col + row + frame_index as usize / 2) % 64 < 4 {
                luma = 220; // moving diagonal stripe marker
            }
            if col >= box_x && col < box_x + box_w && row >= box_y && row < box_y + box_h {
                luma = 235;
            }
            y[row * w + col] = luma;
        }
    }
    for row in 0..ch {
        for col in 0..cw {
            let (px, py) = (col * 2, row * 2);
            let in_box = px >= box_x && px < box_x + box_w && py >= box_y && py < box_y + box_h;
            let idx = row * cw + col;
            if in_box {
                // Strong colour for the box (in YUV space, not RGB).
                u[idx] = 90;
                v[idx] = 200;
            } else {
                u[idx] = 100u8.wrapping_add(seed).wrapping_add((col as u8) / 4);
                v[idx] = 160u8.wrapping_sub(seed).wrapping_sub((row as u8) / 4);
            }
        }
    }
    I420Frame { width, height, y, u, v }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plane_sizes() {
        let f = generate(64, 48, 0, 1);
        assert_eq!(f.y.len(), 64 * 48);
        assert_eq!(f.u.len(), 32 * 24);
        assert_eq!(f.v.len(), 32 * 24);
    }
    #[test]
    fn animates() {
        let a = generate(64, 48, 0, 1);
        let b = generate(64, 48, 30, 1);
        assert_ne!(a.y, b.y);
    }
}
