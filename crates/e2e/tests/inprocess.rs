//! In-process end-to-end test: pattern -> VP9 -> proto fragmentation ->
//! decode -> FrameSink -> FrameAck -> encoder constraint. This exercises
//! every stage except wtransport/QUIC itself (which the dummy crate's
//! roundtrip test covers, and which needs localhost UDP).
use window_stream_e2e::{
    run_inprocess, run_inprocess_with_source, CapturedFrameSource, CountingSink,
};
use window_stream_platform::{CapturedFrame, PixelFormat, WindowId};

/// Render frame `seq` of a moving-rectangle pattern directly in BGRA,
/// the layout ScreenCaptureKit delivers. The content mirrors the I420
/// pattern (gradient background, bright rectangle bouncing
/// diagonally) so the decoded frames have something to check against
/// — but the check is against the *converted* I420 frames, not the
/// pattern function: this test proves the BGRA -> I420 -> VP9 ->
/// decode -> sink wiring, while conversion accuracy itself is covered
/// by the platform crate's unit tests.
fn bgra_pattern_frame(width: u32, height: u32, seq: u64) -> CapturedFrame {
    let (w, h) = (width as usize, height as usize);
    let mut data = vec![0u8; w * h * 4];
    let rw = (w / 4).max(2);
    let rh = (h / 4).max(2);
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
    let x0 = bounce(seq as usize * 7, w - rw);
    let y0 = bounce(seq as usize * 5, h - rh);
    for row in 0..h {
        for col in 0..w {
            let in_rect = col >= x0 && col < x0 + rw && row >= y0 && row < y0 + rh;
            let (r, g, b) = if in_rect {
                (235, 235, 235)
            } else {
                // Gradient background in RGB space.
                (
                    (32 + col * 96 / w.max(1)) as u8,
                    (64 + row * 64 / h.max(1)) as u8,
                    128u8,
                )
            };
            let px = &mut data[(row * w + col) * 4..(row * w + col) * 4 + 4];
            px[0] = b;
            px[1] = g;
            px[2] = r;
            px[3] = 255;
        }
    }
    CapturedFrame {
        window_id: WindowId(7),
        width,
        height,
        format: PixelFormat::Bgra8888,
        strides: vec![w * 4],
        data,
        timestamp: None,
        sequence: seq,
    }
}

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

#[test]
fn bgra_captured_frames_end_to_end_inprocess() {
    // Same pipeline as the pattern test, but the frames arrive as
    // BGRA CapturedFrames through the channel-fed source — the exact
    // conversion + encode + send path RealCaptureSource uses on a Mac.
    const FRAMES: u64 = 10;
    const W: u32 = 64;
    const H: u32 = 48;
    let (tx, source) = CapturedFrameSource::channel(7, "bgra test window");
    // The expected I420 frames, converted the same way the source
    // converts them, for the PSNR check on the last decoded frame.
    let expected: Vec<codec::I420Frame> = (0..FRAMES)
        .map(|seq| {
            let bgra = bgra_pattern_frame(W, H, seq);
            window_stream_e2e::captured_frame_to_i420(&bgra).expect("convert test frame")
        })
        .collect();
    std::thread::spawn(move || {
        for seq in 0..FRAMES {
            let frame = bgra_pattern_frame(W, H, seq);
            // Blocking send: the depth-1 channel holds one frame while
            // the host encodes the previous one; the feeder waits.
            tx.send(frame).expect("feed BGRA frame");
        }
    });
    let mut sink = CountingSink::default();
    let (host, client) =
        run_inprocess_with_source(FRAMES, &mut sink, source).expect("in-process BGRA e2e");
    let expected_seqs: Vec<u64> = (0..FRAMES).collect();
    assert_eq!(host.frames_sent, FRAMES);
    assert!(host.keyframes_sent >= 1, "first frame must be a keyframe");
    assert_eq!(client.frames_reassembled, expected_seqs);
    assert_eq!(client.frames_decoded, expected_seqs);
    assert_eq!(client.decode_errors, 0);
    assert_eq!(client.acks_sent, expected_seqs);
    assert_eq!(host.acks_received, expected_seqs);
    assert_eq!(sink.frames_submitted, FRAMES);
    // Real captured content has no deterministic reference, so the
    // in-process PSNR stays unset — the test checks it explicitly
    // instead of against the pattern.
    assert_eq!(client.min_luma_psnr_db_x10, 0);
    // ...but the last decoded frame must still look like the last
    // converted source frame (BGRA -> I420 -> VP9 round trip).
    let last = sink.last_frame.as_ref().expect("last decoded frame");
    assert_eq!((last.width, last.height), (W, H));
    let decoded = codec::I420Frame::new(
        W,
        H,
        FRAMES - 1,
        last.y.clone(),
        last.u.clone(),
        last.v.clone(),
    )
    .expect("decoded frame is valid I420");
    let psnr = codec::luma_psnr(expected.last().unwrap(), &decoded);
    assert!(
        psnr > 20.0,
        "last BGRA frame must survive the round trip (>20 dB luma PSNR), got {psnr:.1} dB"
    );
}

/// On a non-macOS target the platform backend is the stub, which has
/// no windows: starting real capture must fail fast with a clear
/// error, never by producing fake frames.
#[cfg(not(target_os = "macos"))]
#[test]
fn real_capture_source_fails_fast_on_stub() {
    match window_stream_e2e::RealCaptureSource::start(1, 30) {
        Ok(_) => panic!("stub backend must not start a capture"),
        Err(e) => assert!(e.to_string().contains("not found"), "unexpected error: {e}"),
    }
}

/// Regression test for the 2026-11-23 Mac finding: a real capture
/// source numbers frames by its capture sequence, and frames dropped
/// at the depth-1 channel leave gaps (0, 37, 74, ...) in the seqs
/// the host sends. Those gaps are NOT packet loss — the old
/// ack-to-ack gap rule invalidated the reference on nearly every
/// ack (28 invalidations / 30 frames on the Mac). With sent-aware
/// tracking, consecutive acks of actually-sent frames must produce
/// zero invalidations and a single keyframe.
#[test]
fn sparse_capture_seqs_do_not_invalidate_inprocess() {
    const SEQS: [u64; 5] = [0, 37, 74, 111, 148];
    const W: u32 = 64;
    const H: u32 = 48;
    let (tx, source) = CapturedFrameSource::channel(7, "sparse seq window");
    std::thread::spawn(move || {
        for seq in SEQS {
            let frame = bgra_pattern_frame(W, H, seq);
            tx.send(frame).expect("feed BGRA frame");
        }
    });
    let mut sink = CountingSink::default();
    let (host, client) = run_inprocess_with_source(SEQS.len() as u64, &mut sink, source)
        .expect("in-process sparse-seq e2e");
    assert_eq!(host.frames_sent, SEQS.len() as u64);
    assert_eq!(client.frames_decoded, SEQS.to_vec());
    assert_eq!(client.decode_errors, 0);
    assert_eq!(host.acks_received, SEQS.to_vec());
    assert_eq!(
        host.invalidations, 0,
        "capture-seq gaps between sent frames are not loss"
    );
    assert_eq!(
        host.keyframes_sent, 1,
        "only the first frame should be a keyframe"
    );
}
