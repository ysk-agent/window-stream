//! winit shell for the Android viewer.
//!
//! Mirrors the e2e client's `--display` shell: the winit thread owns
//! the shared `Compositor` (the same `FrameSink` implementation the
//! desktop client uses) and drains [`SinkEvent`]s from the pipeline
//! thread on every redraw. Remote windows are fit into the surface
//! with their aspect ratio preserved (a phone screen is much smaller
//! than a Mac desktop), and refit on resize.
//!
//! Input is deliberately not wired yet: touch -> host `Input`
//! datagrams are the next stage, after display works.

use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};
use window_stream_client::compositor::Compositor;
use window_stream_client::remote_window::{WindowGeometry, TITLE_BAR_H};
use window_stream_client::sink::FrameSink;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId as WinitWindowId};

use crate::pipeline::SinkEvent;

/// Fit a remote frame into the surface (logical pixels), preserving
/// aspect, with the title bar added on top — the same geometry
/// convention as the desktop client (geometry height includes the
/// title bar).
fn fit_geometry(
    frame_width: u32,
    frame_height: u32,
    surface_w: f32,
    surface_h: f32,
) -> WindowGeometry {
    let margin = 8.0f32;
    let avail_w = (surface_w - margin * 2.0).max(1.0);
    let avail_h = (surface_h - margin * 2.0 - TITLE_BAR_H).max(1.0);
    let scale = (avail_w / frame_width.max(1) as f32)
        .min(avail_h / frame_height.max(1) as f32)
        .max(0.01);
    let w = frame_width as f32 * scale;
    let h = frame_height as f32 * scale;
    WindowGeometry {
        x: (surface_w - w) / 2.0,
        y: (surface_h - (h + TITLE_BAR_H)) / 2.0,
        width: w,
        height: h + TITLE_BAR_H,
    }
}

pub struct ViewerApp {
    window: Option<Arc<Window>>,
    compositor: Option<Compositor>,
    rx: Receiver<SinkEvent>,
    scale_factor: f64,
    frames_rendered: u64,
}

impl ViewerApp {
    pub fn new(rx: Receiver<SinkEvent>) -> Self {
        Self {
            window: None,
            compositor: None,
            rx,
            scale_factor: 1.0,
            frames_rendered: 0,
        }
    }

    /// Current surface size in logical pixels.
    fn surface_logical(&self) -> (f32, f32) {
        match &self.compositor {
            Some(c) => (
                c.config.width as f32 / self.scale_factor as f32,
                c.config.height as f32 / self.scale_factor as f32,
            ),
            None => (900.0, 640.0),
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            let Some(c) = &mut self.compositor else {
                // Surface not up yet; dropping lifecycle events here
                // would lose windows, so this cannot happen in
                // practice (resumed runs before the first redraw) —
                // but a frame dropped pre-surface is recoverable: the
                // next frame re-upserts. Log and move on.
                tracing::warn!("sink event before compositor init; dropping");
                return;
            };
            match event {
                SinkEvent::Upsert {
                    id,
                    frame_width,
                    frame_height,
                    title,
                } => {
                    let (sw, sh) = self.surface_logical();
                    let geometry = fit_geometry(frame_width, frame_height, sw, sh);
                    if let Err(e) = c.upsert_window(id, frame_width, frame_height, geometry, &title)
                    {
                        tracing::error!(window_id = id, %e, "upsert_window failed");
                    }
                }
                SinkEvent::Remove { id } => {
                    c.remove_window(id);
                }
                SinkEvent::Frame { id, frame } => {
                    if c.window(id).is_none() {
                        // A frame beat its Window message (or the
                        // upsert raced surface init): create the
                        // window from the frame itself.
                        let (sw, sh) = self.surface_logical();
                        let geometry = fit_geometry(frame.width, frame.height, sw, sh);
                        if let Err(e) = c.upsert_window(id, frame.width, frame.height, geometry, "")
                        {
                            tracing::error!(window_id = id, %e, "implicit upsert failed");
                            continue;
                        }
                    }
                    match c.submit_frame(id, &frame) {
                        Ok(()) => {
                            self.frames_rendered += 1;
                            if self.frames_rendered % 300 == 0 {
                                tracing::info!(
                                    frames = self.frames_rendered,
                                    "viewer frames submitted to compositor"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!(window_id = id, %e, "submit_frame failed");
                        }
                    }
                }
            }
        }
    }

    /// Refit every window after a surface resize (geometry only; the
    /// textures are untouched).
    fn refit_all(&mut self) {
        let (sw, sh) = self.surface_logical();
        if let Some(c) = &mut self.compositor {
            for w in c.windows.iter_mut() {
                w.geometry = fit_geometry(w.frame_width, w.frame_height, sw, sh);
            }
        }
    }
}

impl ApplicationHandler for ViewerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("window-stream viewer")
            .with_inner_size(LogicalSize::new(900, 640));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        self.scale_factor = window.scale_factor();
        let compositor = pollster::block_on(Compositor::new(window.clone())).expect("wgpu init");
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
                    c.resize(size.width, size.height, self.scale_factor);
                }
                self.refit_all();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor;
                if let Some(w) = &self.window {
                    let size = w.inner_size();
                    if let Some(c) = &mut self.compositor {
                        c.resize(size.width, size.height, scale_factor);
                    }
                }
                self.refit_all();
            }
            WindowEvent::RedrawRequested => {
                self.drain_events();
                if let Some(c) = &mut self.compositor {
                    if let Err(e) = c.render() {
                        tracing::error!(%e, "render error");
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // ~30 fps redraw cadence, matching the host's frame rate;
        // input (once wired) wakes the loop on its own.
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(33),
        ));
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

/// Build the event loop (Android variant binds the `AndroidApp` from
/// `android_main`) and run the viewer until the loop exits.
pub fn run_event_loop(
    rx: Receiver<SinkEvent>,
    #[cfg(target_os = "android")] app: winit::platform::android::activity::AndroidApp,
) -> anyhow::Result<()> {
    #[cfg(target_os = "android")]
    let event_loop = {
        use winit::platform::android::EventLoopBuilderExtAndroid;
        let mut builder = EventLoop::builder();
        builder.with_android_app(app);
        builder.build()?
    };
    #[cfg(not(target_os = "android"))]
    let event_loop = EventLoop::new()?;
    let mut viewer = ViewerApp::new(rx);
    event_loop.run_app(&mut viewer)?;
    Ok(())
}
