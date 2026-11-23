//! E2E host binary: test pattern -> VP9 encode -> Frame datagrams,
//! FrameAck -> encoder reference constraint.
//! Usage: `e2e-host [--port 4433] [--frames 30]`
//! Prints the certificate hash a client must pin (`--cert-hash`).
use anyhow::Result;
use window_stream_e2e::run_host;
use window_stream_transport::{HostEndpoint, SelfSignedIdentity};
fn arg(name: &str, default: &str) -> String {
    let a: Vec<String> = std::env::args().collect();
    a.windows(2)
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
    let frames: u64 = arg("--frames", "30").parse()?;
    let identity = SelfSignedIdentity::generate(&["localhost", "127.0.0.1", "::1"])?;
    println!("cert-hash: {}", identity.cert_hash_hex());
    let endpoint = HostEndpoint::bind(format!("127.0.0.1:{port}").parse().unwrap(), identity)?;
    println!("listening: https://127.0.0.1:{port}");
    let s = run_host(endpoint, frames).await?;
    println!("done: frames_encoded={} frames_sent={} keyframes_sent={} encoded_bytes_total={} acks_received={:?} invalidations={}", s.frames_encoded, s.frames_sent, s.keyframes_sent, s.encoded_bytes_total, s.acks_received, s.invalidations);
    Ok(())
}
