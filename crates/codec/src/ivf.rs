//! IVF container read/write — a debug format, not part of the protocol.
//!
//! IVF lets a captured VP9 bitstream be replayed/inspected with standard
//! tools (`ffplay stream.ivf`, `ffmpeg -i stream.ivf ...`, Chromium, ...).
//! Layout (de-facto standard, from the libvpx / WebM project docs):
//!
//! * 32-byte file header: signature `DKIF`, u16 version (0), u16 header
//!   length (32), FourCC (`VP90` here), u16 width, u16 height,
//!   u32 frame rate (timebase denominator), u32 timebase scale (numerator),
//!   u32 frame count (0 = unknown while writing; patched on finish),
//!   u32 unused.
//! * Per frame: u32 payload size, u64 timestamp (in timebase units),
//!   then the raw VP9 payload.
//!
//! All integers are little-endian.

use std::io::{Read, Seek, SeekFrom, Write};

use crate::error::{CodecError, Result};
use crate::frame::EncodedFrame;

/// FourCC for VP9 in IVF.
pub const FOURCC_VP9: &[u8; 4] = b"VP90";
const HEADER_LEN: usize = 32;

/// Streaming IVF writer over any [`Write`] (+ [`Seek`] to patch the frame
/// count on [`IvfWriter::finish`]; use [`IvfWriter::write_header_count`]
/// up-front if the count is known and the sink is not seekable).
pub struct IvfWriter<W: Write> {
    inner: W,
    width: u32,
    height: u32,
    fps: u32,
    frames_written: u32,
}

impl<W: Write> IvfWriter<W> {
    /// Write the 32-byte header with frame count 0 (unknown).
    pub fn new(inner: W, width: u32, height: u32, fps: u32) -> Result<Self> {
        let mut w = Self {
            inner,
            width,
            height,
            fps,
            frames_written: 0,
        };
        w.write_header(0)?;
        Ok(w)
    }

    fn write_header(&mut self, frame_count: u32) -> Result<()> {
        let mut h = [0u8; HEADER_LEN];
        h[0..4].copy_from_slice(b"DKIF");
        h[4..6].copy_from_slice(&0u16.to_le_bytes()); // version
        h[6..8].copy_from_slice(&(HEADER_LEN as u16).to_le_bytes());
        h[8..12].copy_from_slice(FOURCC_VP9);
        h[12..14].copy_from_slice(&(self.width as u16).to_le_bytes());
        h[14..16].copy_from_slice(&(self.height as u16).to_le_bytes());
        // rate / scale: timestamp unit = 1/fps second.
        h[16..20].copy_from_slice(&self.fps.to_le_bytes());
        h[20..24].copy_from_slice(&1u32.to_le_bytes());
        h[24..28].copy_from_slice(&frame_count.to_le_bytes());
        // h[28..32] unused.
        self.inner.write_all(&h)?;
        Ok(())
    }

    /// Append one encoded frame; its `seq` becomes the IVF timestamp.
    pub fn write_frame(&mut self, frame: &EncodedFrame) -> Result<()> {
        let size = u32::try_from(frame.data.len())
            .map_err(|_| CodecError::InvalidIvf("frame larger than u32".into()))?;
        self.inner.write_all(&size.to_le_bytes())?;
        self.inner.write_all(&frame.seq.to_le_bytes())?;
        self.inner.write_all(&frame.data)?;
        self.frames_written += 1;
        Ok(())
    }

    pub fn frames_written(&self) -> u32 {
        self.frames_written
    }

    /// Flush and return the inner writer. Does not patch the frame count;
    /// use [`IvfWriter::finish`] on seekable sinks for that.
    pub fn into_inner(mut self) -> Result<W> {
        self.inner.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write + Seek> IvfWriter<W> {
    /// Patch the frame-count field in the header and finish.
    pub fn finish(mut self) -> Result<W> {
        self.inner.seek(SeekFrom::Start(24))?;
        self.inner.write_all(&self.frames_written.to_le_bytes())?;
        self.inner.seek(SeekFrom::End(0))?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

/// Parsed IVF file header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IvfHeader {
    pub fourcc: [u8; 4],
    pub width: u32,
    pub height: u32,
    /// Frame rate (timebase denominator).
    pub fps: u32,
    /// Timebase scale (numerator).
    pub timebase_scale: u32,
    /// Frame count from the header (0 if the writer left it unknown).
    pub frame_count: u32,
}

/// Minimal IVF reader: enough to validate files in tests and tooling.
pub struct IvfReader<R: Read> {
    inner: R,
    pub header: IvfHeader,
}

impl<R: Read> IvfReader<R> {
    pub fn new(mut inner: R) -> Result<Self> {
        let mut h = [0u8; HEADER_LEN];
        inner
            .read_exact(&mut h)
            .map_err(|e| CodecError::InvalidIvf(format!("short header: {e}")))?;
        if &h[0..4] != b"DKIF" {
            return Err(CodecError::InvalidIvf(format!(
                "bad signature {:?}, expected DKIF",
                &h[0..4]
            )));
        }
        if &h[8..12] != FOURCC_VP9 {
            return Err(CodecError::InvalidIvf(format!(
                "unsupported FourCC {:?}, expected VP90",
                &h[8..12]
            )));
        }
        let header_len = u16::from_le_bytes(h[6..8].try_into().unwrap()) as usize;
        if header_len > HEADER_LEN {
            // Skip any header extension bytes.
            let mut skip = vec![0u8; header_len - HEADER_LEN];
            inner.read_exact(&mut skip)?;
        }
        let rd32 = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
        let rd16 = |o: usize| u16::from_le_bytes(h[o..o + 2].try_into().unwrap()) as u32;
        let header = IvfHeader {
            fourcc: h[8..12].try_into().unwrap(),
            width: rd16(12),
            height: rd16(14),
            fps: rd32(16),
            timebase_scale: rd32(20),
            frame_count: rd32(24),
        };
        Ok(Self { inner, header })
    }

    /// Read the next frame, or `Ok(None)` at end of stream.
    /// `is_keyframe` is recovered from the VP9 uncompressed header's
    /// frame-marker / frame-type bits — enough for test validation, not a
    /// general VP9 parser.
    pub fn next_frame(&mut self) -> Result<Option<EncodedFrame>> {
        let mut size_buf = [0u8; 4];
        match self.inner.read_exact(&mut size_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let size = u32::from_le_bytes(size_buf) as usize;
        let mut ts_buf = [0u8; 8];
        self.inner.read_exact(&mut ts_buf)?;
        let seq = u64::from_le_bytes(ts_buf);
        let mut data = vec![0u8; size];
        self.inner.read_exact(&mut data)?;
        Ok(Some(EncodedFrame {
            is_keyframe: vp9_is_keyframe(&data),
            data,
            seq,
        }))
    }
}

/// Detect a VP9 keyframe from the uncompressed header.
///
/// VP9 bitstream: 2-bit frame marker (must be `0b10`), then profile bits,
/// `show_existing_frame` bit, then 1-bit frame type (0 = keyframe).
/// Superframes (show_frame = 0 packets preceding a shown frame) are not
/// produced by this crate's realtime configuration, so the first frame's
/// header decides.
pub fn vp9_is_keyframe(data: &[u8]) -> bool {
    vp9_header_info(data)
        .map(|h| h.is_keyframe)
        .unwrap_or(false)
}

/// MSB-first bit reader over a VP9 uncompressed header.
struct Vp9BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Vp9BitReader<'a> {
    fn bit(&mut self) -> u32 {
        let idx = self.pos / 8;
        let b = if idx >= self.data.len() {
            0
        } else {
            (u32::from(self.data[idx]) >> (7 - (self.pos % 8))) & 1
        };
        self.pos += 1;
        b
    }

    fn literal(&mut self, n: usize) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit();
        }
        v
    }
}

/// The part of a VP9 uncompressed header that the tests need.
///
/// VP9 spec, uncompressed header, bit order (MSB-first within each byte),
/// cross-checked against libvpx 1.14's decoder
/// (`vp9/decoder/vp9_decodeframe.c`, `read_uncompressed_header`):
/// frame_marker (2, must be `0b10`), profile (2; +1 reserved bit if
/// profile == 3), show_existing_frame (1), frame_type (1), show_frame (1),
/// error_resilient_mode (1). Keyframes then carry sync code / colour /
/// size; inter frames with `show_frame == 0` carry intra_only (1) —
/// with `show_frame == 1` (always, in this crate's realtime streams)
/// intra_only is implicitly 0 and occupies no bit — then, only when
/// error_resilient_mode == 0, reset_frame_context (2), then (unless
/// intra-only) refresh_frame_flags (8) and, per reference, ref_frame_idx
/// (3) followed by its 1-bit sign bias, in LAST / GOLDEN / ALTREF order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vp9HeaderInfo {
    /// True for keyframes (frame_type == 0).
    pub is_keyframe: bool,
    /// True for show-existing-frame packets (no new frame coded).
    pub show_existing_frame: bool,
    /// True for intra-only inter frames (refreshes all references).
    pub intra_only: bool,
    /// `refresh_frame_flags` for inter frames: bit `i` set means
    /// reference-map slot `i` is refreshed with this frame (LSB-first:
    /// the decoder walks the flags from the least significant bit).
    /// `None` for keyframes, show-existing and intra-only frames.
    pub refresh_frame_flags: Option<u8>,
    /// Reference-map slots of the LAST / GOLDEN / ALTREF references, for
    /// inter frames. `None` for keyframes, show-existing and intra-only
    /// frames.
    pub ref_frame_idx: Option<[u8; 3]>,
}

/// Parse the VP9 uncompressed header far enough to recover the keyframe
/// flag, the refresh flags and the reference indices. Returns `None` if
/// the data is too short or the frame marker is not `0b10`.
pub fn vp9_header_info(data: &[u8]) -> Option<Vp9HeaderInfo> {
    if data.len() < 2 {
        return None;
    }
    let mut r = Vp9BitReader { data, pos: 0 };
    if r.literal(2) != 0b10 {
        return None;
    }
    let profile = r.bit() | (r.bit() << 1);
    if profile == 3 {
        r.bit(); // reserved_zero
    }
    let show_existing_frame = r.bit() == 1;
    if show_existing_frame {
        return Some(Vp9HeaderInfo {
            is_keyframe: false,
            show_existing_frame: true,
            intra_only: false,
            refresh_frame_flags: None,
            ref_frame_idx: None,
        });
    }
    let frame_type = r.bit();
    let show_frame = r.bit();
    let error_resilient = r.bit();
    if frame_type == 0 {
        return Some(Vp9HeaderInfo {
            is_keyframe: true,
            show_existing_frame: false,
            intra_only: false,
            refresh_frame_flags: None,
            ref_frame_idx: None,
        });
    }
    // intra_only is only coded when show_frame == 0 (libvpx:
    // `cm->intra_only = cm->show_frame ? 0 : vpx_rb_read_bit(rb)`).
    let intra_only = if show_frame == 0 { r.bit() == 1 } else { false };
    if error_resilient == 0 {
        r.literal(2); // reset_frame_context
    }
    if intra_only {
        return Some(Vp9HeaderInfo {
            is_keyframe: false,
            show_existing_frame: false,
            intra_only: true,
            refresh_frame_flags: None,
            ref_frame_idx: None,
        });
    }
    let refresh_frame_flags = r.literal(8) as u8;
    let mut ref_frame_idx = [0u8; 3];
    for slot in ref_frame_idx.iter_mut() {
        *slot = r.literal(3) as u8;
        let _sign_bias = r.bit(); // ref_frame_sign_bias, one per reference
    }
    Some(Vp9HeaderInfo {
        is_keyframe: false,
        show_existing_frame: false,
        intra_only: false,
        refresh_frame_flags: Some(refresh_frame_flags),
        ref_frame_idx: Some(ref_frame_idx),
    })
}

/// `refresh_frame_flags` of a VP9 inter frame, or `None` for keyframes /
/// show-existing / intra-only frames / unparseable data. Convenience
/// wrapper over [`vp9_header_info`].
pub fn vp9_refresh_frame_flags(data: &[u8]) -> Option<u8> {
    vp9_header_info(data).and_then(|h| h.refresh_frame_flags)
}
