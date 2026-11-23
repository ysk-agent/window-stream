//! Integration test: dummy host <-> dummy client over a real local
//! WebTransport session.
//!
//! Verifies the full v1 round trip:
//! Hello -> Window -> Frame (datagram, fragmented) -> FrameAck.

use window_stream_dummy::{run_client, start_host, DEFAULT_FRAME_BYTES, DUMMY_HEIGHT, DUMMY_WIDTH};

#[tokio::test]
async fn hello_window_frame_frameack_roundtrip() {
    const FRAMES: u64 = 5;
    let host = start_host(FRAMES, DEFAULT_FRAME_BYTES)
        .await
        .expect("host starts");
    let url = format!("https://127.0.0.1:{}", host.addr.port());

    let client = run_client(&url, &host.cert_hash_hex, FRAMES)
        .await
        .expect("client completes the round trip");
    let host_stats = host
        .task
        .await
        .expect("host task joins")
        .expect("host completes the round trip");

    // Hello
    assert_eq!(host_stats.hello_version, 1);
    assert_eq!(client.hello_version, 1);
    // Window
    assert_eq!(client.window_id, 1);
    assert_eq!(client.window_width, DUMMY_WIDTH);
    assert_eq!(client.window_height, DUMMY_HEIGHT);
    // Frames (all of them, in order, payload verified inside run_client)
    assert_eq!(client.frames_received, vec![0, 1, 2, 3, 4]);
    assert_eq!(
        client.payload_bytes_total,
        FRAMES as usize * DEFAULT_FRAME_BYTES
    );
    // FrameAcks (host saw an ack for every frame)
    assert_eq!(host_stats.frames_sent, FRAMES);
    assert_eq!(host_stats.acks_received, vec![0, 1, 2, 3, 4]);
}
