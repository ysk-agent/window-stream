//! Hand-rolled binary headers for datagram messages (`Frame`, `Input`,
//! and the `FrameAck` envelope). Not protobuf: fixed layout, big-endian,
//! no varints, so a header parses with a handful of byte reads.
//!
//! Every datagram starts with a 1-byte kind tag so a receiver can route
//! it without inspecting the payload:
//!
//! * `0x01` — Frame (host -> client)
//! * `0x02` — Input (client -> host)
//! * `0x03` — FrameAck (client -> host; rest of the datagram is the
//!   prost encoding of [`crate::FrameAck`])
//!
//! ## Frame header (22 bytes)
//!
//! ```text
//! offset size field
//! 0      1    kind = 0x01
//! 1      1    flags: bit0 = keyframe, bits 1..7 reserved (must be 0)
//! 2      8    window_id (u64)
//! 10     8    seq (u64, per-window, monotonically increasing)
//! 18     2    fragment_index (u16, 0-based)
//! 20     2    fragment_count (u16, >= 1)
//! 22     ..   payload (VP9 data for this fragment)
//! ```
//!
//! Fragmentation: a frame larger than the QUIC datagram budget is split
//! by [`fragment_frame`]. The fragment size is NOT hard-coded: callers
//! pass the session's detected max datagram size (see
//! `transport::Session::max_datagram_size`). Fragments are never
//! retransmitted; a frame with a missing fragment is dropped whole by
//! the [`FrameReassembler`] once a newer sequence shows up, and the hole
//! is closed by reference invalidation (`FrameAck`), not recovery.
//!
//! ## Input header (22 bytes)
//!
//! ```text
//! offset size field
//! 0      1    kind = 0x02
//! 1      1    event kind (see InputEventKind)
//! 2      8    window_id (u64)
//! 10     4    code (u32: key code, or pointer button number)
//! 14     4    x (i32, window-local; pointer events only)
//! 18     4    y (i32, window-local; pointer events only)
//! ```

use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::collections::HashMap;

pub const KIND_FRAME: u8 = 0x01;
pub const KIND_INPUT: u8 = 0x02;
pub const KIND_FRAME_ACK: u8 = 0x03;

pub const FRAME_HEADER_LEN: usize = 22;
pub const INPUT_HEADER_LEN: usize = 22;

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum DatagramError {
    #[error("datagram too short: {actual} bytes, need at least {needed}")]
    TooShort { actual: usize, needed: usize },
    #[error("unknown datagram kind byte: {0:#04x}")]
    UnknownKind(u8),
    #[error("invalid fragment header: index {index} of count {count}")]
    InvalidFragment { index: u16, count: u16 },
    #[error("frame payload is empty")]
    EmptyPayload,
    #[error("invalid FrameAck payload: {0}")]
    InvalidFrameAck(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub keyframe: bool,
    pub window_id: u64,
    pub seq: u64,
    pub fragment_index: u16,
    pub fragment_count: u16,
}

impl FrameHeader {
    pub fn encode(&self, payload: &[u8]) -> Bytes {
        let mut buf = BytesMut::with_capacity(FRAME_HEADER_LEN + payload.len());
        buf.put_u8(KIND_FRAME);
        buf.put_u8(if self.keyframe { 0x01 } else { 0x00 });
        buf.put_u64(self.window_id);
        buf.put_u64(self.seq);
        buf.put_u16(self.fragment_index);
        buf.put_u16(self.fragment_count);
        buf.put_slice(payload);
        buf.freeze()
    }

    /// Parse a datagram known to start with `KIND_FRAME`.
    /// Returns the header and the payload slice.
    pub fn decode(datagram: &[u8]) -> Result<(Self, &[u8]), DatagramError> {
        if datagram.len() < FRAME_HEADER_LEN {
            return Err(DatagramError::TooShort {
                actual: datagram.len(),
                needed: FRAME_HEADER_LEN,
            });
        }
        if datagram[0] != KIND_FRAME {
            return Err(DatagramError::UnknownKind(datagram[0]));
        }
        let mut cur = &datagram[1..];
        let flags = cur.get_u8();
        let header = Self {
            keyframe: flags & 0x01 != 0,
            window_id: cur.get_u64(),
            seq: cur.get_u64(),
            fragment_index: cur.get_u16(),
            fragment_count: cur.get_u16(),
        };
        if header.fragment_count == 0 || header.fragment_index >= header.fragment_count {
            return Err(DatagramError::InvalidFragment {
                index: header.fragment_index,
                count: header.fragment_count,
            });
        }
        if datagram.len() == FRAME_HEADER_LEN {
            // fragment_frame() never emits an empty chunk, so an empty
            // payload can only come from a malformed peer.
            return Err(DatagramError::EmptyPayload);
        }
        Ok((header, &datagram[FRAME_HEADER_LEN..]))
    }
}

/// Split one frame payload into datagrams of at most `max_datagram`
/// bytes each (header included). `max_datagram` must come from the
/// session's detected QUIC limit, never a hard-coded MTU guess; it is
/// clamped so at least 1 payload byte fits per datagram.
pub fn fragment_frame(
    window_id: u64,
    seq: u64,
    keyframe: bool,
    payload: &[u8],
    max_datagram: usize,
) -> Result<Vec<Bytes>, DatagramError> {
    if payload.is_empty() {
        return Err(DatagramError::EmptyPayload);
    }
    let budget = max_datagram.saturating_sub(FRAME_HEADER_LEN).max(1);
    let count = payload.len().div_ceil(budget);
    if count > u16::MAX as usize {
        return Err(DatagramError::InvalidFragment {
            index: 0,
            count: u16::MAX,
        });
    }
    Ok(payload
        .chunks(budget)
        .enumerate()
        .map(|(i, chunk)| {
            FrameHeader {
                keyframe,
                window_id,
                seq,
                fragment_index: i as u16,
                fragment_count: count as u16,
            }
            .encode(chunk)
        })
        .collect())
}

/// Reassembles fragmented frames. Depth is deliberately tiny: at most
/// one incomplete frame per window is kept; when a fragment of a newer
/// sequence arrives, the older incomplete frame is dropped (it will
/// never be retransmitted, so keeping it only wastes memory).
#[derive(Debug, Default)]
pub struct FrameReassembler {
    pending: HashMap<u64, PendingFrame>,
}

#[derive(Debug)]
struct PendingFrame {
    seq: u64,
    keyframe: bool,
    count: u16,
    parts: HashMap<u16, Bytes>,
}

impl FrameReassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one Frame datagram. Returns `Some((header_of_first_fragment,
    /// full_payload))` once the frame is complete.
    pub fn push(&mut self, datagram: &[u8]) -> Result<Option<(FrameHeader, Bytes)>, DatagramError> {
        let (header, payload) = FrameHeader::decode(datagram)?;
        if header.fragment_count == 1 {
            // The fast path must respect the same staleness rules as
            // the multi-fragment path below: a delayed single-fragment
            // frame must not discard a newer frame already being
            // assembled, and a single-fragment datagram claiming the
            // same seq as a pending multi-fragment frame contradicts
            // that frame's fragment count.
            match self.pending.get(&header.window_id) {
                Some(entry) if entry.seq > header.seq => return Ok(None),
                Some(entry) if entry.seq == header.seq => {
                    return Err(DatagramError::InvalidFragment {
                        index: header.fragment_index,
                        count: header.fragment_count,
                    });
                }
                _ => {}
            }
            self.pending.remove(&header.window_id);
            return Ok(Some((header, Bytes::copy_from_slice(payload))));
        }
        // Datagrams are unordered, so a delayed fragment of an older
        // frame can arrive after a newer frame's assembly has started.
        // Only a strictly newer sequence supersedes the pending frame;
        // an older fragment is ignored, and a same-sequence fragment
        // must agree on the fragment count (otherwise the pending
        // frame could never complete: parts.len() would never reach
        // entry.count).
        match self.pending.get_mut(&header.window_id) {
            None => {
                self.pending.insert(
                    header.window_id,
                    PendingFrame {
                        seq: header.seq,
                        keyframe: header.keyframe,
                        count: header.fragment_count,
                        parts: HashMap::new(),
                    },
                );
            }
            Some(entry) if header.seq > entry.seq => {
                *entry = PendingFrame {
                    seq: header.seq,
                    keyframe: header.keyframe,
                    count: header.fragment_count,
                    parts: HashMap::new(),
                };
            }
            Some(entry) if header.seq < entry.seq => return Ok(None),
            Some(entry) => {
                if header.fragment_count != entry.count {
                    return Err(DatagramError::InvalidFragment {
                        index: header.fragment_index,
                        count: header.fragment_count,
                    });
                }
            }
        }
        let entry = self
            .pending
            .get_mut(&header.window_id)
            .expect("entry exists");
        entry
            .parts
            .insert(header.fragment_index, Bytes::copy_from_slice(payload));
        if entry.parts.len() == entry.count as usize {
            let pending = self
                .pending
                .remove(&header.window_id)
                .expect("entry exists");
            let mut full = BytesMut::new();
            for i in 0..pending.count {
                full.put_slice(&pending.parts[&i]);
            }
            let first = FrameHeader {
                keyframe: pending.keyframe,
                window_id: header.window_id,
                seq: pending.seq,
                fragment_index: 0,
                fragment_count: pending.count,
            };
            return Ok(Some((first, full.freeze())));
        }
        Ok(None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum InputEventKind {
    KeyDown = 1,
    KeyUp = 2,
    PointerMove = 3,
    PointerButtonDown = 4,
    PointerButtonUp = 5,
    /// Periodic full input-state snapshot; prevents stuck keys after a
    /// lost key-up datagram (state resync, not retransmission).
    StateSnapshot = 6,
}

impl InputEventKind {
    fn from_u8(v: u8) -> Result<Self, DatagramError> {
        Ok(match v {
            1 => Self::KeyDown,
            2 => Self::KeyUp,
            3 => Self::PointerMove,
            4 => Self::PointerButtonDown,
            5 => Self::PointerButtonUp,
            6 => Self::StateSnapshot,
            other => return Err(DatagramError::UnknownKind(other)),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputHeader {
    pub event: InputEventKind,
    pub window_id: u64,
    /// Key code for key events, button number for pointer button events.
    pub code: u32,
    pub x: i32,
    pub y: i32,
}

impl InputHeader {
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(INPUT_HEADER_LEN);
        buf.put_u8(KIND_INPUT);
        buf.put_u8(self.event as u8);
        buf.put_u64(self.window_id);
        buf.put_u32(self.code);
        buf.put_i32(self.x);
        buf.put_i32(self.y);
        buf.freeze()
    }

    pub fn decode(datagram: &[u8]) -> Result<Self, DatagramError> {
        if datagram.len() < INPUT_HEADER_LEN {
            return Err(DatagramError::TooShort {
                actual: datagram.len(),
                needed: INPUT_HEADER_LEN,
            });
        }
        if datagram[0] != KIND_INPUT {
            return Err(DatagramError::UnknownKind(datagram[0]));
        }
        let mut cur = &datagram[1..];
        Ok(Self {
            event: InputEventKind::from_u8(cur.get_u8())?,
            window_id: cur.get_u64(),
            code: cur.get_u32(),
            x: cur.get_i32(),
            y: cur.get_i32(),
        })
    }
}

/// Wrap a prost-encoded `FrameAck` as a datagram.
pub fn encode_frame_ack_datagram(ack: &crate::FrameAck) -> Bytes {
    use prost::Message;
    let mut buf = BytesMut::with_capacity(1 + ack.encoded_len());
    buf.put_u8(KIND_FRAME_ACK);
    buf.put_slice(&ack.encode_to_vec());
    buf.freeze()
}

pub fn decode_frame_ack_datagram(datagram: &[u8]) -> Result<crate::FrameAck, DatagramError> {
    use prost::Message;
    if datagram.is_empty() {
        return Err(DatagramError::TooShort {
            actual: 0,
            needed: 1,
        });
    }
    if datagram[0] != KIND_FRAME_ACK {
        return Err(DatagramError::UnknownKind(datagram[0]));
    }
    crate::FrameAck::decode(&datagram[1..])
        .map_err(|e| DatagramError::InvalidFrameAck(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_header_roundtrip() {
        let h = FrameHeader {
            keyframe: true,
            window_id: 7,
            seq: 42,
            fragment_index: 0,
            fragment_count: 1,
        };
        let dg = h.encode(b"vp9-bytes");
        let (got, payload) = FrameHeader::decode(&dg).unwrap();
        assert_eq!(got, h);
        assert_eq!(payload, b"vp9-bytes");
    }

    #[test]
    fn input_header_roundtrip() {
        let h = InputHeader {
            event: InputEventKind::PointerMove,
            window_id: 3,
            code: 0,
            x: -5,
            y: 120,
        };
        let got = InputHeader::decode(&h.encode()).unwrap();
        assert_eq!(got, h);
    }

    #[test]
    fn fragmentation_and_reassembly() {
        let payload: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let dgs = fragment_frame(1, 9, false, &payload, 1200).unwrap();
        assert!(dgs.len() > 1);
        assert!(dgs.iter().all(|d| d.len() <= 1200));
        let mut re = FrameReassembler::new();
        let mut out = None;
        // Deliver out of order: reassembler must still complete the frame.
        for dg in dgs.iter().rev() {
            if let Some(done) = re.push(dg).unwrap() {
                out = Some(done);
            }
        }
        let (header, full) = out.expect("frame completes");
        assert_eq!(header.seq, 9);
        assert_eq!(&full[..], &payload[..]);
    }

    #[test]
    fn stale_partial_frame_is_dropped() {
        let a = fragment_frame(1, 1, true, &[7u8; 3000], 1200).unwrap();
        let b = fragment_frame(1, 2, true, &[9u8; 10], 1200).unwrap();
        let mut re = FrameReassembler::new();
        assert!(re.push(&a[0]).unwrap().is_none());
        let done = re
            .push(&b[0])
            .unwrap()
            .expect("new seq completes on its own");
        assert_eq!(done.0.seq, 2);
    }

    #[test]
    fn frame_ack_datagram_roundtrip() {
        let ack = crate::FrameAck {
            window_id: 5,
            seq: 99,
        };
        let dg = encode_frame_ack_datagram(&ack);
        assert_eq!(dg[0], KIND_FRAME_ACK);
        assert_eq!(decode_frame_ack_datagram(&dg).unwrap(), ack);
    }

    #[test]
    fn delayed_stale_fragment_does_not_clobber_newer_frame() {
        // Frame 1 (fragmented) starts, frame 2 starts and completes,
        // then a delayed fragment of frame 1 arrives: it must be
        // ignored, not restart frame 1's assembly.
        let a = fragment_frame(1, 1, true, &[7u8; 3000], 1200).unwrap();
        let b = fragment_frame(1, 2, true, &[9u8; 3000], 1200).unwrap();
        let mut re = FrameReassembler::new();
        assert!(re.push(&a[0]).unwrap().is_none());
        assert!(re.push(&b[0]).unwrap().is_none());
        assert!(re.push(&b[1]).unwrap().is_none());
        let done = re.push(&b[2]).unwrap().expect("frame 2 completes");
        assert_eq!(done.0.seq, 2);
        // Delayed frame-1 fragment: ignored, and nothing else completes.
        assert!(re.push(&a[1]).unwrap().is_none());
        assert!(re.push(&a[2]).unwrap().is_none());
    }

    #[test]
    fn delayed_stale_single_fragment_does_not_clobber_newer_frame() {
        // Same shape as delayed_stale_fragment_does_not_clobber_newer_frame,
        // but the stale frame is a single fragment: before the fix it
        // took the fast path and discarded frame 2's assembly.
        let stale = fragment_frame(1, 1, true, &[7u8; 10], 1200).unwrap();
        assert_eq!(stale.len(), 1);
        let b = fragment_frame(1, 2, true, &[9u8; 3000], 1200).unwrap();
        let mut re = FrameReassembler::new();
        assert!(re.push(&b[0]).unwrap().is_none());
        assert!(re.push(&b[1]).unwrap().is_none());
        // Delayed stale single-fragment frame: ignored.
        assert!(re.push(&stale[0]).unwrap().is_none());
        let done = re.push(&b[2]).unwrap().expect("frame 2 completes");
        assert_eq!(done.0.seq, 2);
        assert_eq!(&done.1[..], &[9u8; 3000][..]);
    }

    #[test]
    fn single_fragment_claiming_pending_seq_is_rejected() {
        // Frame 2 is being assembled from 3 fragments; a single-fragment
        // datagram claiming seq 2 contradicts its fragment count.
        let single = fragment_frame(1, 2, true, &[9u8; 10], 1200).unwrap();
        let b = fragment_frame(1, 2, true, &[9u8; 3000], 1200).unwrap();
        let mut re = FrameReassembler::new();
        assert!(re.push(&b[0]).unwrap().is_none());
        assert!(matches!(
            re.push(&single[0]),
            Err(DatagramError::InvalidFragment { .. })
        ));
        // The pending assembly is untouched and still completes.
        assert!(re.push(&b[1]).unwrap().is_none());
        let done = re.push(&b[2]).unwrap().expect("frame 2 completes");
        assert_eq!(done.0.seq, 2);
    }

    #[test]
    fn mismatched_fragment_count_is_rejected() {
        let a = fragment_frame(1, 1, true, &[7u8; 3000], 1200).unwrap();
        let mut bad = a[1].to_vec();
        // Rewrite fragment_count of the second fragment from 3 to 4.
        bad[20..22].copy_from_slice(&4u16.to_be_bytes());
        let mut re = FrameReassembler::new();
        assert!(re.push(&a[0]).unwrap().is_none());
        assert!(matches!(
            re.push(&bad),
            Err(DatagramError::InvalidFragment { .. })
        ));
    }

    #[test]
    fn empty_frame_payload_is_rejected() {
        let h = FrameHeader {
            keyframe: true,
            window_id: 1,
            seq: 1,
            fragment_index: 0,
            fragment_count: 1,
        };
        let dg = h.encode(&[]);
        assert_eq!(FrameHeader::decode(&dg), Err(DatagramError::EmptyPayload));
        let mut re = FrameReassembler::new();
        assert_eq!(re.push(&dg), Err(DatagramError::EmptyPayload));
    }

    #[test]
    fn corrupt_frame_ack_reports_invalid_frame_ack() {
        let mut dg = encode_frame_ack_datagram(&crate::FrameAck {
            window_id: 5,
            seq: 99,
        })
        .to_vec();
        dg.truncate(2); // valid kind byte, truncated prost body
        assert!(matches!(
            decode_frame_ack_datagram(&dg),
            Err(DatagramError::InvalidFrameAck(_))
        ));
    }
}
