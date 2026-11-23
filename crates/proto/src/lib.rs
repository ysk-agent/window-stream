//! window-stream v1 protocol definitions.
//!
//! Fixed decisions (see `README.md` / `docs/IMPLEMENTATION.md`, do not
//! relitigate): WebTransport transport, VP9 fixed, no FEC, no
//! retransmission, five core messages only.
//!
//! * Control messages ([`Hello`], [`Window`], [`FrameAck`]) are protobuf
//!   via `prost` ([`control`] module, generated from `proto/control.proto`).
//! * Datagram headers for [`Frame`](datagram::FrameHeader) and
//!   [`Input`](datagram::InputHeader) are hand-rolled fixed binary layouts
//!   in [`datagram`], because they sit on the per-frame hot path.

pub mod control {
    include!(concat!(env!("OUT_DIR"), "/windowstream.v1.rs"));
}

pub mod datagram;

pub use control::{FrameAck, Hello, Window};
pub use datagram::{
    fragment_frame, FrameHeader, FrameReassembler, InputEventKind, InputHeader, DatagramError,
    KIND_FRAME, KIND_FRAME_ACK, KIND_INPUT,
};

/// Protocol version sent in [`Hello`].
pub const PROTOCOL_VERSION: u32 = 1;
