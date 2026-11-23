//! perf-cycle: per-stage measurement of the real frame path, in-process.
//!
//! Path measured (identical codec + proto code to the network path):
//!   source frame (seq-stamped, `Instant` taken as "frame generated")
//!     -> Vp9Encoder::encode            [encode]
//!     -> proto::fragment_frame         [fragment, max_datagram = 1200 in-process budget]
//!     -> proto::FrameReassembler       [reassemble, datagrams fed in order]
//!     -> Vp9Decoder::decode            [decode]
//!     -> sink: plane copy + luma checksum, frame counts as displayable [sink]
//!   E2E latency = sink-complete time - frame-generated time, per seq.
//!
//! NOT measured: WebTransport/QUIC over UDP (this VM cannot reliably run
//! the localhost UDP path — see the investigation report), OS capture,
//! GPU compositing. Those stages are absent, not zero: E2E here is the
//! codec+proto pipeline latency only.
//!
//! Modes: unpaced (as fast as possible; throughput ceiling of the serial
//! pipeline — the real pipeline overlaps stages on separate threads, so
//! serial fps is a lower bound) — CPU is read from /proc/self/stat
//! (utime+stime) and reported relative to one core and to wall time.
//!
//! Usage: perf-cycle [--frames 90] [--cpu-used 6] [--enc-threads 0]
//!                   [--dec-threads 0] [--label baseline] [--out results.jsonl]

use std::time::{Duration, Instant};

use anyhow::Result;
use codec::pattern::moving_rectangle;
use codec::{
    luma_psnr, DecoderConfig, EncoderConfig, I420Frame, VideoDecoder, VideoEncoder, Vp9Decoder,
    Vp9Encoder,
};
use window_stream_proto::{fragment_frame, FrameReassembler};

const MAX_DATAGRAM: usize = 1200; // in-process budget only; network path uses QUIC's detected limit
const NOMINAL_FPS: u32 = 30;

#[derive(Clone, Copy, PartialEq)]
enum Content {
    Static, // near-static desktop: same picture, only an 8x8 cursor block creeps
    Motion, // codec::pattern::moving_rectangle (bouncing quarter-frame rectangle)
}

fn static_frame(w: u32, h: u32, seq: u64) -> codec::Result<I420Frame> {
    let base = moving_rectangle(w, h, 0)?;
    let mut y = base.y().to_vec();
    // 8x8 bright cursor block, moves 1 px/frame horizontally near the top-left.
    let cx = 16 + (seq as usize % 200);
    let cy = 16usize;
    for row in cy..cy + 8 {
        for col in cx..cx + 8 {
            y[row * w as usize + col] = 235;
        }
    }
    I420Frame::new(w, h, seq, y, base.u().to_vec(), base.v().to_vec())
}

fn make_sources(kind: Content, w: u32, h: u32, n: u64) -> Vec<I420Frame> {
    (0..n)
        .map(|i| match kind {
            Content::Motion => moving_rectangle(w, h, i).unwrap(),
            Content::Static => static_frame(w, h, i).unwrap(),
        })
        .collect()
}

fn cpu_time() -> Duration {
    // /proc/self/stat: fields 14 (utime) and 15 (stime), in clock ticks (100 Hz on Linux x86_64).
    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let after = s.rsplit(')').next().unwrap_or("");
    let f: Vec<&str> = after.split_whitespace().collect();
    // f[0] is field 3 (state); utime = field 14 -> f[11], stime = field 15 -> f[12]
    let ticks: u64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0)
        + f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
    Duration::from_millis(ticks * 10)
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

fn stats(mut v: Vec<f64>) -> (f64, f64, f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = v.iter().sum::<f64>() / v.len().max(1) as f64;
    (
        mean,
        pct(&v, 0.50),
        pct(&v, 0.95),
        *v.last().unwrap_or(&0.0),
    )
}

struct Args {
    frames: u64,
    cpu_used: i32,
    /// None = use the codec crate's own defaults (this is what the
    /// "improved" run measures: the shipped configuration, no overrides).
    enc_threads: Option<u32>,
    dec_threads: Option<u32>,
    label: String,
    out: Option<String>,
}

fn parse_args() -> Args {
    let mut a = Args {
        frames: 90,
        cpu_used: 6,
        enc_threads: None,
        dec_threads: None,
        label: "run".into(),
        out: None,
    };
    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i + 1 < argv.len() || i < argv.len() {
        if i >= argv.len() {
            break;
        }
        let (k, v) = (argv[i].as_str(), argv.get(i + 1).cloned());
        match k {
            "--frames" => a.frames = v.unwrap().parse().unwrap(),
            "--cpu-used" => a.cpu_used = v.unwrap().parse().unwrap(),
            "--enc-threads" => a.enc_threads = Some(v.unwrap().parse().unwrap()),
            "--dec-threads" => a.dec_threads = Some(v.unwrap().parse().unwrap()),
            "--label" => a.label = v.unwrap(),
            "--out" => a.out = v,
            _ => break,
        }
        i += 2;
    }
    a
}

fn run_scenario(args: &Args, kind: Content, w: u32, h: u32, bitrate: u32) -> Result<String> {
    let kind_name = match kind {
        Content::Static => "static",
        Content::Motion => "motion",
    };
    let warmup = 5u64;
    let total = warmup + args.frames;
    let sources = make_sources(kind, w, h, total);

    let mut cfg = EncoderConfig::realtime(w, h, bitrate);
    cfg.cpu_used = args.cpu_used;
    if let Some(t) = args.enc_threads {
        cfg.threads = t;
    }
    let enc_threads = cfg.threads;
    let mut enc = Vp9Encoder::new(cfg)?;
    let mut dec_cfg = DecoderConfig::default();
    if let Some(t) = args.dec_threads {
        dec_cfg.threads = t;
    }
    let dec_threads = dec_cfg.threads;
    let mut dec = Vp9Decoder::new(dec_cfg)?;

    let mut enc_ms = Vec::new();
    let mut frag_ms = Vec::new();
    let mut dec_ms = Vec::new();
    let mut e2e_ms = Vec::new();
    let mut enc_bytes = 0usize;
    let mut datagram_bytes = 0usize;
    let mut datagrams = 0usize;
    let mut psnrs = Vec::new();
    let mut decoded = 0u64;

    let cpu0 = cpu_time();
    let wall0 = Instant::now();

    for (idx, frame) in sources.iter().enumerate() {
        let t_gen = Instant::now(); // frame "generated" (source ready, seq stamped)
        let t0 = Instant::now();
        let packets = enc.encode(frame)?;
        let t1 = Instant::now();
        let mut sink_checksum: u64 = 0;
        for pkt in &packets {
            let dgrams = fragment_frame(1, pkt.seq, pkt.is_keyframe, &pkt.data, MAX_DATAGRAM)?;
            let t2 = Instant::now();
            let mut reasm = FrameReassembler::new();
            let mut full = None;
            for d in &dgrams {
                if let Some((hdr, payload)) = reasm.push(d)? {
                    full = Some((hdr, payload));
                }
            }
            let (hdr, payload) = full.expect("reassembled");
            assert_eq!(hdr.seq, pkt.seq);
            let t3 = Instant::now();
            let epkt = codec::EncodedFrame {
                seq: hdr.seq,
                is_keyframe: hdr.keyframe,
                data: payload.to_vec(),
            };
            let frames_out = dec.decode(&epkt)?;
            let t4 = Instant::now();
            for f in &frames_out {
                // sink: plane copy (what to_client_frame does) + luma checksum
                let copy = (f.y().to_vec(), f.u().to_vec(), f.v().to_vec());
                for b in &copy.0 {
                    sink_checksum = sink_checksum.wrapping_mul(31).wrapping_add(*b as u64);
                }
                if idx as u64 >= warmup {
                    let p = luma_psnr(&sources[f.seq() as usize], f);
                    if p.is_finite() {
                        psnrs.push(p);
                    }
                    decoded += 1;
                }
            }
            if idx as u64 >= warmup {
                frag_ms.push((t3 - t1).as_secs_f64() * 1e3);
                dec_ms.push((t4 - t3).as_secs_f64() * 1e3);
                enc_bytes += pkt.data.len();
                datagram_bytes += dgrams.iter().map(|d| d.len()).sum::<usize>();
                datagrams += dgrams.len();
            }
            let _ = t2;
            let _ = sink_checksum;
        }
        if idx as u64 >= warmup {
            enc_ms.push((t1 - t0).as_secs_f64() * 1e3);
            e2e_ms.push((Instant::now() - t_gen).as_secs_f64() * 1e3);
        }
        // keep the ack chain live, as the real client does (FrameAck per decoded frame)
        enc.set_reference_constraint(Some(frame.seq()));
    }

    let wall = wall0.elapsed();
    let cpu = cpu_time() - cpu0;
    let n = args.frames as f64;
    let (enc_m, enc_p50, enc_p95, enc_max) = stats(enc_ms);
    let (dec_m, dec_p50, dec_p95, dec_max) = stats(dec_ms);
    let (e2e_m, e2e_p50, e2e_p95, e2e_max) = stats(e2e_ms);
    let (frag_m, _, _, _) = stats(frag_ms);
    let content_secs = n / NOMINAL_FPS as f64;
    let kbps = enc_bytes as f64 * 8.0 / content_secs / 1000.0;
    let wire_kbps = datagram_bytes as f64 * 8.0 / content_secs / 1000.0;
    let fps_serial = n / wall.as_secs_f64();
    let cpu_pct_one_core = cpu.as_secs_f64() / wall.as_secs_f64() * 100.0;
    let psnr_mean = if psnrs.is_empty() {
        0.0
    } else {
        psnrs.iter().sum::<f64>() / psnrs.len() as f64
    };

    println!(
        "[{}] {}x{} {} target={}kbps cpu_used={} enc_threads={} dec_threads={} frames={}",
        args.label, w, h, kind_name, bitrate, args.cpu_used, enc_threads, dec_threads, args.frames
    );
    println!("  encode  mean {enc_m:.2} ms  p50 {enc_p50:.2}  p95 {enc_p95:.2}  max {enc_max:.2}");
    println!("  frag+reasm mean {frag_m:.3} ms   decode mean {dec_m:.2} ms  p50 {dec_p50:.2}  p95 {dec_p95:.2}  max {dec_max:.2}");
    println!("  e2e(serial,in-process) mean {e2e_m:.2} ms  p50 {e2e_p50:.2}  p95 {e2e_p95:.2}  max {e2e_max:.2}");
    println!(
        "  serial_fps {fps_serial:.1}  bitrate {kbps:.0} kbps (wire {wire_kbps:.0}, overhead {:.1}%)  mean_pkt {} B  datagrams/frame {:.1}",
        (wire_kbps / kbps.max(1.0) - 1.0) * 100.0,
        enc_bytes / args.frames as usize,
        datagrams as f64 / n
    );
    println!(
        "  decoded {decoded}/{}  luma_PSNR_mean {:.2} dB  process_CPU {:.1}% of one core (wall {:.2}s, cpu {:.2}s)",
        args.frames,
        psnr_mean,
        cpu_pct_one_core,
        wall.as_secs_f64(),
        cpu.as_secs_f64()
    );
    if !enc.control_warnings.is_empty() {
        println!("  control_warnings: {:?}", enc.control_warnings);
    }

    Ok(format!(
        "{{\"label\":\"{}\",\"w\":{},\"h\":{},\"content\":\"{}\",\"target_kbps\":{},\"cpu_used\":{},\"enc_threads\":{},\"dec_threads\":{},\"frames\":{},\"encode_ms_mean\":{:.3},\"encode_ms_p50\":{:.3},\"encode_ms_p95\":{:.3},\"encode_ms_max\":{:.3},\"decode_ms_mean\":{:.3},\"decode_ms_p50\":{:.3},\"decode_ms_p95\":{:.3},\"decode_ms_max\":{:.3},\"frag_reasm_ms_mean\":{:.4},\"e2e_ms_mean\":{:.3},\"e2e_ms_p50\":{:.3},\"e2e_ms_p95\":{:.3},\"e2e_ms_max\":{:.3},\"serial_fps\":{:.2},\"bitrate_kbps\":{:.1},\"wire_kbps\":{:.1},\"mean_packet_bytes\":{},\"datagrams_per_frame\":{:.2},\"decoded\":{},\"psnr_db_mean\":{:.2},\"cpu_pct_one_core\":{:.1},\"wall_s\":{:.3}}}",
        args.label, w, h, kind_name, bitrate, args.cpu_used, enc_threads, dec_threads,
        args.frames, enc_m, enc_p50, enc_p95, enc_max, dec_m, dec_p50, dec_p95, dec_max, frag_m,
        e2e_m, e2e_p50, e2e_p95, e2e_max, fps_serial, kbps, wire_kbps,
        enc_bytes / args.frames as usize, datagrams as f64 / n, decoded, psnr_mean, cpu_pct_one_core,
        wall.as_secs_f64()
    ))
}

fn main() -> Result<()> {
    let args = parse_args();
    let mut lines = Vec::new();
    for (w, h, bitrate) in [(1280u32, 720u32, 2500u32), (1920, 1080, 5000)] {
        for kind in [Content::Static, Content::Motion] {
            lines.push(run_scenario(&args, kind, w, h, bitrate)?);
        }
    }
    if let Some(out) = &args.out {
        let mut s = lines.join("\n");
        s.push('\n');
        std::fs::write(out, s)?;
        println!("wrote {out}");
    }
    Ok(())
}
