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
    if data.len() < 2 {
        return false;
    }
    // VP9's bit reader consumes bits MSB-first within each byte.
    let bit = |pos: usize| -> u32 {
        let idx = pos / 8;
        if idx >= data.len() {
            return 0;
        }
        (u32::from(data[idx]) >> (7 - (pos % 8))) & 1
    };
    // frame_marker: 2 bits, value 0b10.
    if bit(0) != 1 || bit(1) != 0 {
        return false;
    }
    // profile_low (1 bit), profile_high (1 bit), [reserved (1 bit) if
    // profile == 3], show_existing (1 bit), frame_type (1 bit).
    let profile = bit(2) | (bit(3) << 1);
    let mut pos = 4usize;
    if profile == 3 {
        pos += 1; // reserved_zero
    }
    if bit(pos) == 1 {
        return false; // show_existing_frame: no new frame coded
    }
    bit(pos + 1) == 0 // frame_type: 0 = key frame
}
