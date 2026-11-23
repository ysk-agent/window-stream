//! window-stream client binary (demo mode).
//!
//! All compositing / window-lifecycle logic lives in the library
//! (`window_stream_client`); this binary is the winit shell: it creates
//! demo windows through the same `FrameSink` seam the transport/codec
//! tracks will use, feeds demo frames, and converts winit input to the
//! placeholder `InputEvent` representation (logged).
//!
//! Coordinates: winit reports physical pixels; everything inside the app
//! (geometry, hit-testing, input) is logical pixels. The conversion is
//! `physical / scale_factor`, done once at this boundary.
//!
//! Usage: `window-stream-client [--demo] [--headless-frames N]`
//! Controls: drag a title bar to move, click to bring to front, Esc closes.

use std::sync::Arc;
use std::time::{Duration, Instant};
use window_stream_client::compositor::Compositor;
use window_stream_client::demo;
use window_stream_client::input::{self, InputEvent, PointerButtonKind, WheelUnit};
use window_stream_client::remote_window::{to_content_coords, WindowGeometry};
use window_stream_client::sink::FrameSink;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId as WinitWindowId};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);

struct DragState {
    window_id: u64,
    offset_x: f32,
    offset_y: f32,
}

struct App {
    window: Option<Arc<Window>>,
    compositor: Option<Compositor>,
    scale_factor: f64,
    cursor: (f32, f32), // logical
    drag: Option<DragState>,
    frame_index: u64,
    next_frame: Instant,
    headless_frames: Option<u64>,
}

impl App {
    fn new(headless_frames: Option<u64>) -> Self {
        Self {
            window: None,
            compositor: None,
            scale_factor: 1.0,
            cursor: (0.0, 0.0),
            drag: None,
            frame_index: 0,
            next_frame: Instant::now(),
            headless_frames,
        }
    }

    fn log_input(&self, ev: InputEvent) {
        tracing::info!(?ev, "input");
    }

    /// Content coordinates for a target window (title-bar offset removed,
    /// scaled to the remote frame size). Falls back to raw logical cursor
    /// position when there is no target.
    fn content_coords(&self, target: Option<u64>) -> (Option<u64>, f32, f32) {
        let Some(c) = &self.compositor else {
            return (None, self.cursor.0, self.cursor.1);
        };
        match target.and_then(|id| c.window(id)) {
            Some(w) => {
                let (x, y) = to_content_coords(
                    &w.geometry,
                    w.frame_width,
                    w.frame_height,
                    self.cursor.0,
                    self.cursor.1,
                );
                (Some(w.id), x, y)
            }
            None => (None, self.cursor.0, self.cursor.1),
        }
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("window-stream client (demo)")
            .with_inner_size(LogicalSize::new(900, 640));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        self.scale_factor = window.scale_factor();
        let mut compositor =
            pollster::block_on(Compositor::new(window.clone())).expect("wgpu init");
        // Demo windows go through the FrameSink seam, exactly like real
        // Window messages from a host will.
        let demo_windows = [
            (
                1,
                "Demo Window A",
                WindowGeometry {
                    x: 40.0,
                    y: 40.0,
                    width: 320.0,
                    height: 264.0,
                },
                320,
                240,
            ),
            (
                2,
                "Demo Window B",
                WindowGeometry {
                    x: 220.0,
                    y: 140.0,
                    width: 280.0,
                    height: 224.0,
                },
                280,
                200,
            ),
            (
                3,
                "Demo Window C",
                WindowGeometry {
                    x: 120.0,
                    y: 300.0,
                    width: 240.0,
                    height: 204.0,
                },
                240,
                180,
            ),
        ];
        for (id, title, geometry, fw, fh) in demo_windows {
            compositor
                .upsert_window(id, fw, fh, geometry, title)
                .expect("upsert demo window");
        }
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
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor;
                if let Some(w) = &self.window {
                    let size = w.inner_size();
                    if let Some(c) = &mut self.compositor {
                        c.resize(size.width, size.height, scale_factor);
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                // Physical -> logical, once, at the winit boundary.
                self.cursor = (
                    (position.x / self.scale_factor) as f32,
                    (position.y / self.scale_factor) as f32,
                );
                // While dragging, move events belong to the dragged window
                // no matter what is under the cursor.
                let target = if let Some(drag) = &self.drag {
                    if let Some(c) = &mut self.compositor {
                        if let Some(w) = c.windows.iter_mut().find(|w| w.id == drag.window_id) {
                            w.geometry.x = self.cursor.0 - drag.offset_x;
                            w.geometry.y = self.cursor.1 - drag.offset_y;
                        }
                    }
                    Some(drag.window_id)
                } else {
                    self.compositor
                        .as_ref()
                        .and_then(|c| c.hit(self.cursor.0, self.cursor.1))
                };
                let (wid, rx, ry) = self.content_coords(target);
                self.log_input(InputEvent::PointerMove {
                    window_id: wid,
                    x: rx,
                    y: ry,
                });
                if self.drag.is_some() {
                    self.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let hit = self
                    .compositor
                    .as_ref()
                    .and_then(|c| c.hit(self.cursor.0, self.cursor.1));
                // During a drag, button events stay with the dragged window.
                let target = self.drag.as_ref().map(|d| d.window_id).or(hit);
                let log_tuple = self.content_coords(target);
                if state == ElementState::Pressed && button == MouseButton::Left {
                    if let Some(id) = hit {
                        let in_title = self
                            .compositor
                            .as_ref()
                            .and_then(|c| c.window(id))
                            .map(|w| w.geometry.in_title_bar(self.cursor.0, self.cursor.1))
                            .unwrap_or(false);
                        let (ox, oy) = self
                            .compositor
                            .as_ref()
                            .and_then(|c| c.window(id))
                            .map(|w| (self.cursor.0 - w.geometry.x, self.cursor.1 - w.geometry.y))
                            .unwrap_or((0.0, 0.0));
                        if let Some(c) = &mut self.compositor {
                            c.bring_to_front(id);
                        }
                        if in_title {
                            self.drag = Some(DragState {
                                window_id: id,
                                offset_x: ox,
                                offset_y: oy,
                            });
                        }
                        self.request_redraw();
                    }
                }
                if state == ElementState::Released && button == MouseButton::Left {
                    self.drag = None;
                }
                let (wid, rx, ry) = log_tuple;
                self.log_input(InputEvent::PointerButton {
                    window_id: wid,
                    button: PointerButtonKind::from(button),
                    pressed: state == ElementState::Pressed,
                    x: rx,
                    y: ry,
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy, unit) = match delta {
                    winit::event::MouseScrollDelta::LineDelta(x, y) => (x, y, WheelUnit::Lines),
                    winit::event::MouseScrollDelta::PixelDelta(p) => (
                        (p.x / self.scale_factor) as f32,
                        (p.y / self.scale_factor) as f32,
                        WheelUnit::Pixels,
                    ),
                };
                let target = self.drag.as_ref().map(|d| d.window_id).or_else(|| {
                    self.compositor
                        .as_ref()
                        .and_then(|c| c.hit(self.cursor.0, self.cursor.1))
                });
                let (wid, _, _) = self.content_coords(target);
                self.log_input(InputEvent::PointerWheel {
                    window_id: wid,
                    delta_x: dx,
                    delta_y: dy,
                    unit,
                });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    if let winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape) =
                        event.logical_key
                    {
                        event_loop.exit();
                    }
                }
                let focused = self
                    .compositor
                    .as_ref()
                    .and_then(|c| c.windows.last().map(|w| w.id));
                self.log_input(input::from_key_event(focused, &event));
            }
            WindowEvent::RedrawRequested => {
                // Generate a new demo frame only when one is due; other
                // redraws (input, resize) just recomposite existing textures.
                if Instant::now() >= self.next_frame {
                    self.next_frame = Instant::now() + FRAME_INTERVAL;
                    if let Some(c) = &mut self.compositor {
                        // Headless smoke test: at frame 10, simulate a
                        // host-side resize of window 1 through upsert_window.
                        // submit_frame must follow the new size, not panic.
                        if self.headless_frames.is_some() && self.frame_index == 10 {
                            if let Some(w) = c.window(1) {
                                let g = w.geometry.clone();
                                let _ = c.upsert_window(1, 160, 120, g, "Demo Window A");
                                tracing::info!(
                                    "smoke: window 1 resized to 160x120 via upsert_window"
                                );
                            }
                        }
                        let ids: Vec<(u64, u32, u32, u8)> = c
                            .windows
                            .iter()
                            .map(|w| (w.id, w.frame_width, w.frame_height, w.seed))
                            .collect();
                        for (id, fw, fh, seed) in ids {
                            let frame = demo::generate(fw, fh, self.frame_index, seed);
                            if let Err(e) = c.submit_frame(id, &frame) {
                                tracing::error!(window_id = id, %e, "submit_frame failed");
                            }
                        }
                    }
                    self.frame_index += 1;
                    if let Some(limit) = self.headless_frames {
                        if self.frame_index >= limit {
                            tracing::info!(frames = self.frame_index, "headless demo complete");
                            event_loop.exit();
                            return;
                        }
                    }
                }
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
        // No busy spin: sleep until the next frame is due. Input events
        // wake the loop earlier on their own.
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_frame));
        if Instant::now() >= self.next_frame {
            self.request_redraw();
        }
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().collect();
    let headless_frames = args
        .windows(2)
        .find(|w| w[0] == "--headless-frames")
        .and_then(|w| w[1].parse::<u64>().ok());
    if !args.iter().any(|a| a == "--demo") && headless_frames.is_none() {
        tracing::info!(
            "no mode flag: defaulting to --demo (transport/codec are not connected in this track)"
        );
    }
    let event_loop = EventLoop::new()?;
    let mut app = App::new(headless_frames);
    event_loop.run_app(&mut app)?;
    Ok(())
}
