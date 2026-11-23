//! wgpu compositor: every remote window is a set of I420 textures drawn as a
//! quad, in list order (back to front). Window moves / restacking are pure
//! client-local list and geometry changes; no host round-trip.
//!
//! The compositor owns the window list and implements
//! [`crate::sink::FrameSink`], which is the lifecycle seam for the
//! transport/codec tracks: windows are created, resized and destroyed
//! through it, and frames are submitted through it.
//!
//! Coordinates: all geometry is in *logical* pixels. The surface is
//! configured in physical pixels; `scale_factor` (from winit) is the only
//! conversion, applied here at the GPU boundary.

use crate::demo::I420Frame;
use crate::input::WindowId;
use crate::remote_window::{GpuWindowTextures, RemoteWindow, WindowGeometry, TITLE_BAR_H};
use crate::sink::{FrameError, FrameSink};
use anyhow::Result;
use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
}

const VERTEX_SIZE: u64 = std::mem::size_of::<Vertex>() as u64;
/// Persistent vertex buffer capacity, in quads (2 per window: frame +
/// title bar). Recreated larger only if a frame ever exceeds it.
const INITIAL_QUAD_CAPACITY: usize = 256;

fn quad(x0: f32, y0: f32, x1: f32, y1: f32, surf_w: f32, surf_h: f32) -> [Vertex; 6] {
    let cx = |px: f32| (px / surf_w) * 2.0 - 1.0;
    let cy = |py: f32| 1.0 - (py / surf_h) * 2.0;
    let v = |px: f32, py: f32, u: f32, v_: f32| Vertex {
        pos: [cx(px), cy(py)],
        uv: [u, v_],
    };
    [
        v(x0, y0, 0.0, 0.0),
        v(x1, y0, 1.0, 0.0),
        v(x0, y1, 0.0, 1.0),
        v(x1, y0, 1.0, 0.0),
        v(x1, y1, 1.0, 1.0),
        v(x0, y1, 0.0, 1.0),
    ]
}

pub struct Compositor {
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
    /// Logical-to-physical scale (winit `scale_factor`). All geometry in
    /// this crate is logical; this is applied only when talking to wgpu.
    pub scale_factor: f64,
    pipeline: wgpu::RenderPipeline,
    solid_pipeline: wgpu::RenderPipeline,
    solid_bind_group: wgpu::BindGroup,
    #[allow(dead_code)]
    solid_uniform: wgpu::Buffer,
    bind_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    vertex_buffer: wgpu::Buffer,
    quad_capacity: usize,
    /// Back to front: last element is topmost.
    pub windows: Vec<RemoteWindow>,
}

impl Compositor {
    pub async fn new(window: std::sync::Arc<winit::window::Window>) -> Result<Self> {
        let size = window.inner_size(); // physical
        let scale_factor = window.scale_factor();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });
        let surface = instance.create_surface(window)?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| anyhow::anyhow!("no wgpu adapter"))?;
        tracing::info!(adapter = ?adapter.get_info(), "wgpu adapter");
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("client-device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                },
                None,
            )
            .await?;
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .ok_or_else(|| anyhow::anyhow!("surface reports no formats"))?;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps
                .alpha_modes
                .first()
                .copied()
                .ok_or_else(|| anyhow::anyhow!("surface reports no alpha modes"))?,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("i420-layout"),
            entries: &[
                tex_entry(0),
                tex_entry(1),
                tex_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("plane-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("i420-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("i420-pipeline-layout"),
            bind_group_layouts: &[&bind_layout],
            push_constant_ranges: &[],
        });
        let pipeline = make_pipeline(&device, &pipeline_layout, &shader, format);

        let solid_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("solid-color"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let solid_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("solid-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let solid_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("solid-bind"),
            layout: &solid_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: solid_uniform.as_entire_binding(),
            }],
        });
        queue.write_buffer(
            &solid_uniform,
            0,
            bytemuck::cast_slice(&[0.20f32, 0.23, 0.28, 1.0]),
        );
        let solid_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("solid-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("solid.wgsl").into()),
        });
        let solid_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("solid-pipeline-layout"),
                bind_group_layouts: &[&solid_layout],
                push_constant_ranges: &[],
            });
        let solid_pipeline = make_pipeline(&device, &solid_pipeline_layout, &solid_shader, format);

        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("persistent-vertex-buffer"),
            size: INITIAL_QUAD_CAPACITY as u64 * 6 * VERTEX_SIZE,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            surface,
            device,
            queue,
            config,
            scale_factor,
            pipeline,
            solid_pipeline,
            solid_bind_group,
            solid_uniform,
            bind_layout,
            sampler,
            vertex_buffer,
            quad_capacity: INITIAL_QUAD_CAPACITY,
            windows: Vec::new(),
        })
    }

    /// `physical_width/height` are the surface (window inner) size in
    /// physical pixels, as winit reports them.
    pub fn resize(&mut self, physical_width: u32, physical_height: u32, scale_factor: f64) {
        self.scale_factor = scale_factor;
        if physical_width > 0 && physical_height > 0 {
            self.config.width = physical_width;
            self.config.height = physical_height;
            self.surface.configure(&self.device, &self.config);
        }
    }

    fn reconfigure(&self) {
        self.surface.configure(&self.device, &self.config);
    }

    fn ensure_textures(&mut self, idx: usize) {
        if self.windows[idx].textures.is_none() {
            let (w, h) = (
                self.windows[idx].frame_width,
                self.windows[idx].frame_height,
            );
            let tex = GpuWindowTextures::new(
                &self.device,
                &self.queue,
                &self.bind_layout,
                &self.sampler,
                w,
                h,
            );
            self.windows[idx].textures = Some(tex);
        }
    }

    pub fn window(&self, id: WindowId) -> Option<&RemoteWindow> {
        self.windows.iter().find(|w| w.id == id)
    }

    pub fn bring_to_front(&mut self, id: WindowId) {
        if let Some(idx) = self.windows.iter().position(|w| w.id == id) {
            let w = self.windows.remove(idx);
            self.windows.push(w);
            tracing::info!(window_id = id, "bring_to_front");
        }
    }

    /// Topmost window under a logical point, if any.
    pub fn hit(&self, px: f32, py: f32) -> Option<WindowId> {
        self.windows
            .iter()
            .rev()
            .find(|w| w.geometry.contains(px, py))
            .map(|w| w.id)
    }

    pub fn set_colorimetry(
        &mut self,
        id: WindowId,
        colorimetry: crate::remote_window::Colorimetry,
    ) -> bool {
        match self.windows.iter_mut().find(|w| w.id == id) {
            Some(w) => match &mut w.textures {
                Some(tex) => {
                    tex.set_colorimetry(&self.queue, colorimetry);
                    true
                }
                None => false,
            },
            None => false,
        }
    }

    /// Render the current window list. Surface errors are handled here:
    /// Lost / Outdated reconfigure, Timeout is skipped silently,
    /// OutOfMemory is reported.
    pub fn render(&mut self) -> Result<()> {
        let output = match self.surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Lost) | Err(wgpu::SurfaceError::Outdated) => {
                self.reconfigure();
                return Ok(());
            }
            Err(wgpu::SurfaceError::Timeout) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Logical surface size for clip-space conversion.
        let (sw, sh) = (
            self.config.width as f32 / self.scale_factor as f32,
            self.config.height as f32 / self.scale_factor as f32,
        );

        // Build all quads into the persistent vertex buffer (one
        // write_buffer for the whole frame; no per-window allocation).
        let quads_needed = self.windows.iter().filter(|w| w.textures.is_some()).count() * 2;
        if quads_needed > self.quad_capacity {
            let new_cap = quads_needed.next_power_of_two();
            self.vertex_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("persistent-vertex-buffer"),
                size: new_cap as u64 * 6 * VERTEX_SIZE,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.quad_capacity = new_cap;
        }
        let mut verts: Vec<Vertex> = Vec::with_capacity(quads_needed * 6);
        for win in self.windows.iter().filter(|w| w.textures.is_some()) {
            let g = &win.geometry;
            verts.extend_from_slice(&quad(
                g.x,
                g.y + TITLE_BAR_H,
                g.x + g.width,
                g.y + g.height,
                sw,
                sh,
            ));
            verts.extend_from_slice(&quad(g.x, g.y, g.x + g.width, g.y + TITLE_BAR_H, sw, sh));
        }
        if !verts.is_empty() {
            self.queue
                .write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&verts));
        }

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.08,
                            g: 0.09,
                            b: 0.11,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            let mut quad_index: u32 = 0;
            for win in self.windows.iter() {
                let Some(tex) = &win.textures else { continue };
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &tex.bind_group, &[]);
                pass.draw(quad_index * 6..quad_index * 6 + 6, 0..1);
                quad_index += 1;
                pass.set_pipeline(&self.solid_pipeline);
                pass.set_bind_group(0, &self.solid_bind_group, &[]);
                pass.draw(quad_index * 6..quad_index * 6 + 6, 0..1);
                quad_index += 1;
            }
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();
        Ok(())
    }
}

impl FrameSink for Compositor {
    fn upsert_window(
        &mut self,
        id: WindowId,
        frame_width: u32,
        frame_height: u32,
        geometry: WindowGeometry,
        title: &str,
    ) -> Result<(), FrameError> {
        if frame_width == 0 || frame_height == 0 {
            return Err(FrameError::InvalidFrame {
                width: frame_width,
                height: frame_height,
                reason: "zero frame size".into(),
            });
        }
        if let Some(win) = self.windows.iter_mut().find(|w| w.id == id) {
            win.geometry = geometry;
            win.title = title.to_string();
            win.set_frame_size(frame_width, frame_height); // drops textures on change
        } else {
            self.windows.push(RemoteWindow::new(
                id,
                title,
                geometry,
                frame_width,
                frame_height,
            ));
        }
        let idx = self.windows.iter().position(|w| w.id == id).unwrap();
        self.ensure_textures(idx);
        Ok(())
    }

    fn remove_window(&mut self, id: WindowId) -> bool {
        if let Some(idx) = self.windows.iter().position(|w| w.id == id) {
            self.windows.remove(idx);
            true
        } else {
            false
        }
    }

    fn submit_frame(&mut self, id: WindowId, frame: &I420Frame) -> Result<(), FrameError> {
        let Some(idx) = self.windows.iter().position(|w| w.id == id) else {
            return Err(FrameError::UnknownWindow(id));
        };
        // Host-side resize: follow the frame, recreating textures. Never panic.
        if self.windows[idx].frame_width != frame.width
            || self.windows[idx].frame_height != frame.height
        {
            tracing::info!(window_id = id, old = ?(self.windows[idx].frame_width, self.windows[idx].frame_height), new = ?(frame.width, frame.height), "frame size changed; recreating textures");
            self.windows[idx].set_frame_size(frame.width, frame.height);
        }
        self.ensure_textures(idx);
        self.windows[idx]
            .textures
            .as_ref()
            .unwrap()
            .upload(&self.queue, frame)
    }
}

fn tex_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn make_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("pipeline"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: "vs_main",
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: VERTEX_SIZE,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
            }],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
    })
}
