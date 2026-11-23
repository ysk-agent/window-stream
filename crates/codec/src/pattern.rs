//! Synthetic test pattern used by the tests, the bench and (later) the
//! `dummy` host: a moving rectangle over a gradient, in I420.
//!
//! Deterministic and dependency-free, so PSNR numbers are reproducible.

use crate::error::Result;
use crate::frame::I420Frame;

/// Generate frame `index` of the moving-rectangle pattern at `width`x`height`.
///
/// * Background: horizontal luma gradient with a slow vertical drift.
/// * Foreground: a bright rectangle (with a dark border and a saturated
///   chroma tint) moving diagonally and bouncing off the edges — motion is
///   what makes inter prediction (and its loss behaviour) measurable.
pub fn moving_rectangle(width: u32, height: u32, index: u64) -> Result<I420Frame> {
    let mut frame = I420Frame::blank(width, height, index)?;
    let (w, h) = (width as usize, height as usize);

    // Background gradient.
    for row in 0..h {
        for col in 0..w {
            frame.y_mut()[row * w + col] =
                (32 + (col * 96 / w.max(1)) + ((row + index as usize) % 16)) as u8;
        }
    }
    frame.u_mut().fill(96);
    frame.v_mut().fill(160);

    // Rectangle geometry: ~1/4 of the frame, bouncing diagonally.
    let rw = (w / 4).max(2);
    let rh = (h / 4).max(2);
    let speed_x = 7usize;
    let speed_y = 5usize;
    let t = index as usize;
    let bounce = |pos: usize, max: usize| -> usize {
        if max == 0 {
            return 0;
        }
        let period = max * 2;
        let p = pos % period;
        if p <= max {
            p
        } else {
            period - p
        }
    };
    let x0 = bounce(t * speed_x, w - rw);
    let y0 = bounce(t * speed_y, h - rh);

    let (cw, ch) = (w / 2, h / 2);
    for row in y0..y0 + rh {
        for col in x0..x0 + rw {
            let border = col == x0 || col == x0 + rw - 1 || row == y0 || row == y0 + rh - 1;
            frame.y_mut()[row * w + col] = if border { 16 } else { 235 };
        }
    }
    for row in (y0 / 2)..((y0 + rh) / 2).min(ch) {
        for col in (x0 / 2)..((x0 + rw) / 2).min(cw) {
            frame.u_mut()[row * cw + col] = 200;
            frame.v_mut()[row * cw + col] = 80;
        }
    }
    Ok(frame)
}
