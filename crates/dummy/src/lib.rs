//! Dummy host / client pair: verifies the window-stream protocol end
//! to end with a synthetic window, no OS capture involved.
//!
//! Flow under test (see `README.md`):
//! `Hello` (control stream) -> `Window` CREATE (control stream) ->
//! `Frame` datagrams (fragmented when larger than the session's
//! detected max datagram size) -> `FrameAck` datagrams.
//!
//! The frame payload is a deterministic test pattern standing in for
//! VP9 data (this track has no codec). The pattern is a function of
//! `(seq, byte_index)` so the client can verify payload integrity
//! after reassembly: [`test_pattern_byte`].

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::info;
use window_stream_proto::control::window::Kind as WindowKind;
use window_stream_proto::datagram::{
    decode_frame_ack_datagram, encode_frame_ack_datagram, fragment_frame,
};
use window_stream_proto::{FrameAck, FrameReassembler, Hello, Window, PROTOCOL_VERSION};
use window_stream_transport::{
    connect, decode_hello, decode_window, parse_cert_hash_hex, read_control_message, send_hello,
    send_window, HostEndpoint, SelfSignedIdentity, Session,
};

pub const DUMMY_WINDOW_ID: u64 = 1;
pub const DUMMY_WIDTH: u32 = 640;
pub const DUMMY_HEIGHT: u32 = 360;
pub const DUMMY_TITLE: &str = "dummy test pattern";

/// Default synthetic frame size. Deliberately larger than a typical
/// QUIC datagram so the fragmentation path is exercised.
pub const DEFAULT_FRAME_BYTES: usize = 4096;

pub fn test_pattern_byte(seq: u64, index: usize) -> u8 {
    ((seq.wrapping_mul(131) + index as u64) % 251) as u8
}

pub fn test_pattern(seq: u64, len: usize) -> Vec<u8> {
    (0..len).map(|i| test_pattern_byte(seq, i)).collect()
}

pub fn verify_test_pattern(seq: u64, data: &[u8]) -> bool {
    data.iter()
        .enumerate()
        .all(|(i, b)| *b == test_pattern_byte(seq, i))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostStats {
    pub hello_version: u32,
    pub frames_sent: u64,
    pub acks_received: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientStats {
    pub hello_version: u32,
    pub window_id: u64,
    pub window_width: u32,
    pub window_height: u32,
    pub frames_received: Vec<u64>,
    pub payload_bytes_total: usize,
}

pub struct HostHandle {
    pub addr: SocketAddr,
    pub cert_hash_hex: String,
    pub task: JoinHandle<Result<HostStats>>,
}

/// Start a dummy host on an ephemeral localhost port. The returned
/// handle carries everything a client needs: address + cert hash.
pub async fn start_host(frame_count: u64, frame_bytes: usize) -> Result<HostHandle> {
    let identity = SelfSignedIdentity::generate(&["localhost", "127.0.0.1", "::1"])
        .context("generate self-signed identity")?;
    let cert_hash_hex = identity.cert_hash_hex().to_string();
    let endpoint = HostEndpoint::bind("127.0.0.1:0".parse().unwrap(), identity)
        .context("bind host endpoint")?;
    let addr = endpoint.local_addr();
    info!(%addr, cert_hash = %cert_hash_hex, "dummy-host listening");
    let task = tokio::spawn(async move { run_host(endpoint, frame_count, frame_bytes).await });
    Ok(HostHandle {
        addr,
        cert_hash_hex,
        task,
    })
}

/// Serve exactly one session, then finish.
pub async fn run_host(
    endpoint: HostEndpoint,
    frame_count: u64,
    frame_bytes: usize,
) -> Result<HostStats> {
    let session: Session = endpoint.accept().await.context("accept session")?;
    info!(remote = %session.remote_address(), stream = "control", "session established");
    let (mut send, mut recv) = session
        .accept_control()
        .await
        .context("accept control stream")?;

    // Hello (client -> host, control stream)
    let (tag, body) = read_control_message(&mut recv)
        .await
        .context("read Hello")?;
    let hello = decode_hello(tag, &body).context("decode Hello")?;
    info!(
        stream = "control",
        version = hello.version,
        "received Hello"
    );
    // The version field is the compatibility gate: accept only the
    // protocol version this implementation speaks. Returning an error
    // here drops the session instead of silently interoperating with
    // a version we do not understand.
    if hello.version != PROTOCOL_VERSION {
        anyhow::bail!(
            "unsupported protocol version {} (this host speaks v{})",
            hello.version,
            PROTOCOL_VERSION
        );
    }

    // Window CREATE (host -> client, control stream)
    let window = Window {
        window_id: DUMMY_WINDOW_ID,
        kind: WindowKind::Create as i32,
        x: 0,
        y: 0,
        width: DUMMY_WIDTH,
        height: DUMMY_HEIGHT,
        title: DUMMY_TITLE.to_string(),
    };
    send_window(&mut send, &window)
        .await
        .context("send Window")?;
    info!(
        stream = "control",
        window_id = DUMMY_WINDOW_ID,
        "sent Window CREATE"
    );

    // Frames (host -> client, datagrams; fragmented per session limit)
    let max_dg = session.max_datagram_size().unwrap_or(1200);
    info!(
        stream = "datagram",
        max_datagram_size = max_dg,
        "sending frames"
    );
    for seq in 0..frame_count {
        let payload = test_pattern(seq, frame_bytes);
        // First frame is a keyframe, the rest are deltas (dummy data,
        // but the flag is part of the wire format under test).
        let fragments = fragment_frame(DUMMY_WINDOW_ID, seq, seq == 0, &payload, max_dg)
            .context("fragment frame")?;
        for frag in &fragments {
            session.send_datagram(frag).context("send Frame datagram")?;
        }
        info!(
            stream = "datagram",
            window_id = DUMMY_WINDOW_ID,
            seq,
            fragments = fragments.len(),
            bytes = payload.len(),
            "sent Frame"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // FrameAcks (client -> host, datagrams)
    let mut acks = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while (acks.len() as u64) < frame_count {
        let dg = tokio::time::timeout_at(deadline, session.receive_datagram())
            .await
            .context("timed out waiting for FrameAck")?
            .context("receive FrameAck datagram")?;
        let ack = decode_frame_ack_datagram(&dg).context("decode FrameAck")?;
        info!(
            stream = "datagram",
            window_id = ack.window_id,
            seq = ack.seq,
            "received FrameAck"
        );
        acks.push(ack.seq);
    }

    session.close(0, "done");
    Ok(HostStats {
        hello_version: hello.version,
        frames_sent: frame_count,
        acks_received: acks,
    })
}

/// Run the dummy client against `url` (`https://127.0.0.1:<port>`),
/// pinning the host certificate by its hex SHA-256 hash.
pub async fn run_client(
    url: &str,
    cert_hash_hex: &str,
    expected_frames: u64,
) -> Result<ClientStats> {
    let cert_hash = parse_cert_hash_hex(cert_hash_hex).context("parse cert hash")?;
    let session = connect(url, cert_hash).await.context("connect")?;
    info!(remote = %session.remote_address(), stream = "control", "session established");
    let (mut send, mut recv) = session
        .open_control()
        .await
        .context("open control stream")?;

    // Hello
    let hello = Hello {
        version: PROTOCOL_VERSION,
    };
    send_hello(&mut send, &hello).await.context("send Hello")?;
    info!(stream = "control", version = hello.version, "sent Hello");

    // Window
    let (tag, body) = read_control_message(&mut recv)
        .await
        .context("read Window")?;
    let window = decode_window(tag, &body).context("decode Window")?;
    info!(stream = "control", window_id = window.window_id, width = window.width, height = window.height, title = %window.title, "received Window");

    // Frames + FrameAcks
    let mut reassembler = FrameReassembler::new();
    let mut frames_received = Vec::new();
    let mut payload_bytes_total = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while (frames_received.len() as u64) < expected_frames {
        let dg = tokio::time::timeout_at(deadline, session.receive_datagram())
            .await
            .context("timed out waiting for Frame")?
            .context("receive Frame datagram")?;
        if let Some((header, payload)) = reassembler.push(&dg).context("reassemble Frame")? {
            anyhow::ensure!(
                verify_test_pattern(header.seq, &payload),
                "test pattern mismatch for seq {}",
                header.seq
            );
            info!(
                stream = "datagram",
                window_id = header.window_id,
                seq = header.seq,
                keyframe = header.keyframe,
                bytes = payload.len(),
                "received Frame (payload verified)"
            );
            frames_received.push(header.seq);
            payload_bytes_total += payload.len();
            let ack = FrameAck {
                window_id: header.window_id,
                seq: header.seq,
            };
            session
                .send_datagram(encode_frame_ack_datagram(&ack))
                .context("send FrameAck datagram")?;
            info!(
                stream = "datagram",
                window_id = ack.window_id,
                seq = ack.seq,
                "sent FrameAck"
            );
        }
    }

    // Do not close immediately after the last FrameAck: a datagram
    // queued just before a close can be discarded with the connection.
    // The host closes once it has received every ack, so wait for
    // that; fall back to closing ourselves after a grace period.
    if tokio::time::timeout(Duration::from_secs(5), session.closed())
        .await
        .is_err()
    {
        session.close(0, "done");
    }
    Ok(ClientStats {
        hello_version: hello.version,
        window_id: window.window_id,
        window_width: window.width,
        window_height: window.height,
        frames_received,
        payload_bytes_total,
    })
}
