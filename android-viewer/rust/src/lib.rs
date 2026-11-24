//! window-stream Android viewer (display-side client).
//!
//! A separate app from the Android host (`net.windowstream.android`):
//! the host needs MediaProjection + accessibility permissions and a
//! foreground service; the viewer needs only INTERNET. They share the
//! Rust core crates (client compositor, transport, codec, proto), not
//! the package.
//!
//! Entry point: [`android_main`], called by the GameActivity glue in
//! the Kotlin `ViewerActivity`. The Kotlin config activity writes
//! `viewer_config.json` into the app's internal data directory:
//!
//! ```json
//! { "url": "https://192.168.1.10:4433", "cert_hash": "<64 hex chars>" }
//! ```
//!
//! With a config, the pipeline thread runs the network client
//! (`pipeline::run_network_client`). Without one (or with an unreadable
//! one) it runs the in-process demo source
//! (`pipeline::run_demo_source`), so the app renders the VP9 test
//! pattern with no host involved.
//!
//! Known v1 limitations: no touch input forwarding yet (display
//! first), and the wgpu surface is created once at `resumed` — a
//! suspend/resume cycle that replaces the native window may need an
//! app restart.

mod app;
mod pipeline;

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct ViewerConfig {
    url: Option<String>,
    cert_hash: Option<String>,
}

enum Mode {
    Network { url: String, cert_hash: String },
    Demo,
}

fn load_mode(data_dir: Option<PathBuf>) -> Mode {
    let Some(dir) = data_dir else {
        tracing::warn!("no internal data path; falling back to demo mode");
        return Mode::Demo;
    };
    let path = dir.join("viewer_config.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            tracing::info!(path = %path.display(), %e, "no viewer config; demo mode");
            return Mode::Demo;
        }
    };
    match serde_json::from_str::<ViewerConfig>(&text) {
        Ok(ViewerConfig {
            url: Some(url),
            cert_hash: Some(cert_hash),
        }) if !url.is_empty() && !cert_hash.is_empty() => {
            tracing::info!(%url, "viewer config loaded; network mode");
            Mode::Network { url, cert_hash }
        }
        Ok(_) => {
            tracing::info!("viewer config has no url/cert_hash; demo mode");
            Mode::Demo
        }
        Err(e) => {
            tracing::warn!(%e, "viewer config unreadable; demo mode");
            Mode::Demo
        }
    }
}

fn spawn_pipeline(mode: Mode, tx: std::sync::mpsc::Sender<pipeline::SinkEvent>) {
    std::thread::spawn(move || {
        let result = match mode {
            Mode::Network { url, cert_hash } => pipeline::run_network_client(url, cert_hash, tx),
            Mode::Demo => pipeline::run_demo_source(tx),
        };
        match result {
            Ok(()) => tracing::info!("pipeline ended"),
            Err(e) => tracing::error!(%e, "pipeline failed"),
        }
    });
}

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(android_app: winit::platform::android::activity::AndroidApp) {
    paranoid_android::init("windowstream_viewer");
    tracing::info!("window-stream viewer starting");
    let mode = load_mode(android_app.internal_data_path());
    let (tx, rx) = std::sync::mpsc::channel();
    spawn_pipeline(mode, tx);
    if let Err(e) = app::run_event_loop(rx, android_app) {
        tracing::error!(%e, "event loop failed");
    }
}
