//! Round-trip tests: synthetic pattern -> VP9 encode -> VP9 decode ->
//! PSNR against the source, plus IVF dump/parse validation and the
//! reference-invalidation behaviour (forced keyframe after loss).

use codec::ivf::vp9_is_keyframe;
use codec::pattern::moving_rectangle;
use codec::{
    DecoderConfig, EncoderConfig, IvfReader, IvfWriter, ReferenceMode, VideoDecoder, VideoEncoder,
    Vp9Decoder, Vp9Encoder,
};

const W: u32 = 640;
const H: u32 = 360;
const FRAMES: u64 = 45;
/// Luma PSNR floor for the moving-rectangle pattern at 1.5 Mbit/s.
/// Measured values are printed by the test; the floor is set well below
/// them so the test catches real regressions, not noise.
const PSNR_FLOOR_DB: f64 = 30.0;

fn encode_pattern(
    mode: ReferenceMode,
    frames: u64,
) -> (Vec<codec::EncodedFrame>, Vec<codec::I420Frame>) {
    let mut cfg = EncoderConfig::realtime(W, H, 1500);
    cfg.reference_mode = mode;
    let mut enc = Vp9Encoder::new(cfg).expect("encoder init");
    assert!(
        enc.control_warnings.is_empty(),
        "unexpected control warnings: {:?}",
        enc.control_warnings
    );
    let mut packets = Vec::new();
    let mut sources = Vec::new();
    for i in 0..frames {
        let frame = moving_rectangle(W, H, i).unwrap();
        let out = enc.encode(&frame).expect("encode");
        assert_eq!(out.len(), 1, "realtime encode must emit 1 packet/frame");
        packets.extend(out);
        sources.push(frame);
    }
    packets.extend(enc.flush().expect("flush"));
    (packets, sources)
}

#[test]
fn roundtrip_psnr_and_keyframes() {
    let (packets, sources) = encode_pattern(ReferenceMode::AckGated, FRAMES);
    assert_eq!(packets.len() as u64, FRAMES);
    assert!(packets[0].is_keyframe, "first frame must be a keyframe");
    assert!(
        packets[1..].iter().all(|p| !p.is_keyframe),
        "no periodic keyframes expected in AckGated mode"
    );
    // The keyframe flag must agree with an independent parse of the VP9
    // uncompressed header.
    for p in &packets {
        assert_eq!(p.is_keyframe, vp9_is_keyframe(&p.data), "seq {}", p.seq);
    }

    let mut dec = Vp9Decoder::new(DecoderConfig::default()).expect("decoder init");
    let mut psnr_sum = 0.0;
    let mut decoded = 0usize;
    for (packet, source) in packets.iter().zip(sources.iter()) {
        let frames = dec.decode(packet).expect("decode");
        assert_eq!(frames.len(), 1);
        let f = &frames[0];
        assert_eq!((f.width(), f.height()), (W, H));
        assert_eq!(f.seq(), source.seq());
        let psnr = codec::luma_psnr(source, f);
        psnr_sum += psnr;
        decoded += 1;
    }
    let mean_psnr = psnr_sum / decoded as f64;
    eprintln!("roundtrip: {decoded} frames, mean luma PSNR = {mean_psnr:.2} dB");
    println!("roundtrip: {decoded} frames, mean luma PSNR = {mean_psnr:.2} dB");
    assert!(
        mean_psnr > PSNR_FLOOR_DB,
        "mean PSNR {mean_psnr:.2} dB below floor {PSNR_FLOOR_DB} dB"
    );
}

#[test]
fn invalidate_reference_forces_keyframe() {
    let mut enc = Vp9Encoder::new(EncoderConfig::realtime(W, H, 1500)).unwrap();
    let mut dec = Vp9Decoder::new(DecoderConfig::default()).unwrap();
    let mut packets = Vec::new();
    for i in 0..10 {
        let out = enc.encode(&moving_rectangle(W, H, i).unwrap()).unwrap();
        assert_eq!(out[0].is_keyframe, i == 0, "frame {i}");
        packets.push(out.into_iter().next().unwrap());
    }
    // The same decoder decodes frames 0..9, as in a live session.
    let mut sources = Vec::new();
    for p in &packets {
        let frames = dec.decode(p).expect("decode pre-loss frames");
        assert_eq!(frames.len(), 1);
        sources.push(frames.into_iter().next().unwrap());
    }
    assert_eq!(dec.frames_decoded(), 10);
    // Simulate the protocol event: frames 10..14 are lost on the wire
    // (never fed to the decoder) and acks stall at 9.
    enc.set_reference_constraint(Some(9));
    assert_eq!(enc.last_acked_seq(), Some(9));
    enc.invalidate_reference();
    let kf = enc
        .encode(&moving_rectangle(W, H, 15).unwrap())
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(
        kf.is_keyframe,
        "frame after invalidation must be a keyframe"
    );
    // Resynchronisation on the SAME decoder: the forced keyframe plus
    // the next delta must decode, even though the decoder missed the
    // in-between deltas. (A fresh decoder would prove nothing about
    // resync — it has no stale reference chain to heal.)
    let frames = dec
        .decode(&kf)
        .expect("keyframe must decode on the stalled decoder");
    assert_eq!(frames.len(), 1);
    let psnr = codec::luma_psnr(&moving_rectangle(W, H, 15).unwrap(), &frames[0]);
    assert!(
        psnr > PSNR_FLOOR_DB,
        "post-keyframe PSNR {psnr:.2} dB below floor {PSNR_FLOOR_DB} dB"
    );
    let delta = enc
        .encode(&moving_rectangle(W, H, 16).unwrap())
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(!delta.is_keyframe, "frame after the keyframe is a delta");
    let frames = dec
        .decode(&delta)
        .expect("delta after keyframe must decode");
    assert_eq!(frames.len(), 1);
    let psnr = codec::luma_psnr(&moving_rectangle(W, H, 16).unwrap(), &frames[0]);
    assert!(
        psnr > PSNR_FLOOR_DB,
        "post-keyframe delta PSNR {psnr:.2} dB below floor {PSNR_FLOOR_DB} dB"
    );
    // And the same keyframe also restarts a fresh decoder (it is
    // independently decodable, by design).
    let mut fresh = Vp9Decoder::new(DecoderConfig::default()).unwrap();
    assert_eq!(fresh.decode(&kf).unwrap().len(), 1);
    let (_, _, invalidations) = enc.stats();
    assert_eq!(invalidations, 1);
}

#[test]
fn ack_stall_forces_keyframe() {
    // Acks stuck at frame 0 while encoding continues: the encoder must
    // escalate to a keyframe once the lag exceeds the threshold (30).
    let mut enc = Vp9Encoder::new(EncoderConfig::realtime(W, H, 1500)).unwrap();
    let first = enc.encode(&moving_rectangle(W, H, 0).unwrap()).unwrap();
    assert!(first[0].is_keyframe);
    enc.set_reference_constraint(Some(0));
    let mut keyframe_seqs = Vec::new();
    for i in 1..=40u64 {
        let out = enc.encode(&moving_rectangle(W, H, i).unwrap()).unwrap();
        if out[0].is_keyframe {
            keyframe_seqs.push(i);
        }
    }
    assert_eq!(
        keyframe_seqs,
        vec![31],
        "stalled acks must force exactly one keyframe at seq 31 (lag 31 > 30), got {keyframe_seqs:?}"
    );

    // Control: acks advancing with every frame must not force any
    // keyframe before the (disabled) periodic interval.
    let mut enc = Vp9Encoder::new(EncoderConfig::realtime(W, H, 1500)).unwrap();
    for i in 0..40u64 {
        enc.set_reference_constraint(Some(i.saturating_sub(1)));
        let out = enc.encode(&moving_rectangle(W, H, i).unwrap()).unwrap();
        assert_eq!(out[0].is_keyframe, i == 0, "frame {i} with live acks");
    }
}

#[test]
fn bitrate_change_keeps_keyframe_interval() {
    // Regression: set_bitrate_kbps used to rebuild the libvpx config
    // from defaults and silently drop kf_mode/kf_max_dist, so periodic
    // keyframes vanished after a bitrate change.
    let mut cfg = EncoderConfig::realtime(W, H, 1500);
    cfg.keyframe_interval = 10;
    let mut enc = Vp9Encoder::new(cfg).unwrap();
    let mut keyframe_seqs = Vec::new();
    for i in 0..=30u64 {
        if i == 5 {
            enc.set_bitrate_kbps(800).expect("bitrate change");
        }
        let out = enc.encode(&moving_rectangle(W, H, i).unwrap()).unwrap();
        assert_eq!(out.len(), 1);
        if out[0].is_keyframe {
            keyframe_seqs.push(i);
        }
    }
    assert_eq!(
        keyframe_seqs,
        vec![0, 10, 20, 30],
        "periodic keyframes must survive a bitrate change at frame 5, got {keyframe_seqs:?}"
    );
}

#[test]
fn ack_gated_never_refreshes_golden_or_altref() {
    use codec::ivf::vp9_header_info;
    // Bitstream-level check that NO_UPD_GF / NO_UPD_ARF take effect:
    // for every inter frame in AckGated mode, the refresh_frame_flags
    // bits for the buffers holding the GOLDEN (ref_frame_idx[1]) and
    // ALTREF (ref_frame_idx[2]) references must be clear.
    let (packets, _) = encode_pattern(ReferenceMode::AckGated, 30);
    let mut inter = 0usize;
    for p in &packets {
        let Some(info) = vp9_header_info(&p.data) else {
            panic!("unparseable VP9 header at seq {}", p.seq);
        };
        if info.is_keyframe {
            continue;
        }
        inter += 1;
        let flags = info.refresh_frame_flags.expect("inter frame refresh flags");
        let idx = info.ref_frame_idx.expect("inter frame ref indices");
        for (role, buf) in [("golden", idx[1]), ("alt-ref", idx[2])] {
            assert_eq!(
                (flags >> buf) & 1,
                0,
                "AckGated inter frame seq {} refreshes its {role} buffer {buf} (flags 0x{flags:02x}, ref idx {idx:?})",
                p.seq
            );
        }
    }
    assert!(inter >= 20, "expected >= 20 inter frames, got {inter}");

    // Control: the same parse on Normal mode must see golden/alt-ref
    // refreshes — otherwise the parser could be returning false
    // negatives and the AckGated assertion would prove nothing.
    let (packets, _) = encode_pattern(ReferenceMode::Normal, 30);
    let mut refreshed = 0usize;
    for p in &packets {
        let Some(info) = vp9_header_info(&p.data) else {
            continue;
        };
        if info.is_keyframe {
            continue;
        }
        let (Some(flags), Some(idx)) = (info.refresh_frame_flags, info.ref_frame_idx) else {
            continue;
        };
        if (flags >> idx[1]) & 1 == 1 || (flags >> idx[2]) & 1 == 1 {
            refreshed += 1;
        }
    }
    assert!(
        refreshed > 0,
        "control failed: Normal mode never refreshed golden/alt-ref in 30 frames, so the parser cannot be trusted"
    );
    eprintln!(
        "refresh-bit control: Normal mode refreshed golden/alt-ref on {refreshed} inter frames"
    );
}

#[test]
fn luma_psnr_dimension_mismatch_is_nan() {
    let a = codec::I420Frame::blank(64, 64, 0).unwrap();
    let b = codec::I420Frame::blank(32, 32, 0).unwrap();
    assert!(codec::luma_psnr(&a, &b).is_nan());
    assert!(codec::luma_psnr(&a, &a).is_infinite());
}

#[test]
fn keyframes_only_mode() {
    let (packets, _) = encode_pattern(ReferenceMode::KeyframesOnly, 8);
    assert_eq!(packets.len(), 8);
    assert!(
        packets.iter().all(|p| p.is_keyframe),
        "every frame must be a keyframe in KeyframesOnly mode"
    );
    // Any single packet decodes standalone.
    for p in &packets {
        let mut dec = Vp9Decoder::new(DecoderConfig::default()).unwrap();
        assert_eq!(dec.decode(p).unwrap().len(), 1);
    }
}

#[test]
fn ivf_dump_roundtrip() {
    let (packets, _) = encode_pattern(ReferenceMode::AckGated, 20);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pattern.ivf");
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = IvfWriter::new(file, W, H, 30).unwrap();
        for p in &packets {
            writer.write_frame(p).unwrap();
        }
        assert_eq!(writer.frames_written(), 20);
        writer.finish().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[0..4], b"DKIF");
    assert_eq!(&bytes[8..12], b"VP90");

    let mut reader = IvfReader::new(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(reader.header.width, W);
    assert_eq!(reader.header.height, H);
    assert_eq!(reader.header.fps, 30);
    assert_eq!(reader.header.frame_count, 20);
    let mut back = Vec::new();
    while let Some(f) = reader.next_frame().unwrap() {
        back.push(f);
    }
    assert_eq!(back.len(), packets.len());
    for (a, b) in packets.iter().zip(back.iter()) {
        assert_eq!(a.data, b.data);
        assert_eq!(a.seq, b.seq);
        assert_eq!(a.is_keyframe, b.is_keyframe);
    }
    // The dumped stream must also decode end-to-end (it is what ffplay
    // would consume).
    let mut dec = Vp9Decoder::new(DecoderConfig::default()).unwrap();
    for f in &back {
        assert_eq!(dec.decode(f).unwrap().len(), 1);
    }
}

#[test]
fn decoder_rejects_garbage() {
    let mut dec = Vp9Decoder::new(DecoderConfig::default()).unwrap();
    let junk = codec::EncodedFrame {
        data: vec![0xAA; 64],
        is_keyframe: false,
        seq: 0,
    };
    assert!(dec.decode(&junk).is_err());
}
