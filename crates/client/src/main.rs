//! window-stream client (client track).
//!
//! Scope of this track: winit + wgpu compositing of per-window I420
//! textures, client-local window move / restack, and input capture to a
//! placeholder `Input` representation (logged). No transport, no codec:
//! demo mode feeds locally generated I420 test frames into the same texture
//! path a decoded VP9 frame will use.
//!
//! Usage: `window-stream-client [--demo]`  (demo is currently the only mode)
//! Controls: drag a window's title bar to move it, click a window to bring
//! it to the front, Esc closes.

mod compositor;
mod demo;
mod input;
mod remote_window;

use compositor::Compositor;
use input::{InputEvent, PointerButtonKind};
use remote_window::{RemoteWindow, WindowGeometry};
use std::sync::Arc;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId as WinitWindowId};

struct DragState {
    window_id: u64,
    offset_x: f32,
    offset_y: f32,
}

struct App {
    window: Option<Arc<Window>>,
    compositor: Option<Compositor>,
    /// Back to front: last element is topmost.
    windows: Vec<RemoteWindow>,
    cursor: (f32, f32),
    drag: Option<DragState>,
    frame_index: u64,
    last_frame: Instant,
    headless_frames: Option<u64>,
}

impl App {
    fn new(headless_frames: Option<u64>) -> Self {
        let windows = vec![
            RemoteWindow { id: 1, title: "Demo Window A".into(), geometry: WindowGeometry { x: 40.0, y: 40.0, width: 320.0, height: 264.0 }, textures: None, frame_width: 320, frame_height: 240, seed: 10 },
            RemoteWindow { id: 2, title: "Demo Window B".into(), geometry: WindowGeometry { x: 220.0, y: 140.0, width: 280.0, height: 224.0 }, textures: None, frame_width: 280, frame_height: 200, seed: 70 },
            RemoteWindow { id: 3, title: "Demo Window C".into(), geometry: WindowGeometry { x: 120.0, y: 300.0, width: 240.0, height: 204.0 }, textures: None, frame_width: 240, frame_height: 180, seed: 140 },
        ];
        Self { window: None, compositor: None, windows, cursor: (0.0, 0.0), drag: None, frame_index: 0, last_frame: Instant::now(), headless_frames }
    }

    /// Topmost window under the cursor, if any.
    fn hit(&self, px: f32, py: f32) -> Option<usize> {
        self.windows.iter().rposition(|w| w.geometry.contains(px, py))
    }

    fn bring_to_front(&mut self, idx: usize) {
        let w = self.windows.remove(idx);
        tracing::info!(window_id = w.id, "bring_to_front");
        self.windows.push(w);
    }

    fn log_input(&self, ev: InputEvent) {
        // In the full client this is where the event is handed to transport
        // as a datagram. For this track it is logged, with window id.
        tracing::info!(?ev, "input");
    }

    fn window_relative(&self, idx: Option<usize>) -> (Option<u64>, f32, f32) {
        match idx {
            Some(i) => {
                let g = &self.windows[i].geometry;
                (Some(self.windows[i].id), self.cursor.0 - g.x, self.cursor.1 - g.y)
            }
            None => (None, self.cursor.0, self.cursor.1),
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
        let compositor = pollster::block_on(Compositor::new(window.clone())).expect("wgpu init");
        for w in &mut self.windows {
            compositor.ensure_textures(w);
        }
        self.compositor = Some(compositor);
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WinitWindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(c) = &mut self.compositor {
                    c.resize(size.width, size.height);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
                if let Some(drag) = &self.drag {
                    if let Some(w) = self.windows.iter_mut().find(|w| w.id == drag.window_id) {
                        w.geometry.x = self.cursor.0 - drag.offset_x;
                        w.geometry.y = self.cursor.1 - drag.offset_y;
                    }
                }
                let idx = self.hit(self.cursor.0, self.cursor.1);
                let (wid, rx, ry) = self.window_relative(idx);
                self.log_input(InputEvent::PointerMove { window_id: wid, x: rx, y: ry });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let idx = self.hit(self.cursor.0, self.cursor.1);
                // Compute the log tuple before any restack: bring_to_front
                // reorders the list, which would invalidate `idx`.
                let log_tuple = self.window_relative(idx);
                if state == ElementState::Pressed && button == MouseButton::Left {
                    if let Some(i) = idx {
                        let in_title = self.windows[i].geometry.in_title_bar(self.cursor.0, self.cursor.1);
                        let (ox, oy) = (self.cursor.0 - self.windows[i].geometry.x, self.cursor.1 - self.windows[i].geometry.y);
                        self.bring_to_front(i);
                        if in_title {
                            let top_id = self.windows.last().unwrap().id;
                            self.drag = Some(DragState { window_id: top_id, offset_x: ox, offset_y: oy });
                        }
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
                let (dx, dy) = match delta {
                    winit::event::MouseScrollDelta::LineDelta(x, y) => (x, y),
                    winit::event::MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32),
                };
                let idx = self.hit(self.cursor.0, self.cursor.1);
                let (wid, _, _) = self.window_relative(idx);
                self.log_input(InputEvent::PointerWheel { window_id: wid, delta_x: dx, delta_y: dy });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    if let winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape) = event.logical_key {
                        event_loop.exit();
                    }
                }
                // Keys go to the frontmost (focused) window.
                let focused = self.windows.last().map(|w| w.id);
                self.log_input(input::from_key_event(focused, &event));
            }
            WindowEvent::RedrawRequested => {
                // Throttle demo generation to ~60 fps.
                if self.last_frame.elapsed().as_millis() < 16 && self.frame_index > 0 {
                    return;
                }
                self.last_frame = Instant::now();
                if let Some(c) = &self.compositor {
                    for w in &mut self.windows {
                        let frame = demo::generate(w.frame_width, w.frame_height, self.frame_index, w.seed);
                        if let Some(tex) = &w.textures {
                            tex.upload(&c.queue, &frame);
                        }
                    }
                }
                self.frame_index += 1;
                if let Some(limit) = self.headless_frames {
                    if self.frame_index >= limit {
                        tracing::info!(frames = self.frame_index, "headless demo complete");
                        event_loop.exit();
                    }
                }
                if let Some(c) = &mut self.compositor {
                    match c.render(&self.windows) {
                        Ok(()) => {}
                        Err(wgpu::SurfaceError::Lost) => {
                            let size = self.window.as_ref().unwrap().inner_size();
                            c.resize(size.width, size.height);
                        }
                        Err(e) => tracing::error!(?e, "render error"),
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args: Vec<String> = std::env::args().collect();
    // --headless-frames N: render N demo frames then exit (smoke test / CI).
    let headless_frames = args
        .windows(2)
        .find(|w| w[0] == "--headless-frames")
        .and_then(|w| w[1].parse::<u64>().ok());
    if !args.iter().any(|a| a == "--demo") && headless_frames.is_none() {
        tracing::info!("no mode flag: defaulting to --demo (transport/codec are not connected in this track)");
    }
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App::new(headless_frames);
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hit_and_restack() {
        let mut app = App::new(None);
        // (250,160) is inside both A and B; B is later (more front).
        let idx = app.hit(250.0, 160.0).unwrap();
        assert_eq!(app.windows[idx].id, 2);
        app.bring_to_front(0);
        assert_eq!(app.windows.last().unwrap().id, 1);
        let idx = app.hit(50.0, 50.0).unwrap();
        assert_eq!(app.windows[idx].id, 1);
    }
}
