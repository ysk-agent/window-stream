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
    for i in 0..10 {
        let out = enc.encode(&moving_rectangle(W, H, i).unwrap()).unwrap();
        assert_eq!(out[0].is_keyframe, i == 0, "frame {i}");
    }
    // Simulate the protocol event: a frame was lost, acks stall.
    enc.set_reference_constraint(Some(4));
    assert_eq!(enc.last_acked_seq(), Some(4));
    enc.invalidate_reference();
    let out = enc.encode(&moving_rectangle(W, H, 10).unwrap()).unwrap();
    assert!(
        out[0].is_keyframe,
        "frame after invalidation must be a keyframe"
    );
    // And that keyframe must restart a decodable stream on its own: a
    // fresh decoder starting at the new keyframe decodes it fine (it just
    // cannot decode the pre-loss deltas it never saw — by design).
    let mut dec = Vp9Decoder::new(DecoderConfig::default()).unwrap();
    let frames = dec
        .decode(&out[0])
        .expect("keyframe must decode standalone");
    assert_eq!(frames.len(), 1);
    let (_, _, invalidations) = enc.stats();
    assert_eq!(invalidations, 1);
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
