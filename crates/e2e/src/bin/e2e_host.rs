//! E2E host binary: frame source -> VP9 encode -> Frame datagrams,
//! FrameAck -> encoder reference constraint.
//!
//! Sources (`--source`):
//! * `pattern` (default) — the deterministic moving-rectangle test
//!   pattern; runs anywhere, including this VM and CI.
//! * `real` — live macOS ScreenCaptureKit capture of one window
//!   (`--window <id>`, list ids with `platform_probe --list` on the
//!   Mac). Needs a GUI session and the Screen Recording grant; see
//!   `docs/REAL_CAPTURE.md`. On Linux / in CI the stub backend makes
//!   this fail fast with a clear error instead of faking frames.
//!
//! Usage: `e2e-host [--port 4433] [--frames 30] [--source pattern|real] [--window <id>] [--fps 30] [--scale native|point] [--pace fixed|ack]`
//! Prints the certificate hash a client must pin (`--cert-hash`).
use anyhow::{Context, Result};
use window_stream_e2e::{
    start_host_on_with_pace, CaptureScale, PaceMode, PatternSource, RealCaptureSource,
};
fn arg(name: &str, default: &str) -> String {
    let a: Vec<String> = std::env::args().collect();
    a.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].clone())
        .unwrap_or_else(|| default.to_string())
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let port: u16 = arg("--port", "4433").parse()?;
    let frames: u64 = arg("--frames", "30").parse()?;
    let source_name = arg("--source", "pattern");
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    // Start the source *before* binding / printing the cert hash, so
    // a bad window id or a missing Screen Recording grant fails
    // fast with no listener left behind.
    let pace = match arg("--pace", "fixed").as_str() {
        "fixed" => PaceMode::Fixed,
        "ack" => PaceMode::Ack,
        other => anyhow::bail!("unknown --pace {other:?} (want fixed|ack)"),
    };
    let handle = match source_name.as_str() {
        "pattern" => start_host_on_with_pace(addr, frames, PatternSource::new(), pace).await?,
        "real" => {
            let window: u32 = arg("--window", "")
                .parse()
                .context("--source real needs --window <id> (see `platform_probe --list`)")?;
            let fps: u32 = arg("--fps", "30").parse()?;
            let scale = match arg("--scale", "native").as_str() {
                "native" => CaptureScale::Native,
                "point" => CaptureScale::Point,
                other => anyhow::bail!("unknown --scale {other:?} (want native|point)"),
            };
            let source = RealCaptureSource::start_with_scale(window, fps, scale)?;
            start_host_on_with_pace(addr, frames, source, pace).await?
        }
        other => anyhow::bail!("unknown --source {other:?} (want pattern|real)"),
    };
    println!("cert-hash: {}", handle.cert_hash_hex);
    println!("listening: https://127.0.0.1:{port}");
    let s = handle
        .task
        .join()
        .map_err(|_| anyhow::anyhow!("host thread panicked"))?
        .context("host pipeline")?;
    let encode_ms_avg = s.encode_ms_total.checked_div(s.frames_encoded).unwrap_or(0);
    println!("done: frames_encoded={} frames_sent={} keyframes_sent={} encoded_bytes_total={} acks_received={:?} invalidations={} encode_ms_avg={} encode_ms_max={} stream_ms={}", s.frames_encoded, s.frames_sent, s.keyframes_sent, s.encoded_bytes_total, s.acks_received, s.invalidations, encode_ms_avg, s.encode_ms_max, s.stream_ms);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    run().await
}
