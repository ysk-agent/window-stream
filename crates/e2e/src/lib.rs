//! End-to-end wiring for window-stream (track/e2e-wiring).
//!
//! The real frame path, wired for the first time:
//!
//! ```text
//! source::FrameSource                (frame source: PatternSource by
//!        |                            default; RealCaptureSource pulls
//!        |                            ScreenCaptureKit BGRA on macOS
//!        |                            and converts it to I420)
//!        v
//! codec::Vp9Encoder (AckGated)  -- VP9 bitstream
//!        |
//!        v
//! proto::fragment_frame         -- Frame datagrams (header + fragments,
//!        |                            sized by the session's max datagram)
//!        v   transport::Session datagrams (WebTransport/QUIC), or an
//!        |   in-memory datagram loop with the identical proto/codec code
//!        v
//! proto::FrameReassembler -> codec::Vp9Decoder -> I420
//!        |
//!        v
//! client::sink::FrameSink       (Compositor in --display mode,
//!        |                        CountingSink headless)
//!        v
//! proto FrameAck datagram ------> host: VideoEncoder::set_reference_constraint
//! ```
//!
//! Placement decision: this lives in a NEW crate (`crates/e2e`), not in
//! `dummy` and not in `client`. `dummy`'s contract is "protocol round trip
//! with payloads standing in for VP9, no codec" — adding libvpx + wgpu
//! dependencies there would break it and slow its tests. `client`'s
//! contract is compositing behind the `FrameSink` seam. The wiring is a
//! third thing — it owns neither — so it gets its own crate that depends
//! on all of them one-way, matching `docs/IMPLEMENTATION.md`
//! (`host / client -> transport / codec / platform -> proto`): the e2e
//! host is the `host` pipeline and the e2e client is the `client`
//! pipeline, in miniature, before real capture exists.
//!
//! Fixed decisions are untouched: WebTransport, VP9, no retransmission,
//! no FEC, five messages, FrameAck-driven reference invalidation.
//!
//! Real capture (track/real-capture): the host no longer calls the
//! dummy pattern directly. It pulls frames from a
//! [`source::FrameSource`]; [`source::PatternSource`] is the default
//! (and the only source that runs on Linux / in CI), while
//! [`source::RealCaptureSource`] starts a platform capture session
//! and feeds ScreenCaptureKit BGRA frames through the same encode →
//! fragment → decode → sink path. The encoder is sized from the
//! first frame that arrives, so a Retina (2x) capture flows through
//! with no size hard-coded anywhere. See `docs/REAL_CAPTURE.md` for
//! the on-device verification procedure; the real-device end-to-end
//! run itself is still unverified (it needs a GUI session and the
//! Screen Recording grant, which this VM / CI cannot provide).

pub mod source;

pub use source::{
    captured_frame_to_i420, CapturedFrameSource, FrameSource, PatternSource, RealCaptureSource,
    ReferenceFn, CAPTURE_FRAME_TIMEOUT,
};

use anyhow::{Context, Result};
use std::time::Duration;
use tracing::info;
use window_stream_client::demo::I420Frame as ClientFrame;
use window_stream_client::remote_window::WindowGeometry;
use window_stream_client::sink::{FrameError, FrameSink};
use window_stream_proto::control::window::Kind as WindowKind;
use window_stream_proto::datagram::{
    decode_frame_ack_datagram, encode_frame_ack_datagram, fragment_frame,
};
use window_stream_proto::{FrameAck, FrameReassembler, Hello, Window, PROTOCOL_VERSION};
use window_stream_transport::{
    connect, decode_hello, decode_window, parse_cert_hash_hex, read_control_message, send_hello,
    send_window, HostEndpoint, SelfSignedIdentity, Session,
};

use codec::{
    DecoderConfig, EncodedFrame, EncoderConfig, I420Frame, VideoDecoder, VideoEncoder, Vp9Decoder,
    Vp9Encoder,
};

pub const E2E_WINDOW_ID: u64 = 1;
pub const E2E_WIDTH: u32 = 640;
pub const E2E_HEIGHT: u32 = 360;
pub const E2E_TITLE: &str = "e2e test pattern (VP9)";
pub const E2E_BITRATE_KBPS: u32 = 800;
/// Datagram budget used by the in-memory loop. The network path never
/// uses this: it takes `Session::max_datagram_size()` (per the design,
/// fragment size comes from QUIC, never a hard-coded MTU).
pub const INPROCESS_MAX_DATAGRAM: usize = 1200;

pub fn encoder_config() -> EncoderConfig {
    // EncoderConfig::realtime defaults to ReferenceMode::AckGated.
    EncoderConfig::realtime(E2E_WIDTH, E2E_HEIGHT, E2E_BITRATE_KBPS)
}

/// Target bitrate for a frame size, in kbit/s.
///
/// The 640x360 pattern runs at 800 kbit/s; larger frames scale
/// linearly with pixel count, clamped to 800..=8000 kbit/s. This is
/// a v1 sizing heuristic for the host (a Retina capture at
/// 3360x2100 needs far more than the pattern bitrate), not adaptive
/// quality control — that stays deferred per the README.
pub fn bitrate_for_size(width: u32, height: u32) -> u32 {
    let pixels = u64::from(width) * u64::from(height);
    let scaled =
        pixels * u64::from(E2E_BITRATE_KBPS) / (u64::from(E2E_WIDTH) * u64::from(E2E_HEIGHT));
    scaled.clamp(800, 8000) as u32
}

/// Realtime encoder config sized to the first frame a source
/// produces. The host sizes its encoder from that frame, so a real
/// capture's pixel size (Retina included) needs no configuration.
pub fn encoder_config_for(width: u32, height: u32) -> EncoderConfig {
    EncoderConfig::realtime(width, height, bitrate_for_size(width, height))
}

/// Convert a decoded codec frame into the client's I420 frame type.
/// Both are tightly-packed 8-bit I420; this is a plane copy, no colour
/// conversion (that happens in the client's shader).
pub fn to_client_frame(f: &codec::I420Frame) -> ClientFrame {
    ClientFrame {
        width: f.width(),
        height: f.height(),
        y: f.y().to_vec(),
        u: f.u().to_vec(),
        v: f.v().to_vec(),
    }
}

// ---------------------------------------------------------------------------
// Headless FrameSink
// ---------------------------------------------------------------------------

/// A `FrameSink` that counts and checks frames instead of displaying
/// them. Used by the headless client, the in-process loop and the tests,
/// so the numeric verification runs through the exact trait the
/// Compositor implements.
#[derive(Default)]
pub struct CountingSink {
    pub windows: Vec<u64>,
    pub frames_submitted: u64,
    pub last_seq_frame_checksum: u64,
    pub last_frame: Option<ClientFrame>,
}

impl std::fmt::Debug for CountingSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CountingSink")
            .field("windows", &self.windows)
            .field("frames_submitted", &self.frames_submitted)
            .field("last_seq_frame_checksum", &self.last_seq_frame_checksum)
            .field("has_last_frame", &self.last_frame.is_some())
            .finish()
    }
}

impl FrameSink for CountingSink {
    fn upsert_window(
        &mut self,
        id: u64,
        frame_width: u32,
        frame_height: u32,
        _geometry: WindowGeometry,
        _title: &str,
    ) -> Result<(), FrameError> {
        if frame_width == 0 || frame_height == 0 {
            return Err(FrameError::InvalidFrame {
                width: frame_width,
                height: frame_height,
                reason: "zero frame size".into(),
            });
        }
        if !self.windows.contains(&id) {
            self.windows.push(id);
        }
        Ok(())
    }

    fn remove_window(&mut self, id: u64) -> bool {
        let before = self.windows.len();
        self.windows.retain(|w| *w != id);
        self.windows.len() != before
    }

    fn submit_frame(&mut self, id: u64, frame: &ClientFrame) -> Result<(), FrameError> {
        if !self.windows.contains(&id) {
            return Err(FrameError::UnknownWindow(id));
        }
        // FNV-1a over luma: a cheap content fingerprint for the logs.
        let mut h: u64 = 0xcbf29ce484222325;
        for b in &frame.y {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        self.last_seq_frame_checksum = h;
        self.frames_submitted += 1;
        self.last_frame = Some(ClientFrame {
            width: frame.width,
            height: frame.height,
            y: frame.y.clone(),
            u: frame.u.clone(),
            v: frame.v.clone(),
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Shared stats
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostStats {
    pub frames_encoded: u64,
    pub frames_sent: u64,
    pub keyframes_sent: u64,
    pub encoded_bytes_total: usize,
    pub acks_received: Vec<u64>,
    /// Times a gap / stall in acks forced `invalidate_reference()`.
    pub invalidations: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientStats {
    pub window_id: u64,
    pub frames_reassembled: Vec<u64>,
    pub frames_decoded: Vec<u64>,
    pub decode_errors: u64,
    pub acks_sent: Vec<u64>,
    pub min_luma_psnr_db_x10: i64,
}

// ---------------------------------------------------------------------------
// Host pipeline (WebTransport)
// ---------------------------------------------------------------------------

pub struct HostHandle {
    pub addr: std::net::SocketAddr,
    pub cert_hash_hex: String,
    /// The host runs on its own thread with a current-thread runtime:
    /// the VP9 encoder is `!Send` by design (one encoder per window,
    /// driven from its dedicated thread — see docs/IMPLEMENTATION.md:
    /// "Capture and encode run on dedicated threads; tokio tasks do
    /// I/O only"), so it must never be spawned onto a work-stealing
    /// pool while the encoder is live across an await.
    pub task: std::thread::JoinHandle<Result<HostStats>>,
}

pub async fn start_host(frame_count: u64) -> Result<HostHandle> {
    start_host_with_source(frame_count, PatternSource::new()).await
}

/// Start a host on `127.0.0.1:0` with an explicit frame source, on
/// its own thread (see `HostHandle` for why the encoder must never
/// live on a work-stealing pool).
pub async fn start_host_with_source<S>(frame_count: u64, source: S) -> Result<HostHandle>
where
    S: FrameSource + Send + 'static,
{
    start_host_on("127.0.0.1:0".parse().unwrap(), frame_count, source).await
}

/// Start a host bound to `addr` with an explicit frame source, on
/// its own thread. This is the entry point `e2e-host` uses so it can
/// listen on a chosen `--port`.
pub async fn start_host_on<S>(
    addr: std::net::SocketAddr,
    frame_count: u64,
    source: S,
) -> Result<HostHandle>
where
    S: FrameSource + Send + 'static,
{
    let identity = SelfSignedIdentity::generate(&["localhost", "127.0.0.1", "::1"])
        .context("generate self-signed identity")?;
    let cert_hash_hex = identity.cert_hash_hex().to_string();
    let endpoint = HostEndpoint::bind(addr, identity).context("bind host endpoint")?;
    let addr = endpoint.local_addr();
    info!(%addr, "e2e-host listening");
    let task = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build host runtime")?;
        rt.block_on(run_host_with_source(endpoint, frame_count, source))
    });
    Ok(HostHandle {
        addr,
        cert_hash_hex,
        task,
    })
}

pub async fn run_host(endpoint: HostEndpoint, frame_count: u64) -> Result<HostStats> {
    run_host_with_source(endpoint, frame_count, PatternSource::new()).await
}

/// Host pipeline over WebTransport, pulling frames from `source`.
///
/// The first frame is pulled *before* the `Window` message is sent:
/// its size (and the source's title) is what the client is told
/// about the window, and it is what the encoder is sized to — so a
/// real capture's pixel size, Retina scaling included, needs no
/// configuration. A later frame whose size differs from the first
/// (the window was resized mid-stream) is an error in v1: the
/// encoder is fixed-size, so the host stops and the operator
/// restarts it (see `docs/REAL_CAPTURE.md`).
///
/// `FrameSource::next_frame` blocks while waiting for the next
/// captured frame; that is intentional — this function runs on the
/// host's dedicated thread (see `HostHandle`), and FrameAcks are
/// collected in the pacing window after each encode.
pub async fn run_host_with_source<S>(
    endpoint: HostEndpoint,
    frame_count: u64,
    mut source: S,
) -> Result<HostStats>
where
    S: FrameSource,
{
    let session: Session = endpoint.accept().await.context("accept session")?;
    let (mut send, mut recv) = session.accept_control().await.context("accept control")?;

    let (tag, body) = read_control_message(&mut recv)
        .await
        .context("read Hello")?;
    let hello = decode_hello(tag, &body).context("decode Hello")?;
    if hello.version != PROTOCOL_VERSION {
        anyhow::bail!("unsupported protocol version {}", hello.version);
    }

    let mut stats = HostStats::default();
    if frame_count == 0 {
        session.close(0, "done");
        return Ok(stats);
    }
    let first = source.next_frame().context("first frame from source")?;
    let (width, height) = (first.width(), first.height());
    let window_id = source.window_id();
    let title = source.title().to_string();
    send_window(
        &mut send,
        &Window {
            window_id,
            kind: WindowKind::Create as i32,
            x: 0,
            y: 0,
            width,
            height,
            title: title.clone(),
        },
    )
    .await
    .context("send Window")?;

    let max_dg = session.max_datagram_size().unwrap_or(1200);
    info!(
        stream = "datagram",
        max_datagram_size = max_dg,
        window_id,
        width,
        height,
        title = %title,
        "e2e-host sending VP9 frames"
    );
    let mut encoder =
        Vp9Encoder::new(encoder_config_for(width, height)).context("create VP9 encoder")?;
    let mut last_ack: Option<u64> = None;

    send_encoded(
        &session,
        &mut encoder,
        &first,
        window_id,
        max_dg,
        &mut stats,
    )
    .await?;
    pace_and_collect_acks(&session, &mut encoder, &mut stats, &mut last_ack, window_id).await;

    for _ in 1..frame_count {
        let frame = source.next_frame().context("next frame from source")?;
        if (frame.width(), frame.height()) != (width, height) {
            anyhow::bail!(
                "window resized mid-stream: {}x{} -> {}x{}; restart the host \
                 (v1 has no mid-stream encoder resize)",
                width,
                height,
                frame.width(),
                frame.height()
            );
        }
        send_encoded(
            &session,
            &mut encoder,
            &frame,
            window_id,
            max_dg,
            &mut stats,
        )
        .await?;
        pace_and_collect_acks(&session, &mut encoder, &mut stats, &mut last_ack, window_id).await;
    }
    // Drain remaining acks.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while (stats.acks_received.len() as u64) < stats.frames_sent {
        match tokio::time::timeout_at(deadline, session.receive_datagram()).await {
            Ok(Ok(dg)) => {
                if let Ok(ack) = decode_frame_ack_datagram(&dg) {
                    apply_ack(&mut encoder, &mut stats, &mut last_ack, window_id, ack.seq);
                }
            }
            _ => break,
        }
    }
    session.close(0, "done");
    Ok(stats)
}

/// Encode one source frame and send it as fragmented Frame datagrams.
async fn send_encoded(
    session: &Session,
    encoder: &mut Vp9Encoder,
    frame: &I420Frame,
    window_id: u64,
    max_dg: usize,
    stats: &mut HostStats,
) -> Result<()> {
    let packets = encoder.encode(frame).context("VP9 encode")?;
    for pkt in &packets {
        let frags = fragment_frame(window_id, pkt.seq, pkt.is_keyframe, &pkt.data, max_dg)
            .context("fragment frame")?;
        for f in &frags {
            session.send_datagram(f).context("send Frame datagram")?;
        }
        stats.frames_sent += 1;
        if pkt.is_keyframe {
            stats.keyframes_sent += 1;
        }
        stats.encoded_bytes_total += pkt.data.len();
        info!(
            stream = "datagram",
            window_id,
            seq = pkt.seq,
            keyframe = pkt.is_keyframe,
            bytes = pkt.data.len(),
            fragments = frags.len(),
            "sent Frame (VP9)"
        );
    }
    stats.frames_encoded += packets.len() as u64;
    Ok(())
}

/// Pace ~30 fps and collect FrameAcks in the pacing window; each ack
/// tightens the encoder's reference constraint before the next frame
/// is encoded.
async fn pace_and_collect_acks(
    session: &Session,
    encoder: &mut Vp9Encoder,
    stats: &mut HostStats,
    last_ack: &mut Option<u64>,
    window_id: u64,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(33);
    while let Ok(Ok(dg)) = tokio::time::timeout_at(deadline, session.receive_datagram()).await {
        if let Ok(ack) = decode_frame_ack_datagram(&dg) {
            apply_ack(encoder, stats, last_ack, window_id, ack.seq);
        }
    }
}

fn apply_ack(
    encoder: &mut Vp9Encoder,
    stats: &mut HostStats,
    last_ack: &mut Option<u64>,
    window_id: u64,
    seq: u64,
) {
    // A jump over un-acked seqs means those frames were lost at the
    // client (never decoded, never acked): the reference chain is
    // broken for anything after the gap, so invalidate promptly
    // instead of waiting for the encoder's ack-stall escalation.
    if let Some(prev) = *last_ack {
        if seq > prev + 1 {
            encoder.invalidate_reference();
            stats.invalidations += 1;
            info!(
                stream = "datagram",
                prev_ack = prev,
                ack = seq,
                "FrameAck gap: reference invalidated"
            );
        }
    }
    encoder.set_reference_constraint(Some(seq));
    *last_ack = Some(seq);
    stats.acks_received.push(seq);
    info!(
        stream = "datagram",
        window_id, seq, "received FrameAck -> set_reference_constraint"
    );
}

// ---------------------------------------------------------------------------
// Client pipeline (WebTransport, generic over the FrameSink)
// ---------------------------------------------------------------------------

pub async fn run_client<S: FrameSink>(
    url: &str,
    cert_hash_hex: &str,
    expected_frames: u64,
    sink: &mut S,
) -> Result<ClientStats> {
    let cert_hash = parse_cert_hash_hex(cert_hash_hex).context("parse cert hash")?;
    let session = connect(url, cert_hash).await.context("connect")?;
    let (mut send, mut recv) = session.open_control().await.context("open control")?;
    send_hello(
        &mut send,
        &Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await
    .context("send Hello")?;
    let (tag, body) = read_control_message(&mut recv)
        .await
        .context("read Window")?;
    let window = decode_window(tag, &body).context("decode Window")?;
    sink.upsert_window(
        window.window_id,
        window.width,
        window.height,
        WindowGeometry {
            x: window.x as f32,
            y: window.y as f32,
            width: window.width as f32,
            height: (window.height + 24) as f32,
        },
        &window.title,
    )
    .map_err(|e| anyhow::anyhow!("upsert_window: {e}"))?;
    info!(
        stream = "control",
        window_id = window.window_id,
        width = window.width,
        height = window.height,
        "received Window -> FrameSink::upsert_window"
    );

    let stats = receive_decode_ack(&session, expected_frames, sink, None).await?;
    if tokio::time::timeout(Duration::from_secs(5), session.closed())
        .await
        .is_err()
    {
        session.close(0, "done");
    }
    Ok(ClientStats {
        window_id: window.window_id,
        ..stats
    })
}

/// The datagram half of the client: reassemble -> decode -> sink -> ack.
/// Shared by the network client; the in-process loop mirrors it exactly.
///
/// `reference` is the source's deterministic reference frame, when the
/// host's source has one (the pattern source does; real capture does
/// not). The network client never has one — it cannot know what the
/// host's source produced — so it passes `None` and reports no PSNR.
/// Only the in-process loop, which runs host and client in one
/// process, can compare decoded frames against the source.
async fn receive_decode_ack<S: FrameSink>(
    session: &Session,
    expected_frames: u64,
    sink: &mut S,
    reference: Option<ReferenceFn>,
) -> Result<ClientStats> {
    let mut decoder = Vp9Decoder::new(DecoderConfig::default()).context("create VP9 decoder")?;
    let mut reassembler = FrameReassembler::new();
    let mut stats = ClientStats::default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while (stats.frames_decoded.len() as u64) < expected_frames {
        let dg = tokio::time::timeout_at(deadline, session.receive_datagram())
            .await
            .context("timed out waiting for Frame")?
            .context("receive Frame datagram")?;
        if let Some((header, payload)) = reassembler.push(&dg).context("reassemble")? {
            stats.frames_reassembled.push(header.seq);
            decode_sink_ack(
                &mut decoder,
                sink,
                &EncodedFrame {
                    data: payload.to_vec(),
                    is_keyframe: header.keyframe,
                    seq: header.seq,
                },
                header.window_id,
                &mut stats,
                reference,
                |ack| {
                    session
                        .send_datagram(encode_frame_ack_datagram(&ack))
                        .map_err(|e| anyhow::anyhow!("send FrameAck: {e}"))
                },
            )?;
        }
    }
    Ok(stats)
}

fn decode_sink_ack(
    decoder: &mut Vp9Decoder,
    sink: &mut impl FrameSink,
    pkt: &EncodedFrame,
    window_id: u64,
    stats: &mut ClientStats,
    reference: Option<ReferenceFn>,
    mut send_ack: impl FnMut(FrameAck) -> Result<()>,
) -> Result<()> {
    match decoder.decode(pkt) {
        Ok(frames) => {
            for f in &frames {
                // Content check against the source's deterministic
                // reference, when the source has one. Real captured
                // frames have no reference: PSNR stays unset (0)
                // and the frame is still submitted and acked.
                if let Some(reference) = reference {
                    if let Some(src) = reference(f.width(), f.height(), f.seq()) {
                        let psnr = codec::luma_psnr(&src, f);
                        if psnr.is_finite() {
                            let v = (psnr * 10.0) as i64;
                            if stats.min_luma_psnr_db_x10 == 0 || v < stats.min_luma_psnr_db_x10 {
                                stats.min_luma_psnr_db_x10 = v;
                            }
                        }
                    }
                }
                sink.submit_frame(window_id, &to_client_frame(f))
                    .map_err(|e| anyhow::anyhow!("submit_frame: {e}"))?;
                stats.frames_decoded.push(f.seq());
                info!(
                    stream = "datagram",
                    window_id,
                    seq = f.seq(),
                    keyframe = pkt.is_keyframe,
                    "decoded VP9 -> FrameSink::submit_frame"
                );
                let ack = FrameAck {
                    window_id,
                    seq: f.seq(),
                };
                send_ack(ack)?;
                stats.acks_sent.push(f.seq());
            }
        }
        Err(e) => {
            // No ack for an undecodable frame: that silence is exactly
            // what drives the host's reference invalidation.
            stats.decode_errors += 1;
            tracing::warn!(stream="datagram", window_id, seq=pkt.seq, %e, "VP9 decode failed; no FrameAck sent");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// In-process loop: identical proto + codec + sink code, datagrams carried
// in memory. This is the verification path when localhost UDP/QUIC is
// blocked by the VM sandbox (sendmsg EPERM, verified with a plain UDP
// echo probe); it does NOT verify wtransport/QUIC itself.
// ---------------------------------------------------------------------------

pub fn run_inprocess<S: FrameSink>(
    frame_count: u64,
    sink: &mut S,
) -> Result<(HostStats, ClientStats)> {
    run_inprocess_with_source(frame_count, sink, PatternSource::new())
}

/// In-process loop with an explicit frame source: identical proto +
/// codec + sink code to the network path, datagrams carried in
/// memory. This is the verification path when localhost UDP/QUIC is
/// blocked by the VM sandbox (sendmsg EPERM, verified with a plain
/// UDP echo probe); it does NOT verify wtransport/QUIC itself.
///
/// As in `run_host_with_source`, the encoder is sized from the
/// source's first frame, and a mid-stream size change is an error.
pub fn run_inprocess_with_source<S: FrameSink, F: FrameSource>(
    frame_count: u64,
    sink: &mut S,
    mut source: F,
) -> Result<(HostStats, ClientStats)> {
    let mut host = HostStats::default();
    let mut client = ClientStats::default();
    if frame_count == 0 {
        return Ok((host, client));
    }
    let first = source.next_frame().context("first frame from source")?;
    let (width, height) = (first.width(), first.height());
    let window_id = source.window_id();
    let title = source.title().to_string();
    let reference = source.reference_fn();
    sink.upsert_window(
        window_id,
        width,
        height,
        WindowGeometry {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: (height + 24) as f32,
        },
        &title,
    )
    .map_err(|e| anyhow::anyhow!("upsert_window: {e}"))?;
    client.window_id = window_id;

    let mut encoder =
        Vp9Encoder::new(encoder_config_for(width, height)).context("create VP9 encoder")?;
    let mut decoder = Vp9Decoder::new(DecoderConfig::default()).context("create VP9 decoder")?;
    let mut last_ack: Option<u64> = None;
    // FrameAck channel: client acks are applied to the encoder before
    // the next frame is encoded, exactly as in run_host's pacing loop.
    let mut pending_acks: Vec<u64> = Vec::new();

    let encode_send_decode = |frame: &I420Frame,
                              encoder: &mut Vp9Encoder,
                              decoder: &mut Vp9Decoder,
                              host: &mut HostStats,
                              client: &mut ClientStats,
                              pending_acks: &mut Vec<u64>,
                              sink: &mut S|
     -> Result<()> {
        let packets = encoder.encode(frame).context("VP9 encode")?;
        host.frames_encoded += packets.len() as u64;
        for pkt in &packets {
            host.frames_sent += 1;
            if pkt.is_keyframe {
                host.keyframes_sent += 1;
            }
            host.encoded_bytes_total += pkt.data.len();
            let frags = fragment_frame(
                window_id,
                pkt.seq,
                pkt.is_keyframe,
                &pkt.data,
                INPROCESS_MAX_DATAGRAM,
            )
            .context("fragment frame")?;
            let mut reassembler = FrameReassembler::new();
            for frag in &frags {
                if let Some((header, payload)) = reassembler.push(frag).context("reassemble")? {
                    client.frames_reassembled.push(header.seq);
                    let mut acked = Vec::new();
                    decode_sink_ack(
                        decoder,
                        sink,
                        &EncodedFrame {
                            data: payload.to_vec(),
                            is_keyframe: header.keyframe,
                            seq: header.seq,
                        },
                        header.window_id,
                        client,
                        reference,
                        |ack| {
                            acked.push(ack.seq);
                            Ok(())
                        },
                    )?;
                    pending_acks.extend(acked);
                }
            }
        }
        Ok(())
    };

    encode_send_decode(
        &first,
        &mut encoder,
        &mut decoder,
        &mut host,
        &mut client,
        &mut pending_acks,
        sink,
    )?;
    for _ in 1..frame_count {
        for ack_seq in pending_acks.drain(..) {
            apply_ack(&mut encoder, &mut host, &mut last_ack, window_id, ack_seq);
        }
        let frame = source.next_frame().context("next frame from source")?;
        if (frame.width(), frame.height()) != (width, height) {
            anyhow::bail!(
                "window resized mid-stream: {}x{} -> {}x{}; restart the host \
                 (v1 has no mid-stream encoder resize)",
                width,
                height,
                frame.width(),
                frame.height()
            );
        }
        encode_send_decode(
            &frame,
            &mut encoder,
            &mut decoder,
            &mut host,
            &mut client,
            &mut pending_acks,
            sink,
        )?;
    }
    for ack_seq in pending_acks.drain(..) {
        apply_ack(&mut encoder, &mut host, &mut last_ack, window_id, ack_seq);
    }
    Ok((host, client))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_scales_with_pixels_and_clamps() {
        assert_eq!(bitrate_for_size(640, 360), 800);
        assert_eq!(bitrate_for_size(320, 240), 800);
        assert_eq!(bitrate_for_size(1280, 720), 3200);
        // Retina example from the Mac verification (3360x2100):
        // 30.6x the pattern pixels, clamped to the 8000 cap.
        assert_eq!(bitrate_for_size(3360, 2100), 8000);
    }
}
