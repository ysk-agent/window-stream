//! In-process end-to-end test: pattern -> VP9 -> proto fragmentation ->
//! decode -> FrameSink -> FrameAck -> encoder constraint. This exercises
//! every stage except wtransport/QUIC itself (which the dummy crate's
//! roundtrip test covers, and which needs localhost UDP).
use window_stream_e2e::{run_inprocess, CountingSink};
#[test]
fn vp9_frame_path_end_to_end_inprocess() {
    const FRAMES: u64 = 20;
    let mut sink = CountingSink::default();
    let (host, client) = run_inprocess(FRAMES, &mut sink).expect("in-process e2e");
    let expected: Vec<u64> = (0..FRAMES).collect();
    assert_eq!(host.frames_sent, FRAMES);
    assert!(host.keyframes_sent >= 1, "first frame must be a keyframe");
    assert_eq!(client.frames_reassembled, expected);
    assert_eq!(
        client.frames_decoded, expected,
        "every frame decodes, in order"
    );
    assert_eq!(client.decode_errors, 0);
    assert_eq!(client.acks_sent, expected);
    assert_eq!(
        host.acks_received, expected,
        "every ack reaches the encoder constraint"
    );
    assert_eq!(sink.frames_submitted, FRAMES);
    // VP9 is lossy, but the moving-rectangle pattern must survive well:
    // worst decoded frame still > 20 dB luma PSNR vs the source pattern.
    assert!(
        client.min_luma_psnr_db_x10 > 200,
        "min PSNR {} dB",
        client.min_luma_psnr_db_x10 as f64 / 10.0
    );
}
