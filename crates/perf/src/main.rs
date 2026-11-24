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
//! Modes (cycle 2):
//! * unpaced (default): as fast as possible; throughput ceiling of the
//!   serial pipeline — the real pipeline overlaps stages on separate
//!   threads, so serial fps is a lower bound.
//! * `--paced`: producer thread emits at exactly 30 fps into a depth-1
//!   slot (a newer frame replaces a queued stale one, which is counted
//!   as dropped — the protocol's own queue rule); a consumer thread runs
//!   the full path. Latency is measured from the frame's *scheduled*
//!   generation instant to sink completion, so queue wait is included.
//! * `--multi`: N encoder threads in parallel (N windows on one host),
//!   encode-only, reporting aggregate throughput and process CPU — the
//!   per-window thread-budget experiment.
//! * `--sweep`: cpu_used 5..=9 probe at 1080p (motion + noise) and 720p
//!   (motion), recording per-frame packet sizes — the cpu_used 8
//!   bitrate-collapse investigation.
//!
//! CPU is read from /proc/self/stat on Linux and getrusage on macOS,
//! reported relative to one core and to wall time.
//!
//! Usage: perf-cycle [--frames 90] [--cpu-used 6] [--enc-threads N]
//!                   [--dec-threads N] [--tile-columns L] [--label run]
//!                   [--paced] [--multi] [--windows 3] [--sweep]
//!                   [--out results.jsonl]

use std::sync::{Arc, Condvar, Mutex};
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
    Noise,  // deterministic high-detail texture, translated a few px/frame
}

fn content_name(kind: Content) -> &'static str {
    match kind {
        Content::Static => "static",
        Content::Motion => "motion",
        Content::Noise => "noise",
    }
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

/// High-detail texture (xorshift PRNG, fixed seed) translated by
/// 3 px/frame horizontally. Unlike the flat synthetic patterns, this
/// content cannot be coded cheaply with large partitions — it is the
/// discriminator for the cpu_used rate-control investigation.
fn noise_sources(w: u32, h: u32, n: u64) -> Vec<I420Frame> {
    let (wu, hu) = (w as usize, h as usize);
    let mut state: u32 = 0x1234_5678;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        (state >> 24) as u8
    };
    let base_y: Vec<u8> = (0..wu * hu).map(|_| next()).collect();
    let base_u: Vec<u8> = (0..(wu / 2) * (hu / 2))
        .map(|_| 96 + (next() % 64))
        .collect();
    let base_v: Vec<u8> = (0..(wu / 2) * (hu / 2))
        .map(|_| 96 + (next() % 64))
        .collect();
    (0..n)
        .map(|seq| {
            let off = (seq as usize * 3) % wu;
            let mut y = vec![0u8; wu * hu];
            for row in 0..hu {
                let src = &base_y[row * wu..(row + 1) * wu];
                y[row * wu..row * wu + (wu - off)].copy_from_slice(&src[off..]);
                y[row * wu + (wu - off)..(row + 1) * wu].copy_from_slice(&src[..off]);
            }
            I420Frame::new(w, h, seq, y, base_u.clone(), base_v.clone()).unwrap()
        })
        .collect()
}

fn make_sources(kind: Content, w: u32, h: u32, n: u64) -> Vec<I420Frame> {
    match kind {
        Content::Noise => noise_sources(w, h, n),
        _ => (0..n)
            .map(|i| match kind {
                Content::Motion => moving_rectangle(w, h, i).unwrap(),
                Content::Static => static_frame(w, h, i).unwrap(),
                Content::Noise => unreachable!(),
            })
            .collect(),
    }
}

#[cfg(target_os = "macos")]
fn cpu_time() -> Duration {
    // getrusage(RUSAGE_SELF): ru_utime and ru_stime are the first two
    // fields; each timeval is {i64 tv_sec, i32 tv_usec} padded to 16 B.
    #[repr(C)]
    struct Rusage {
        utime_sec: i64,
        utime_usec: i32,
        _p0: i32,
        stime_sec: i64,
        stime_usec: i32,
        _p1: i32,
        _rest: [u8; 128],
    }
    extern "C" {
        fn getrusage(who: i32, usage: *mut Rusage) -> i32;
    }
    let mut ru = Rusage {
        utime_sec: 0,
        utime_usec: 0,
        _p0: 0,
        stime_sec: 0,
        stime_usec: 0,
        _p1: 0,
        _rest: [0; 128],
    };
    unsafe { getrusage(0, &mut ru) };
    let secs = (ru.utime_sec + ru.stime_sec) as u64;
    let micros = (ru.utime_usec + ru.stime_usec) as u64;
    Duration::from_secs(secs) + Duration::from_micros(micros)
}

#[cfg(not(target_os = "macos"))]
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
    tile_columns: Option<u32>,
    screen_content: Option<bool>,
    label: String,
    out: Option<String>,
    paced: bool,
    multi: bool,
    windows: u32,
    sweep: bool,
    motion_only: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        frames: 90,
        cpu_used: 6,
        enc_threads: None,
        dec_threads: None,
        tile_columns: None,
        screen_content: None,
        label: "run".into(),
        out: None,
        paced: false,
        multi: false,
        windows: 3,
        sweep: false,
        motion_only: false,
    };
    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < argv.len() {
        let (k, v) = (argv[i].as_str(), argv.get(i + 1).cloned());
        match k {
            "--frames" => {
                a.frames = v.unwrap().parse().unwrap();
                i += 2;
            }
            "--cpu-used" => {
                a.cpu_used = v.unwrap().parse().unwrap();
                i += 2;
            }
            "--enc-threads" => {
                a.enc_threads = Some(v.unwrap().parse().unwrap());
                i += 2;
            }
            "--dec-threads" => {
                a.dec_threads = Some(v.unwrap().parse().unwrap());
                i += 2;
            }
            "--screen-content" => {
                a.screen_content = Some(true);
                i += 1;
            }
            "--no-screen-content" => {
                a.screen_content = Some(false);
                i += 1;
            }
            "--tile-columns" => {
                a.tile_columns = Some(v.unwrap().parse().unwrap());
                i += 2;
            }
            "--label" => {
                a.label = v.unwrap();
                i += 2;
            }
            "--out" => {
                a.out = v;
                i += 2;
            }
            "--windows" => {
                a.windows = v.unwrap().parse().unwrap();
                i += 2;
            }
            "--paced" => {
                a.paced = true;
                i += 1;
            }
            "--multi" => {
                a.multi = true;
                i += 1;
            }
            "--sweep" => {
                a.sweep = true;
                i += 1;
            }
            "--motion-only" => {
                a.motion_only = true;
                i += 1;
            }
            _ => break,
        }
    }
    a
}

fn encoder_cfg(args: &Args, w: u32, h: u32, bitrate: u32, cpu_used: i32) -> EncoderConfig {
    let mut cfg = EncoderConfig::realtime(w, h, bitrate);
    cfg.cpu_used = cpu_used;
    if let Some(t) = args.enc_threads {
        cfg.threads = t;
    }
    if let Some(tc) = args.tile_columns {
        cfg.tile_columns_log2 = tc;
    }
    if let Some(sc) = args.screen_content {
        cfg.screen_content = sc;
    }
    cfg
}

/// Run one frame through frag/reasm/decode/sink; returns
/// (frag+reasm ms, decode ms, psnr or None, decoded count).
#[allow(clippy::too_many_arguments)]
fn decode_path(
    dec: &mut Vp9Decoder,
    pkt: &codec::EncodedFrame,
    sources: &[I420Frame],
    count_psnr: bool,
    frag_ms: &mut Vec<f64>,
    dec_ms: &mut Vec<f64>,
    psnrs: &mut Vec<f64>,
) -> Result<u64> {
    let mut decoded = 0u64;
    let t1 = Instant::now();
    let dgrams = fragment_frame(1, pkt.seq, pkt.is_keyframe, &pkt.data, MAX_DATAGRAM)?;
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
    let mut sink_checksum: u64 = 0;
    for f in &frames_out {
        let copy = (f.y().to_vec(), f.u().to_vec(), f.v().to_vec());
        for b in &copy.0 {
            sink_checksum = sink_checksum.wrapping_mul(31).wrapping_add(*b as u64);
        }
        if count_psnr {
            let p = luma_psnr(&sources[f.seq() as usize], f);
            if p.is_finite() {
                psnrs.push(p);
            }
            decoded += 1;
        }
    }
    std::hint::black_box(sink_checksum);
    if count_psnr {
        frag_ms.push((t3 - t1).as_secs_f64() * 1e3);
        dec_ms.push((t4 - t3).as_secs_f64() * 1e3);
    }
    Ok(decoded)
}

fn run_scenario(args: &Args, kind: Content, w: u32, h: u32, bitrate: u32) -> Result<String> {
    let kind_name = content_name(kind);
    let warmup = 5u64;
    let total = warmup + args.frames;
    let sources = make_sources(kind, w, h, total);

    let cfg = encoder_cfg(args, w, h, bitrate, args.cpu_used);
    let enc_threads = cfg.threads;
    let tile_columns = cfg.tile_columns_log2;
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
    let mut pkt_sizes: Vec<usize> = Vec::new();
    let mut enc_bytes = 0usize;
    let mut datagram_bytes = 0usize;
    let mut datagrams = 0usize;
    let mut psnrs = Vec::new();
    let mut decoded = 0u64;

    let cpu0 = cpu_time();
    let wall0 = Instant::now();

    for (idx, frame) in sources.iter().enumerate() {
        let timed = idx as u64 >= warmup;
        let t_gen = Instant::now();
        let t0 = Instant::now();
        let packets = enc.encode(frame)?;
        let t1 = Instant::now();
        for pkt in &packets {
            let n_dgrams = (pkt.data.len() + MAX_DATAGRAM - 1) / MAX_DATAGRAM.max(1);
            let _ = n_dgrams;
            decoded += decode_path(
                &mut dec,
                pkt,
                &sources,
                timed,
                &mut frag_ms,
                &mut dec_ms,
                &mut psnrs,
            )?;
            if timed {
                pkt_sizes.push(pkt.data.len());
                enc_bytes += pkt.data.len();
                // datagram bytes: recompute framing cost cheaply
                let dgrams = fragment_frame(1, pkt.seq, pkt.is_keyframe, &pkt.data, MAX_DATAGRAM)?;
                datagram_bytes += dgrams.iter().map(|d| d.len()).sum::<usize>();
                datagrams += dgrams.len();
            }
        }
        if timed {
            enc_ms.push((t1 - t0).as_secs_f64() * 1e3);
            e2e_ms.push((Instant::now() - t_gen).as_secs_f64() * 1e3);
        }
        enc.set_reference_constraint(Some(frame.seq()));
    }

    if let Ok(path) = std::env::var("PERF_DUMP_FRAMES") {
        let dump = pkt_sizes
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(",");
        std::fs::write(
            format!("{path}.{}.{}.csv", args.label, kind_name),
            format!(
                "label,{}\nframes,{}\npkt_bytes,{}\n",
                args.label, args.frames, dump
            ),
        )
        .ok();
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
    let pkt_f: Vec<f64> = pkt_sizes.iter().map(|&b| b as f64).collect();
    let (pkt_mean, pkt_p50, _, pkt_max) = stats(pkt_f);

    println!(
        "[{}] {}x{} {} target={}kbps cpu_used={} enc_threads={} dec_threads={} tile_cols={} frames={}",
        args.label, w, h, kind_name, bitrate, args.cpu_used, enc_threads, dec_threads,
        tile_columns, args.frames
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
        "  decoded {decoded}/{}  luma_PSNR_mean {:.2} dB  process_CPU {:.1}% of one core (wall {:.2}s, cpu {:.2}s)  pkt mean {:.0} B p50 {:.0} max {:.0}",
        args.frames,
        psnr_mean,
        cpu_pct_one_core,
        wall.as_secs_f64(),
        cpu.as_secs_f64(),
        pkt_mean, pkt_p50, pkt_max
    );
    if !enc.control_warnings.is_empty() {
        println!("  control_warnings: {:?}", enc.control_warnings);
    }

    Ok(format!(
        "{{\"label\":\"{}\",\"mode\":\"unpaced\",\"w\":{},\"h\":{},\"content\":\"{}\",\"target_kbps\":{},\"cpu_used\":{},\"enc_threads\":{},\"dec_threads\":{},\"tile_columns_log2\":{},\"frames\":{},\"encode_ms_mean\":{:.3},\"encode_ms_p50\":{:.3},\"encode_ms_p95\":{:.3},\"encode_ms_max\":{:.3},\"decode_ms_mean\":{:.3},\"decode_ms_p50\":{:.3},\"decode_ms_p95\":{:.3},\"decode_ms_max\":{:.3},\"frag_reasm_ms_mean\":{:.4},\"e2e_ms_mean\":{:.3},\"e2e_ms_p50\":{:.3},\"e2e_ms_p95\":{:.3},\"e2e_ms_max\":{:.3},\"serial_fps\":{:.2},\"bitrate_kbps\":{:.1},\"wire_kbps\":{:.1},\"mean_packet_bytes\":{},\"pkt_bytes_p50\":{:.0},\"pkt_bytes_max\":{:.0},\"datagrams_per_frame\":{:.2},\"decoded\":{},\"psnr_db_mean\":{:.2},\"cpu_pct_one_core\":{:.1},\"wall_s\":{:.3},\"cpu_s\":{:.3}}}",
        args.label, w, h, kind_name, bitrate, args.cpu_used, enc_threads, dec_threads,
        tile_columns, args.frames, enc_m, enc_p50, enc_p95, enc_max, dec_m, dec_p50, dec_p95, dec_max, frag_m,
        e2e_m, e2e_p50, e2e_p95, e2e_max, fps_serial, kbps, wire_kbps,
        enc_bytes / args.frames as usize, pkt_p50, pkt_max, datagrams as f64 / n, decoded, psnr_mean, cpu_pct_one_core,
        wall.as_secs_f64(), cpu.as_secs_f64()
    ))
}

// ---------------------------------------------------------------- paced

struct Slot {
    queued: Option<u64>,
    done: bool,
    dropped_stale: u64,
}

fn run_paced(args: &Args, kind: Content, w: u32, h: u32, bitrate: u32) -> Result<String> {
    let kind_name = content_name(kind);
    let warmup = 5u64;
    let total = warmup + args.frames;
    let sources = Arc::new(make_sources(kind, w, h, total));
    let interval = Duration::from_secs_f64(1.0 / NOMINAL_FPS as f64);

    let slot = Arc::new((
        Mutex::new(Slot {
            queued: None,
            done: false,
            dropped_stale: 0,
        }),
        Condvar::new(),
    ));
    let cpu0 = cpu_time();
    let start = Instant::now();

    let prod_slot = slot.clone();
    let producer = std::thread::spawn(move || {
        for seq in 0..total {
            let sched = start + interval * (seq as u32);
            let now = Instant::now();
            if sched > now {
                std::thread::sleep(sched - now);
            }
            let (lock, cv) = &*prod_slot;
            let mut s = lock.lock().unwrap();
            if s.queued.is_some() {
                s.dropped_stale += 1; // depth-1 queue: stale frame replaced, never queued
            }
            s.queued = Some(seq);
            cv.notify_one();
        }
        let (lock, cv) = &*prod_slot;
        let mut s = lock.lock().unwrap();
        s.done = true;
        cv.notify_one();
    });

    // Consumer runs on this thread.
    let cfg = encoder_cfg(args, w, h, bitrate, args.cpu_used);
    let enc_threads = cfg.threads;
    let mut enc = Vp9Encoder::new(cfg)?;
    let mut dec_cfg = DecoderConfig::default();
    if let Some(t) = args.dec_threads {
        dec_cfg.threads = t;
    }
    let dec_threads = dec_cfg.threads;
    let mut dec = Vp9Decoder::new(dec_cfg)?;

    let mut lat_ms = Vec::new();
    let mut enc_ms = Vec::new();
    let mut dec_ms = Vec::new();
    let mut frag_ms = Vec::new();
    let mut psnrs = Vec::new();
    let mut enc_bytes = 0usize;
    let mut processed: u64 = 0;

    loop {
        let seq = {
            let (lock, cv) = &*slot;
            let mut s = lock.lock().unwrap();
            loop {
                if let Some(q) = s.queued.take() {
                    break Some(q);
                }
                if s.done {
                    break None;
                }
                s = cv.wait(s).unwrap();
            }
        };
        let Some(seq) = seq else { break };
        let sched = start + interval * (seq as u32);
        let frame = &sources[seq as usize];
        let timed = seq >= warmup;
        let t0 = Instant::now();
        let packets = enc.encode(frame)?;
        let t1 = Instant::now();
        for pkt in &packets {
            let before_dec = dec_ms.len();
            let _ = before_dec;
            let mut f = Vec::new();
            let mut d = Vec::new();
            let mut p = Vec::new();
            decode_path(&mut dec, pkt, &sources, timed, &mut f, &mut d, &mut p)?;
            if timed {
                frag_ms.extend(f);
                dec_ms.extend(d);
                psnrs.extend(p);
                enc_bytes += pkt.data.len();
            }
        }
        enc.set_reference_constraint(Some(frame.seq()));
        if timed {
            processed += 1;
            enc_ms.push((t1 - t0).as_secs_f64() * 1e3);
            lat_ms.push((Instant::now() - sched).as_secs_f64() * 1e3);
        }
    }
    producer.join().unwrap();
    let wall = start.elapsed();
    let cpu = cpu_time() - cpu0;
    let dropped = slot.0.lock().unwrap().dropped_stale;
    // Timed drops: drops of seq >= warmup are not separable in the counter
    // (warmup drops are included); report total and processed so the
    // timed-window drop count can be derived as frames - processed.
    let timed_dropped = args.frames - processed;
    let (lat_m, lat_p50, lat_p95, lat_max) = stats(lat_ms);
    let (enc_m, enc_p50, _, _) = stats(enc_ms);
    let (dec_m, dec_p50, _, _) = stats(dec_ms);
    let content_secs = args.frames as f64 / NOMINAL_FPS as f64;
    let kbps = enc_bytes as f64 * 8.0 / content_secs / 1000.0;
    let cpu_pct = cpu.as_secs_f64() / wall.as_secs_f64() * 100.0;
    let psnr_mean = if psnrs.is_empty() {
        0.0
    } else {
        psnrs.iter().sum::<f64>() / psnrs.len() as f64
    };

    println!(
        "[{}|paced] {}x{} {} target={}kbps cpu_used={} enc_threads={} dec_threads={} frames={}",
        args.label, w, h, kind_name, bitrate, args.cpu_used, enc_threads, dec_threads, args.frames
    );
    println!(
        "  latency(sched->sink) mean {lat_m:.2} ms p50 {lat_p50:.2} p95 {lat_p95:.2} max {lat_max:.2} | encode mean {enc_m:.2} p50 {enc_p50:.2} | decode mean {dec_m:.2} p50 {dec_p50:.2}"
    );
    println!(
        "  processed {processed}/{} timed_dropped_stale {timed_dropped} (total drops incl warmup {dropped})  bitrate(processed basis) {kbps:.0} kbps  PSNR {:.2} dB  CPU {:.1}% of one core (wall {:.2}s cpu {:.2}s)",
        args.frames, psnr_mean, cpu_pct, wall.as_secs_f64(), cpu.as_secs_f64()
    );

    Ok(format!(
        "{{\"label\":\"{}\",\"mode\":\"paced\",\"w\":{},\"h\":{},\"content\":\"{}\",\"target_kbps\":{},\"cpu_used\":{},\"enc_threads\":{},\"dec_threads\":{},\"frames\":{},\"processed\":{},\"dropped_stale_timed\":{},\"dropped_stale_total\":{},\"latency_ms_mean\":{:.3},\"latency_ms_p50\":{:.3},\"latency_ms_p95\":{:.3},\"latency_ms_max\":{:.3},\"encode_ms_mean\":{:.3},\"encode_ms_p50\":{:.3},\"decode_ms_mean\":{:.3},\"decode_ms_p50\":{:.3},\"bitrate_kbps_processed_basis\":{:.1},\"psnr_db_mean\":{:.2},\"cpu_pct_one_core\":{:.1},\"wall_s\":{:.3},\"cpu_s\":{:.3}}}",
        args.label, w, h, kind_name, bitrate, args.cpu_used, enc_threads, dec_threads,
        args.frames, processed, timed_dropped, dropped, lat_m, lat_p50, lat_p95, lat_max,
        enc_m, enc_p50, dec_m, dec_p50, kbps, psnr_mean, cpu_pct, wall.as_secs_f64(), cpu.as_secs_f64()
    ))
}

// ---------------------------------------------------------------- multi

fn run_multi(args: &Args, w: u32, h: u32, bitrate: u32) -> Result<String> {
    let windows = args.windows;
    let frames = args.frames;
    let cpu_used = args.cpu_used;
    let enc_threads = args.enc_threads.unwrap_or(2);
    let tile_columns = args.tile_columns.unwrap_or(0);
    let cpu0 = cpu_time();
    let wall0 = Instant::now();
    let mut handles = Vec::new();
    for _win in 0..windows {
        handles.push(std::thread::spawn(move || -> Result<(u64, usize)> {
            let sources = make_sources(Content::Motion, w, h, frames);
            let mut cfg = EncoderConfig::realtime(w, h, bitrate);
            cfg.cpu_used = cpu_used;
            cfg.threads = enc_threads;
            cfg.tile_columns_log2 = tile_columns;
            let mut enc = Vp9Encoder::new(cfg)?;
            let mut bytes = 0usize;
            for f in &sources {
                for pkt in &enc.encode(f)? {
                    bytes += pkt.data.len();
                }
                enc.set_reference_constraint(Some(f.seq()));
            }
            Ok((frames, bytes))
        }));
    }
    let mut total_frames = 0u64;
    let mut total_bytes = 0usize;
    for hd in handles {
        let (n, b) = hd.join().unwrap()?;
        total_frames += n;
        total_bytes += b;
    }
    let wall = wall0.elapsed();
    let cpu = cpu_time() - cpu0;
    let agg_fps = total_frames as f64 / wall.as_secs_f64();
    let per_window_fps = agg_fps / windows as f64;
    let cpu_pct = cpu.as_secs_f64() / wall.as_secs_f64() * 100.0;
    // Per-window bitrate: (total bytes / windows) over the per-window
    // content duration (frames / fps). (An earlier version divided by
    // windows twice via total_frames and reported 1/windows of the true
    // value at windows > 1; caught against the windows=1 run.)
    let kbps_per_window =
        total_bytes as f64 * 8.0 / (frames as f64 / NOMINAL_FPS as f64) / windows as f64 / 1000.0;
    println!(
        "[{}|multi] {}x{} motion windows={} enc_threads={} tile_cols={} cpu_used={} frames/win={}",
        args.label, w, h, windows, enc_threads, tile_columns, cpu_used, frames
    );
    println!(
        "  aggregate {:.1} fps  per-window {:.1} fps  bitrate/win {:.0} kbps  CPU {:.1}% of one core (wall {:.2}s cpu {:.2}s)  cpu_s_per_window_frame {:.2} ms",
        agg_fps, per_window_fps, kbps_per_window, cpu_pct, wall.as_secs_f64(), cpu.as_secs_f64(),
        cpu.as_secs_f64() * 1e3 / total_frames as f64
    );
    Ok(format!(
        "{{\"label\":\"{}\",\"mode\":\"multi\",\"w\":{},\"h\":{},\"content\":\"motion\",\"target_kbps\":{},\"cpu_used\":{},\"enc_threads\":{},\"tile_columns_log2\":{},\"windows\":{},\"frames_per_window\":{},\"aggregate_fps\":{:.2},\"per_window_fps\":{:.2},\"bitrate_kbps_per_window\":{:.1},\"cpu_pct_one_core\":{:.1},\"wall_s\":{:.3},\"cpu_s\":{:.3},\"cpu_ms_per_window_frame\":{:.3}}}",
        args.label, w, h, bitrate, cpu_used, enc_threads, tile_columns, windows, frames,
        agg_fps, per_window_fps, kbps_per_window, cpu_pct, wall.as_secs_f64(), cpu.as_secs_f64(),
        cpu.as_secs_f64() * 1e3 / total_frames as f64
    ))
}

// ---------------------------------------------------------------- sweep

fn run_sweep(args: &Args) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    // cpu_used sweep: 1080p motion + noise (discriminator), 720p motion.
    for cpu_used in [5, 6, 7, 8, 9] {
        for (kind, w, h, bitrate) in [
            (Content::Motion, 1920, 1080, 5000),
            (Content::Noise, 1920, 1080, 5000),
            (Content::Motion, 1280, 720, 2500),
        ] {
            let mut a = Args {
                frames: 60,
                ..parse_args_shallow(args)
            };
            a.cpu_used = cpu_used;
            a.label = format!("sweep_cpu{cpu_used}");
            lines.push(run_scenario(&a, kind, w, h, bitrate)?);
        }
    }
    // At cpu_used 8, thread count is the other suspect: repeat 1080p
    // motion with enc_threads 0 and 1 (shipped default is 2).
    for t in [0u32, 1] {
        let mut a = Args {
            frames: 60,
            ..parse_args_shallow(args)
        };
        a.cpu_used = 8;
        a.enc_threads = Some(t);
        a.label = format!("sweep_cpu8_thr{t}");
        lines.push(run_scenario(&a, Content::Motion, 1920, 1080, 5000)?);
    }
    Ok(lines)
}

fn parse_args_shallow(a: &Args) -> Args {
    Args {
        frames: a.frames,
        cpu_used: a.cpu_used,
        enc_threads: a.enc_threads,
        dec_threads: a.dec_threads,
        tile_columns: a.tile_columns,
        screen_content: a.screen_content,
        label: a.label.clone(),
        out: None,
        paced: false,
        multi: false,
        windows: a.windows,
        sweep: false,
        motion_only: a.motion_only,
    }
}

fn main() -> Result<()> {
    let args = parse_args();
    let mut lines = Vec::new();
    if args.sweep {
        lines.extend(run_sweep(&args)?);
    } else if args.multi {
        for (w, h, bitrate) in [(1280u32, 720u32, 2500u32), (1920, 1080, 5000)] {
            lines.push(run_multi(&args, w, h, bitrate)?);
        }
    } else if args.paced {
        for (w, h, bitrate) in [(1280u32, 720u32, 2500u32), (1920, 1080, 5000)] {
            for kind in [Content::Static, Content::Motion] {
                lines.push(run_paced(&args, kind, w, h, bitrate)?);
            }
        }
    } else {
        let kinds: &[Content] = if args.motion_only {
            &[Content::Motion]
        } else {
            &[Content::Static, Content::Motion]
        };
        for (w, h, bitrate) in [(1280u32, 720u32, 2500u32), (1920, 1080, 5000)] {
            for &kind in kinds {
                lines.push(run_scenario(&args, kind, w, h, bitrate)?);
            }
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
