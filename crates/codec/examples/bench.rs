//! Minimal encode bench: synthetic moving-rectangle frames at 720p and
//! 1080p, reporting wall-clock encode time per frame and the equivalent
//! fps. Run with:
//!
//! ```sh
//! cargo run --release --example bench
//! ```
//!
//! Decode of the same stream is timed too, as a sanity reference.
//! Numbers are single-run wall clock on whatever machine runs this; they
//! are a smoke signal, not a benchmark suite.

use std::time::Instant;

use codec::pattern::moving_rectangle;
use codec::{DecoderConfig, EncoderConfig, VideoDecoder, VideoEncoder, Vp9Decoder, Vp9Encoder};

const FRAMES: u64 = 60;

fn bench_size(width: u32, height: u32, bitrate_kbps: u32) {
    let sources: Vec<_> = (0..FRAMES)
        .map(|i| moving_rectangle(width, height, i).unwrap())
        .collect();

    // Warm up on a throwaway encoder (first frame is a keyframe; this also
    // pays libvpx's lazy allocation costs) so the timed stream below still
    // starts with its keyframe.
    {
        let mut warm =
            Vp9Encoder::new(EncoderConfig::realtime(width, height, bitrate_kbps)).unwrap();
        let _ = warm.encode(&sources[0]).unwrap();
    }
    let mut enc = Vp9Encoder::new(EncoderConfig::realtime(width, height, bitrate_kbps)).unwrap();

    let start = Instant::now();
    let mut packets = Vec::new();
    for frame in &sources {
        packets.extend(enc.encode(frame).unwrap());
    }
    let enc_elapsed = start.elapsed();
    let total_bytes: usize = packets.iter().map(|p| p.data.len()).sum();

    let mut dec = Vp9Decoder::new(DecoderConfig::default()).unwrap();
    let start = Instant::now();
    for packet in &packets {
        let _ = dec.decode(packet).unwrap();
    }
    let dec_elapsed = start.elapsed();

    let enc_ms = enc_elapsed.as_secs_f64() * 1000.0 / FRAMES as f64;
    let dec_ms = dec_elapsed.as_secs_f64() * 1000.0 / packets.len() as f64;
    println!(
        "{width}x{height} @{bitrate_kbps}kbit/s: encode {enc_ms:.2} ms/frame ({:.1} fps), \
         decode {dec_ms:.2} ms/frame ({:.1} fps), mean packet {} bytes",
        1000.0 / enc_ms,
        1000.0 / dec_ms,
        total_bytes / packets.len().max(1),
    );
    if !enc.control_warnings.is_empty() {
        println!("  control warnings: {:?}", enc.control_warnings);
    }
}

fn main() {
    bench_size(1280, 720, 2500);
    bench_size(1920, 1080, 5000);
}
