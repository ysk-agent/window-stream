//! `platform_probe` -- manual, on-device sample / smoke test.
//!
//! Usage:
//!
//! ```text
//! # List capturable windows
//! cargo run --bin platform_probe -- --list
//!
//! # Capture N frames of one window and write them as raw files
//! # (frame-0000.bgra / .yuv, plus a frames.txt index)
//! cargo run --bin platform_probe -- --window 1234 --frames 5 --out ./capture-out
//!
//! # Same, but ask for I420 frames
//! cargo run --bin platform_probe -- --window 1234 --frames 5 --format i420
//! ```
//!
//! On macOS the first run triggers the Screen Recording permission
//! prompt (or fails until the binary is allowed in System Settings);
//! see `crates/platform/docs/PERMISSIONS.md`. On other OSes it uses
//! the stub backend: `--list` prints nothing and capture fails with
//! an explicit "unsupported" error -- that is the expected behaviour
//! there, not a bug.
//!
//! Exit codes: 0 ok, 1 usage error, 2 backend/runtime error,
//! 3 capture timed out before enough frames arrived.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use window_stream_platform::{create_backend, CaptureConfig, PixelFormat, WindowId};

/// (width, height, format, strides, byte length) per received frame.
type FrameSummary = (u32, u32, PixelFormat, Vec<usize>, usize);

fn usage() -> ! {
    eprintln!(
        "usage: platform_probe --list\n       platform_probe --window <id> [--frames N] [--format bgra|i420] [--out DIR]"
    );
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let backend = create_backend();
    println!("backend: {}", backend.name());

    if args.iter().any(|a| a == "--list") {
        match backend.list_windows() {
            Ok(windows) => {
                println!("{} window(s)", windows.len());
                for w in windows {
                    println!(
                        "id={} app={:?} title={:?} bounds=({:.0},{:.0} {:.0}x{:.0}) layer={} on_screen={} active={}",
                        w.id,
                        w.app_name,
                        w.title,
                        w.bounds.x,
                        w.bounds.y,
                        w.bounds.width,
                        w.bounds.height,
                        w.layer,
                        w.is_on_screen,
                        w.is_active,
                    );
                }
            }
            Err(e) => {
                eprintln!("list_windows failed: {e}");
                std::process::exit(2);
            }
        }
        return;
    }

    // Parse --window / --frames / --format / --out.
    let mut window: Option<u32> = None;
    let mut frames_wanted: usize = 5;
    let mut format = PixelFormat::Bgra8888;
    let mut out_dir = PathBuf::from("capture-out");
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--window" => {
                i += 1;
                window = args.get(i).and_then(|v| v.parse().ok());
            }
            "--frames" => {
                i += 1;
                frames_wanted = args
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage());
            }
            "--format" => {
                i += 1;
                format = match args.get(i).map(String::as_str) {
                    Some("bgra") => PixelFormat::Bgra8888,
                    Some("i420") => PixelFormat::I420,
                    _ => usage(),
                };
            }
            "--out" => {
                i += 1;
                out_dir = PathBuf::from(args.get(i).unwrap_or_else(|| usage()));
            }
            _ => usage(),
        }
        i += 1;
    }
    let window_id = WindowId(window.unwrap_or_else(|| usage()));

    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("cannot create {}: {e}", out_dir.display());
        std::process::exit(2);
    }

    let received: Arc<Mutex<Vec<FrameSummary>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    let out_for_cb = out_dir.clone();
    let config = CaptureConfig {
        pixel_format: format,
        ..CaptureConfig::default()
    };

    let callback = Box::new(move |frame: window_stream_platform::CapturedFrame| {
        let ext = match frame.format {
            PixelFormat::Bgra8888 => "bgra",
            PixelFormat::I420 => "yuv",
        };
        let path = out_for_cb.join(format!("frame-{:04}.{ext}", frame.sequence));
        if let Ok(mut f) = std::fs::File::create(&path) {
            let _ = f.write_all(&frame.data);
        }
        if let Ok(mut v) = sink.lock() {
            v.push((
                frame.width,
                frame.height,
                frame.format,
                frame.strides.clone(),
                frame.data.len(),
            ));
        }
        println!(
            "frame seq={} {}x{} {:?} bytes={}",
            frame.sequence,
            frame.width,
            frame.height,
            frame.format,
            frame.data.len()
        );
    });

    let mut session = match backend.start_capture(window_id, config, callback) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("start_capture failed: {e}");
            std::process::exit(2);
        }
    };

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let n = received.lock().map(|v| v.len()).unwrap_or(0);
        if n >= frames_wanted {
            break;
        }
        if Instant::now() > deadline {
            eprintln!("timed out: got {n} of {frames_wanted} frames in 30s");
            let _ = session.stop();
            std::process::exit(3);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = session.stop();

    // Write an index so the raw files are self-describing.
    let frames = received.lock().unwrap();
    let mut index = String::new();
    for (idx, (w, h, fmt, strides, bytes)) in frames.iter().enumerate() {
        index.push_str(&format!(
            "frame-{idx:04} width={w} height={h} format={fmt:?} strides={strides:?} bytes={bytes}\n"
        ));
    }
    let _ = std::fs::write(out_dir.join("frames.txt"), index);
    println!("wrote {} frame(s) to {}", frames.len(), out_dir.display());
}
