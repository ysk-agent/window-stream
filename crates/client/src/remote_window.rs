//! One remote window: geometry (client-local), z-order, and its GPU textures.
//!
//! Geometry is owned by the client: moves and restacking are local
//! operations (per the design) and never wait for the host.

use crate::demo::I420Frame;
use crate::input::WindowId;

pub const TITLE_BAR_H: f32 = 24.0;

#[derive(Debug, Clone)]
pub struct WindowGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl WindowGeometry {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }
    pub fn in_title_bar(&self, px: f32, py: f32) -> bool {
        self.contains(px, py) && py < self.y + TITLE_BAR_H
    }
}

/// GPU side of a remote window: three R8 textures (Y, U, V) + bind group.
pub struct GpuWindowTextures {
    pub bind_group: wgpu::BindGroup,
    pub width: u32,
    pub height: u32,
    y_tex: wgpu::Texture,
    u_tex: wgpu::Texture,
    v_tex: wgpu::Texture,
}

fn make_plane(device: &wgpu::Device, w: u32, h: u32, label: &str) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

impl GpuWindowTextures {
    pub fn new(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        width: u32,
        height: u32,
    ) -> Self {
        let y_tex = make_plane(device, width, height, "y-plane");
        let u_tex = make_plane(device, width / 2, height / 2, "u-plane");
        let v_tex = make_plane(device, width / 2, height / 2, "v-plane");
        let view = |t: &wgpu::Texture| t.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("i420-bind-group"),
            layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view(&y_tex)) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view(&u_tex)) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&view(&v_tex)) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(sampler) },
            ],
        });
        Self { bind_group, width, height, y_tex, u_tex, v_tex }
    }

    pub fn upload(&self, queue: &wgpu::Queue, frame: &I420Frame) {
        assert_eq!(frame.width, self.width);
        assert_eq!(frame.height, self.height);
        let upload = |tex: &wgpu::Texture, data: &[u8], w: u32, h: u32| {
            queue.write_texture(
                wgpu::ImageCopyTexture { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                data,
                wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(w), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        };
        upload(&self.y_tex, &frame.y, self.width, self.height);
        upload(&self.u_tex, &frame.u, self.width / 2, self.height / 2);
        upload(&self.v_tex, &frame.v, self.width / 2, self.height / 2);
    }
}

pub struct RemoteWindow {
    pub id: WindowId,
    pub title: String,
    pub geometry: WindowGeometry,
    /// Higher is more front. Maintained by the compositor's window list order.
    pub textures: Option<GpuWindowTextures>,
    pub frame_width: u32,
    pub frame_height: u32,
    pub seed: u8,
}
