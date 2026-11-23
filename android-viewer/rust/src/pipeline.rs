//! Frame producers for the Android viewer.
//!
//! Two sources feed the same [`SinkEvent`] channel the winit thread
//! drains into the shared `Compositor`:
//!
//! * **Network** — the real client pipeline, written for this viewer
//!   against the shared crates: `transport::connect` (WebTransport,
//!   certificate-hash pinning) -> control stream (`Hello` out, `Window`
//!   messages in) -> `proto::FrameReassembler` -> `codec::Vp9Decoder`
//!   -> `SinkEvent::Frame`, with a `FrameAck` datagram per decoded
//!   frame. Unlike the e2e test client there is no frame budget and no
//!   overall deadline: the viewer runs until the session closes.
//!   Decoders are per window id (each window has its own VP9 reference
//!   chain), so a host serving several windows works unchanged.
//!
//! * **Demo** — no host at all: the codec test pattern is encoded and
//!   decoded in-process on this thread and fed through the identical
//!   channel. It exercises codec + compositor on the device GPU, so a
//!   fresh install can be smoke-tested before any host exists.
//!
//! A frame that fails to decode gets no `FrameAck`: that silence is
//! what drives the host's reference invalidation (same contract as the
//! e2e client).

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use window_stream_client::demo::I420Frame as ClientFrame;
use window_stream_proto::control::window::Kind as WindowKind;
use window_stream_proto::datagram::encode_frame_ack_datagram;
use window_stream_proto::{FrameAck, FrameReassembler, Hello, PROTOCOL_VERSION};
use window_stream_transport::{
    connect, decode_window, parse_cert_hash_hex, read_control_message, send_hello,
};

use codec::pattern::moving_rectangle;
use codec::{
    DecoderConfig, EncodedFrame, EncoderConfig, VideoDecoder, VideoEncoder, Vp9Decoder, Vp9Encoder,
};

/// One window-lifecycle or frame event for the UI thread.
pub enum SinkEvent {
    Upsert {
        id: u64,
        frame_width: u32,
        frame_height: u32,
        title: String,
    },
    Remove {
        id: u64,
    },
    Frame {
        id: u64,
        frame: ClientFrame,
    },
}

/// Convert a decoded codec frame into the client's I420 frame type.
/// Both are tightly-packed 8-bit I420; this is a plane copy, no colour
/// conversion (that happens in the client's shader). Same conversion
/// as `window_stream_e2e::to_client_frame`, kept local so the viewer
/// does not pull in the e2e crate's host side.
fn to_client_frame(f: &codec::I420Frame) -> ClientFrame {
    ClientFrame {
        width: f.width(),
        height: f.height(),
        y: f.y().to_vec(),
        u: f.u().to_vec(),
        v: f.v().to_vec(),
    }
}

// ---------------------------------------------------------------------------
// Network client
// ---------------------------------------------------------------------------

pub fn run_network_client(url: String, cert_hash_hex: String, tx: Sender<SinkEvent>) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    rt.block_on(network_client(&url, &cert_hash_hex, tx))
}

async fn network_client(url: &str, cert_hash_hex: &str, tx: Sender<SinkEvent>) -> Result<()> {
    let cert_hash = parse_cert_hash_hex(cert_hash_hex).context("parse cert hash")?;
    let session = connect(url, cert_hash).await.context("connect")?;
    tracing::info!(remote = %session.remote_address(), "connected");
    let (mut send, mut recv) = session.open_control().await.context("open control")?;
    send_hello(
        &mut send,
        &Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await
    .context("send Hello")?;

    // Control reader: Window create/update/destroy -> sink events.
    // Runs as its own task; when the host closes the control stream it
    // logs and ends, and the datagram loop below ends when the session
    // itself closes.
    let control_tx = tx.clone();
    tokio::spawn(async move {
        loop {
            match read_control_message(&mut recv).await {
                Ok((tag, body)) => match decode_window(tag, &body) {
                    Ok(window) => {
                        let event = match window.kind() {
                            WindowKind::Destroy => SinkEvent::Remove {
                                id: window.window_id,
                            },
                            _ => SinkEvent::Upsert {
                                id: window.window_id,
                                frame_width: window.width,
                                frame_height: window.height,
                                title: window.title.clone(),
                            },
                        };
                        if control_tx.send(event).is_err() {
                            return; // UI thread gone
                        }
                    }
                    Err(e) => {
                        tracing::warn!(%e, "undecodable control message; ignoring");
                    }
                },
                Err(e) => {
                    tracing::info!(%e, "control stream ended");
                    return;
                }
            }
        }
    });

    let mut decoders: HashMap<u64, Vp9Decoder> = HashMap::new();
    // One reassembler for the session: it keys its in-flight state by
    // window id internally. Decoders are per window (each window has
    // its own VP9 reference chain).
    let mut reassembler = FrameReassembler::new();
    loop {
        let dg = match session.receive_datagram().await {
            Ok(dg) => dg,
            Err(e) => {
                tracing::info!(%e, "session closed; viewer pipeline ending");
                return Ok(());
            }
        };
        let completed = match reassembler.push(&dg) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(%e, "reassembly failed; dropping datagram");
                continue;
            }
        };
        let Some((header, payload)) = completed else {
            continue;
        };
        let decoder = match decoders.get_mut(&header.window_id) {
            Some(d) => d,
            None => match Vp9Decoder::new(DecoderConfig::default()) {
                Ok(d) => decoders.entry(header.window_id).or_insert(d),
                Err(e) => {
                    tracing::error!(window_id = header.window_id, %e, "create VP9 decoder failed");
                    continue;
                }
            },
        };
        let pkt = EncodedFrame {
            data: payload.to_vec(),
            is_keyframe: header.keyframe,
            seq: header.seq,
        };
        match decoder.decode(&pkt) {
            Ok(frames) => {
                for f in &frames {
                    if tx
                        .send(SinkEvent::Frame {
                            id: header.window_id,
                            frame: to_client_frame(f),
                        })
                        .is_err()
                    {
                        return Ok(()); // UI thread gone
                    }
                    let ack = FrameAck {
                        window_id: header.window_id,
                        seq: f.seq(),
                    };
                    if let Err(e) = session.send_datagram(encode_frame_ack_datagram(&ack)) {
                        tracing::warn!(%e, "send FrameAck failed");
                    }
                }
            }
            Err(e) => {
                // No ack for an undecodable frame: the silence drives
                // the host's reference invalidation.
                tracing::warn!(
                    window_id = header.window_id,
                    seq = pkt.seq,
                    %e,
                    "VP9 decode failed; no FrameAck sent"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// In-process demo source (no host)
// ---------------------------------------------------------------------------

pub const DEMO_WINDOW_ID: u64 = 1;
pub const DEMO_WIDTH: u32 = 640;
pub const DEMO_HEIGHT: u32 = 360;
pub const DEMO_BITRATE_KBPS: u32 = 800;
pub const DEMO_TITLE: &str = "demo pattern (in-process VP9)";

pub fn run_demo_source(tx: Sender<SinkEvent>) -> Result<()> {
    if tx
        .send(SinkEvent::Upsert {
            id: DEMO_WINDOW_ID,
            frame_width: DEMO_WIDTH,
            frame_height: DEMO_HEIGHT,
            title: DEMO_TITLE.to_string(),
        })
        .is_err()
    {
        return Ok(());
    }
    let mut encoder = Vp9Encoder::new(EncoderConfig::realtime(
        DEMO_WIDTH,
        DEMO_HEIGHT,
        DEMO_BITRATE_KBPS,
    ))
    .context("create VP9 encoder")?;
    let mut decoder = Vp9Decoder::new(DecoderConfig::default()).context("create VP9 decoder")?;
    let mut seq: u64 = 0;
    loop {
        let started = Instant::now();
        let source = moving_rectangle(DEMO_WIDTH, DEMO_HEIGHT, seq).context("pattern frame")?;
        let packets = encoder.encode(&source).context("VP9 encode")?;
        for pkt in &packets {
            let frames = decoder.decode(pkt).context("VP9 decode")?;
            for f in &frames {
                if tx
                    .send(SinkEvent::Frame {
                        id: DEMO_WINDOW_ID,
                        frame: to_client_frame(f),
                    })
                    .is_err()
                {
                    return Ok(()); // UI thread gone
                }
            }
        }
        seq += 1;
        // Pace ~30 fps, matching the encoder's nominal rate.
        let elapsed = started.elapsed();
        if elapsed < Duration::from_millis(33) {
            std::thread::sleep(Duration::from_millis(33) - elapsed);
        }
    }
}
