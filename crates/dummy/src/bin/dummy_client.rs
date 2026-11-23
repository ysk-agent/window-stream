//! Dummy client binary: connects to a dummy host, logs the window and
//! frames it receives, and answers each frame with a FrameAck.
//!
//! Usage: `dummy-client --url https://127.0.0.1:4433 --cert-hash <hex> [--frames 5]`

use anyhow::Result;
use window_stream_dummy::run_client;

fn arg(name: &str, default: &str) -> String {
    let args: Vec<String> = std::env::args().collect();
    args.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].clone())
        .unwrap_or_else(|| default.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let url = arg("--url", "https://127.0.0.1:4433");
    let cert_hash = arg("--cert-hash", "");
    if cert_hash.is_empty() {
        anyhow::bail!("--cert-hash <hex sha256> is required (printed by dummy-host)");
    }
    let frames: u64 = arg("--frames", "5").parse()?;
    let stats = run_client(&url, &cert_hash, frames).await?;
    println!(
        "done: window_id={} {}x{} frames_received={:?} payload_bytes_total={}",
        stats.window_id,
        stats.window_width,
        stats.window_height,
        stats.frames_received,
        stats.payload_bytes_total
    );
    Ok(())
}
