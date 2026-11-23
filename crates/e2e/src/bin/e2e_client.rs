//! E2E client binary.
//!
//! Modes:
//! * (default) headless, in-process datagram loop: VP9 decode ->
//!   `CountingSink`, numeric stats printed. Use when localhost UDP is
//!   unavailable or for a quick check.
//! * `--url https://127.0.0.1:PORT --cert-hash HEX [--frames N]`:
//!   headless over real WebTransport, same CountingSink.
//! * `--display [--frames N] [--url ... --cert-hash ...]`: winit window
//!   with the real client `Compositor` as the FrameSink (Xvfb-friendly).
//!   Without `--url` the decoded frames come from the in-process loop
//!   running on a worker thread; with `--url` from the network client.

use anyhow::Result;
use std::sync::mpsc::{channel, Receiver, Sender};
use window_stream_client::demo::I420Frame as ClientFrame;
use window_stream_client::remote_window::WindowGeometry;
use window_stream_client::sink::{FrameError, FrameSink};
use window_stream_e2e::{run_client, run_inprocess, CountingSink};

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
}
fn has(flag: &str) -> bool {
    std::env::args().any(|a| a == flag)
}

/// FrameSink that forwards decoded frames to the winit thread.
struct ChannelSink {
    tx: Sender<(u64, u32, u32, String, ClientFrame)>,
    window: Option<(u64, u32, u32, String)>,
}
impl FrameSink for ChannelSink {
    fn upsert_window(
        &mut self,
        id: u64,
        fw: u32,
        fh: u32,
        _g: WindowGeometry,
        title: &str,
    ) -> Result<(), FrameError> {
        self.window = Some((id, fw, fh, title.to_string()));
        Ok(())
    }
    fn remove_window(&mut self, _id: u64) -> bool {
        true
    }
    fn submit_frame(&mut self, id: u64, frame: &ClientFrame) -> Result<(), FrameError> {
        let (_, fw, fh, title) =
            self.window
                .clone()
                .unwrap_or((id, frame.width, frame.height, String::new()));
        let _ = fw;
        let _ = fh;
        let _ = self.tx.send((
            id,
            frame.width,
            frame.height,
            title,
            ClientFrame {
                width: frame.width,
                height: frame.height,
                y: frame.y.clone(),
                u: frame.u.clone(),
                v: frame.v.clone(),
            },
        ));
        Ok(())
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let frames: u64 = arg("--frames").and_then(|v| v.parse().ok()).unwrap_or(30);
    let url = arg("--url");
    let cert = arg("--cert-hash");

    if has("--display") {
        let hold: u64 = arg("--hold-secs").and_then(|v| v.parse().ok()).unwrap_or(0);
        return run_display(frames, url, cert, hold);
    }

    let rt = tokio::runtime::Runtime::new()?;
    match (url, cert) {
        (Some(url), Some(cert)) => {
            let mut sink = CountingSink::default();
            let stats = rt.block_on(run_client(&url, &cert, frames, &mut sink))?;
            println!(
                "done(network): window_id={} reassembled={:?} decoded={:?} decode_errors={} acks_sent={:?} sink_frames={} min_psnr_db={:.1}",
                stats.window_id, stats.frames_reassembled, stats.frames_decoded, stats.decode_errors,
                stats.acks_sent, sink.frames_submitted, stats.min_luma_psnr_db_x10 as f64 / 10.0
            );
        }
        _ => {
            let mut sink = CountingSink::default();
            let (host, client) = run_inprocess(frames, &mut sink)?;
            println!(
                "done(inprocess): host_sent={} keyframes={} encoded_bytes={} acks_received={:?} invalidations={}",
                host.frames_sent, host.keyframes_sent, host.encoded_bytes_total, host.acks_received, host.invalidations
            );
            println!(
                "done(inprocess): reassembled={:?} decoded={:?} decode_errors={} acks_sent={:?} sink_frames={} min_psnr_db={:.1}",
                client.frames_reassembled, client.frames_decoded, client.decode_errors,
                client.acks_sent, sink.frames_submitted, client.min_luma_psnr_db_x10 as f64 / 10.0
            );
        }
    }
    Ok(())
}

fn run_display(
    frames: u64,
    url: Option<String>,
    cert: Option<String>,
    hold_secs: u64,
) -> Result<()> {
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use window_stream_client::compositor::Compositor;
    use winit::application::ApplicationHandler;
    use winit::dpi::LogicalSize;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::window::{Window, WindowId as WinitWindowId};

    let (tx, rx) = channel::<(u64, u32, u32, String, ClientFrame)>();
    // Worker thread: the full encode->fragment->decode pipeline (network
    // if --url, in-process otherwise), feeding ChannelSink.
    std::thread::spawn(move || {
        let mut sink = ChannelSink { tx, window: None };
        match (url, cert) {
            (Some(url), Some(cert)) => {
                let rt = tokio::runtime::Runtime::new().expect("runtime");
                match rt.block_on(run_client(&url, &cert, frames, &mut sink)) {
                    Ok(s) => tracing::info!(?s, "display network client finished"),
                    Err(e) => tracing::error!(%e, "display network client failed"),
                }
            }
            _ => match run_inprocess(frames, &mut sink) {
                Ok((h, c)) => tracing::info!(?h, ?c, "display in-process pipeline finished"),
                Err(e) => tracing::error!(%e, "display in-process pipeline failed"),
            },
        }
    });

    struct App {
        window: Option<Arc<Window>>,
        compositor: Option<Compositor>,
        rx: Receiver<(u64, u32, u32, String, ClientFrame)>,
        received: u64,
        target: u64,
        started: Instant,
        hold_secs: u64,
        hold_until: Option<Instant>,
    }
    impl ApplicationHandler for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.window.is_some() {
                return;
            }
            let attrs = Window::default_attributes()
                .with_title("window-stream client (e2e VP9)")
                .with_inner_size(LogicalSize::new(900, 640));
            let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
            let compositor =
                pollster::block_on(Compositor::new(window.clone())).expect("wgpu init");
            self.compositor = Some(compositor);
            self.window = Some(window);
        }
        fn window_event(
            &mut self,
            event_loop: &ActiveEventLoop,
            _id: WinitWindowId,
            event: WindowEvent,
        ) {
            match event {
                WindowEvent::CloseRequested => event_loop.exit(),
                WindowEvent::Resized(size) => {
                    if let Some(c) = &mut self.compositor {
                        let sf = self
                            .window
                            .as_ref()
                            .map(|w| w.scale_factor())
                            .unwrap_or(1.0);
                        c.resize(size.width, size.height, sf);
                    }
                }
                WindowEvent::RedrawRequested => {
                    // Drain every decoded frame waiting in the channel.
                    while let Ok((id, fw, fh, title, frame)) = self.rx.try_recv() {
                        if let Some(c) = &mut self.compositor {
                            if c.window(id).is_none() {
                                let _ = c.upsert_window(
                                    id,
                                    fw,
                                    fh,
                                    WindowGeometry {
                                        x: 60.0,
                                        y: 40.0,
                                        width: fw as f32,
                                        height: fh as f32 + 24.0,
                                    },
                                    if title.is_empty() {
                                        "e2e test pattern (VP9)"
                                    } else {
                                        &title
                                    },
                                );
                            }
                            if c.submit_frame(id, &frame).is_ok() {
                                self.received += 1;
                            }
                        }
                    }
                    if let Some(c) = &mut self.compositor {
                        let _ = c.render();
                    }
                    tracing::info!(
                        received = self.received,
                        "display frames submitted to Compositor"
                    );
                    if self.received >= self.target {
                        // Optional hold: keep the final composited frame
                        // on screen (screenshot verification).
                        if self.hold_secs > 0 && self.hold_until.is_none() {
                            self.hold_until =
                                Some(Instant::now() + Duration::from_secs(self.hold_secs));
                        }
                        let hold_expired =
                            self.hold_until.map(|h| Instant::now() >= h).unwrap_or(true);
                        if hold_expired {
                            println!(
                                "display done: frames_submitted_to_compositor={}",
                                self.received
                            );
                            event_loop.exit();
                        }
                    } else if self.started.elapsed() > Duration::from_secs(60) {
                        println!(
                            "display done: frames_submitted_to_compositor={}",
                            self.received
                        );
                        event_loop.exit();
                    }
                }
                _ => {}
            }
        }
        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(33),
            ));
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
    }

    let event_loop = EventLoop::new()?;
    let mut app = App {
        window: None,
        compositor: None,
        rx,
        received: 0,
        target: frames,
        started: Instant::now(),
        hold_secs,
        hold_until: None,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}
