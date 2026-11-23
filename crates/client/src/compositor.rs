//! wgpu compositor: every remote window is a set of I420 textures drawn as a
//! quad, in list order (back to front). Window moves / restacking are pure
//! client-local list and geometry changes; no host round-trip.

use crate::remote_window::{GpuWindowTextures, RemoteWindow, TITLE_BAR_H};
use anyhow::Result;
use wgpu::util::DeviceExt;
use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
}

fn quad(x0: f32, y0: f32, x1: f32, y1: f32, surf_w: f32, surf_h: f32) -> [Vertex; 6] {
    // Pixel coords (top-left origin) -> clip space.
    let cx = |px: f32| (px / surf_w) * 2.0 - 1.0;
    let cy = |py: f32| 1.0 - (py / surf_h) * 2.0;
    let v = |px: f32, py: f32, u: f32, v_: f32| Vertex { pos: [cx(px), cy(py)], uv: [u, v_] };
    [
        v(x0, y0, 0.0, 0.0), v(x1, y0, 1.0, 0.0), v(x0, y1, 0.0, 1.0),
        v(x1, y0, 1.0, 0.0), v(x1, y1, 1.0, 1.0), v(x0, y1, 0.0, 1.0),
    ]
}

pub struct Compositor {
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    solid_pipeline: wgpu::RenderPipeline,
    solid_bind_group: wgpu::BindGroup,
    solid_uniform: wgpu::Buffer,
    pub bind_layout: wgpu::BindGroupLayout,
    pub sampler: wgpu::Sampler,
}

impl Compositor {
    pub async fn new(window: std::sync::Arc<winit::window::Window>) -> Result<Self> {
        let size = window.inner_size();
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
                &wgpu::DeviceDescriptor { label: Some("client-device"), required_features: wgpu::Features::empty(), required_limits: wgpu::Limits::downlevel_defaults() },
                None,
            )
            .await?;
        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("i420-layout"),
            entries: &[
                tex_entry(0), tex_entry(1), tex_entry(2),
                wgpu::BindGroupLayoutEntry { binding: 3, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None },
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

        // Solid-colour pipeline for title bars.
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
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            }],
        });
        let solid_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("solid-bind"),
            layout: &solid_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: solid_uniform.as_entire_binding() }],
        });
        queue.write_buffer(&solid_uniform, 0, bytemuck::cast_slice(&[0.20f32, 0.23, 0.28, 1.0]));
        let solid_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("solid-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("solid.wgsl").into()),
        });
        let solid_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("solid-pipeline-layout"),
            bind_group_layouts: &[&solid_layout],
            push_constant_ranges: &[],
        });
        let solid_pipeline = make_pipeline(&device, &solid_pipeline_layout, &solid_shader, format);

        Ok(Self { surface, device, queue, config, pipeline, solid_pipeline, solid_bind_group, solid_uniform, bind_layout, sampler })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&self.device, &self.config);
        }
    }

    pub fn ensure_textures(&self, win: &mut RemoteWindow) {
        if win.textures.is_none() {
            win.textures = Some(GpuWindowTextures::new(&self.device, &self.bind_layout, &self.sampler, win.frame_width, win.frame_height));
        }
    }

    pub fn render(&mut self, windows: &[RemoteWindow]) -> Result<(), wgpu::SurfaceError> {
        let output = self.surface.get_current_texture()?;
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        let (sw, sh) = (self.config.width as f32, self.config.height as f32);
        // Pre-create all vertex buffers before the pass: the pass borrows
        // them, so they must not be created (or moved) while it is alive.
        let mut buffers: Vec<wgpu::Buffer> = Vec::new();
        for win in windows {
            if win.textures.is_none() { continue; }
            let g = &win.geometry;
            let verts = quad(g.x, g.y + TITLE_BAR_H, g.x + g.width, g.y + g.height, sw, sh);
            buffers.push(self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("quad"), contents: bytemuck::cast_slice(&verts), usage: wgpu::BufferUsages::VERTEX,
            }));
            let tverts = quad(g.x, g.y, g.x + g.width, g.y + TITLE_BAR_H, sw, sh);
            buffers.push(self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("title-quad"), contents: bytemuck::cast_slice(&tverts), usage: wgpu::BufferUsages::VERTEX,
            }));
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.08, g: 0.09, b: 0.11, a: 1.0 }), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            let mut bi = 0usize;
            for win in windows {
                let Some(tex) = &win.textures else { continue };
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &tex.bind_group, &[]);
                pass.set_vertex_buffer(0, buffers[bi].slice(..));
                pass.draw(0..6, 0..1);
                pass.set_pipeline(&self.solid_pipeline);
                pass.set_bind_group(0, &self.solid_bind_group, &[]);
                pass.set_vertex_buffer(0, buffers[bi + 1].slice(..));
                pass.draw(0..6, 0..1);
                bi += 2;
            }
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();
        Ok(())
    }
}

fn tex_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
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
                array_stride: 16,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
            }],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState { format, blend: Some(wgpu::BlendState::REPLACE), write_mask: wgpu::ColorWrites::ALL })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
    })
}
