//! Dummy host binary: serves one synthetic window to one client.
//!
//! Usage: `dummy-host [--port 4433] [--frames 5] [--frame-bytes 4096]`
//! Prints the certificate hash a client must pin (`--cert-hash`).

use anyhow::Result;
use window_stream_dummy::run_host;
use window_stream_transport::{HostEndpoint, SelfSignedIdentity};

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
    let port: u16 = arg("--port", "4433").parse()?;
    let frames: u64 = arg("--frames", "5").parse()?;
    let frame_bytes: usize = arg("--frame-bytes", "4096").parse()?;

    let identity = SelfSignedIdentity::generate(&["localhost", "127.0.0.1", "::1"])?;
    println!("cert-hash: {}", identity.cert_hash_hex());
    let endpoint = HostEndpoint::bind(format!("127.0.0.1:{port}").parse().unwrap(), identity)?;
    println!("listening: https://127.0.0.1:{port}");
    let stats = run_host(endpoint, frames, frame_bytes).await?;
    println!(
        "done: hello_version={} frames_sent={} acks_received={:?}",
        stats.hello_version, stats.frames_sent, stats.acks_received
    );
    Ok(())
}
