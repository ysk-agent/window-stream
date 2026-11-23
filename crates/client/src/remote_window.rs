//! One remote window: geometry (client-local), and its GPU textures.
//!
//! Geometry is owned by the client: moves and restacking are local
//! operations (per the design) and never wait for the host. Frame size is
//! owned by the host and can change at any time; textures follow it via
//! `Compositor::upsert_window` / `submit_frame` (never a panic).

use crate::demo::I420Frame;
use crate::input::WindowId;
use crate::sink::FrameError;
use bytemuck::{Pod, Zeroable};

pub const TITLE_BAR_H: f32 = 24.0;

/// Chroma plane dimension for I420: odd sizes round up.
pub fn chroma_dim(size: u32) -> u32 {
    size.div_ceil(2)
}

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

/// Convert a cursor position (logical pixels, compositor space) into
/// content coordinates: relative to the frame area (below the title bar),
/// scaled from display size to the remote window's real frame size, and
/// clamped to the frame.
pub fn to_content_coords(
    geometry: &WindowGeometry,
    frame_width: u32,
    frame_height: u32,
    px: f32,
    py: f32,
) -> (f32, f32) {
    let content_w = geometry.width.max(1.0);
    let content_h = (geometry.height - TITLE_BAR_H).max(1.0);
    let rel_x = px - geometry.x;
    let rel_y = py - (geometry.y + TITLE_BAR_H);
    let sx = rel_x / content_w * frame_width as f32;
    let sy = rel_y / content_h * frame_height as f32;
    (
        sx.clamp(0.0, frame_width as f32),
        sy.clamp(0.0, frame_height as f32),
    )
}

/// Colour matrix / range selection for the I420 -> RGB shader.
/// Default by resolution: HD (>= 720 lines, or >= 1280 wide) is BT.709,
/// below that BT.601, matching what VP9 encoders normally use there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMatrix {
    Bt601,
    Bt709,
}

#[derive(Debug, Clone, Copy)]
pub struct Colorimetry {
    pub matrix: ColorMatrix,
    pub full_range: bool,
}

impl Colorimetry {
    pub fn default_for(frame_width: u32, frame_height: u32) -> Self {
        let hd = frame_height >= 720 || frame_width >= 1280;
        Self {
            matrix: if hd {
                ColorMatrix::Bt709
            } else {
                ColorMatrix::Bt601
            },
            full_range: false,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ColorUniform {
    matrix: u32, // 0 = BT.601, 1 = BT.709
    full_range: u32,
    _pad: [u32; 2],
}

/// Validate plane sizes for an I420 frame of `width`x`height`.
pub fn validate_frame(frame: &I420Frame, width: u32, height: u32) -> Result<(), FrameError> {
    let err = |reason: String| FrameError::InvalidFrame {
        width,
        height,
        reason,
    };
    if frame.width != width || frame.height != height {
        return Err(err(format!(
            "frame is {}x{}, expected {}x{}",
            frame.width, frame.height, width, height
        )));
    }
    let (cw, ch) = (chroma_dim(width) as usize, chroma_dim(height) as usize);
    if frame.y.len() != (width * height) as usize {
        return Err(err(format!("Y plane has {} bytes", frame.y.len())));
    }
    if frame.u.len() != cw * ch || frame.v.len() != cw * ch {
        return Err(err(format!(
            "chroma planes have {}/{} bytes, expected {}",
            frame.u.len(),
            frame.v.len(),
            cw * ch
        )));
    }
    Ok(())
}

/// GPU side of a remote window: three R8 textures (Y, U, V), a colour
/// uniform, and a bind group.
pub struct GpuWindowTextures {
    pub bind_group: wgpu::BindGroup,
    pub width: u32,
    pub height: u32,
    pub colorimetry: Colorimetry,
    y_tex: wgpu::Texture,
    u_tex: wgpu::Texture,
    v_tex: wgpu::Texture,
    color_uniform: wgpu::Buffer,
}

fn make_plane(device: &wgpu::Device, w: u32, h: u32, label: &str) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: w.max(1),
            height: h.max(1),
            depth_or_array_layers: 1,
        },
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
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        width: u32,
        height: u32,
    ) -> Self {
        let (cw, ch) = (chroma_dim(width), chroma_dim(height));
        let y_tex = make_plane(device, width, height, "y-plane");
        let u_tex = make_plane(device, cw, ch, "u-plane");
        let v_tex = make_plane(device, cw, ch, "v-plane");
        let color_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("color-uniform"),
            size: std::mem::size_of::<ColorUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let view = |t: &wgpu::Texture| t.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("i420-bind-group"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view(&y_tex)),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view(&u_tex)),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&view(&v_tex)),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: color_uniform.as_entire_binding(),
                },
            ],
        });
        let this = Self {
            bind_group,
            width,
            height,
            colorimetry: Colorimetry::default_for(width, height),
            y_tex,
            u_tex,
            v_tex,
            color_uniform,
        };
        this.write_color_uniform(queue);
        this
    }

    fn write_color_uniform(&self, queue: &wgpu::Queue) {
        let u = ColorUniform {
            matrix: match self.colorimetry.matrix {
                ColorMatrix::Bt601 => 0,
                ColorMatrix::Bt709 => 1,
            },
            full_range: self.colorimetry.full_range as u32,
            _pad: [0; 2],
        };
        queue.write_buffer(&self.color_uniform, 0, bytemuck::cast_slice(&[u]));
    }

    pub fn set_colorimetry(&mut self, queue: &wgpu::Queue, colorimetry: Colorimetry) {
        self.colorimetry = colorimetry;
        self.write_color_uniform(queue);
    }

    /// Upload one frame. Returns an error on a size/plane mismatch instead
    /// of panicking; callers that allow resizes recreate textures first
    /// (see `Compositor::submit_frame`).
    pub fn upload(&self, queue: &wgpu::Queue, frame: &I420Frame) -> Result<(), FrameError> {
        validate_frame(frame, self.width, self.height)?;
        let upload = |tex: &wgpu::Texture, data: &[u8], w: u32, h: u32| {
            queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(w),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        };
        upload(&self.y_tex, &frame.y, self.width, self.height);
        upload(
            &self.u_tex,
            &frame.u,
            chroma_dim(self.width),
            chroma_dim(self.height),
        );
        upload(
            &self.v_tex,
            &frame.v,
            chroma_dim(self.width),
            chroma_dim(self.height),
        );
        Ok(())
    }
}

pub struct RemoteWindow {
    pub id: WindowId,
    pub title: String,
    pub geometry: WindowGeometry,
    pub textures: Option<GpuWindowTextures>,
    pub frame_width: u32,
    pub frame_height: u32,
    pub seed: u8,
}

impl RemoteWindow {
    pub fn new(
        id: WindowId,
        title: &str,
        geometry: WindowGeometry,
        frame_width: u32,
        frame_height: u32,
    ) -> Self {
        Self {
            id,
            title: title.to_string(),
            geometry,
            textures: None,
            frame_width,
            frame_height,
            seed: (id as u8).wrapping_mul(37),
        }
    }
    /// Record a new frame size; returns true if textures must be recreated.
    pub fn set_frame_size(&mut self, width: u32, height: u32) -> bool {
        let changed = self.frame_width != width || self.frame_height != height;
        if changed {
            self.frame_width = width;
            self.frame_height = height;
            self.textures = None;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chroma_is_ceil() {
        assert_eq!(chroma_dim(640), 320);
        assert_eq!(chroma_dim(641), 321);
        assert_eq!(chroma_dim(1), 1);
    }

    #[test]
    fn colorimetry_defaults() {
        assert_eq!(
            Colorimetry::default_for(320, 240).matrix,
            ColorMatrix::Bt601
        );
        assert_eq!(
            Colorimetry::default_for(1920, 1080).matrix,
            ColorMatrix::Bt709
        );
        assert_eq!(
            Colorimetry::default_for(1280, 540).matrix,
            ColorMatrix::Bt709
        );
    }

    #[test]
    fn content_coords_skip_title_bar_and_scale() {
        let g = WindowGeometry {
            x: 100.0,
            y: 50.0,
            width: 200.0,
            height: 124.0,
        };
        let (x, y) = to_content_coords(&g, 400, 200, 200.0, 50.0 + TITLE_BAR_H + 50.0);
        assert_eq!((x, y), (200.0, 100.0));
        let (_, y) = to_content_coords(&g, 400, 200, 200.0, 55.0);
        assert_eq!(y, 0.0);
    }

    #[test]
    fn validate_rejects_bad_planes() {
        let bad = I420Frame {
            width: 64,
            height: 48,
            y: vec![0; 10],
            u: vec![0; 24 * 32],
            v: vec![0; 24 * 32],
        };
        assert!(validate_frame(&bad, 64, 48).is_err());
        let odd = crate::demo::generate(63, 47, 0, 1);
        assert!(validate_frame(&odd, 63, 47).is_ok());
    }

    #[test]
    fn set_frame_size_flags_recreate() {
        let mut w = RemoteWindow::new(
            1,
            "t",
            WindowGeometry {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            },
            64,
            48,
        );
        assert!(!w.set_frame_size(64, 48));
        assert!(w.set_frame_size(128, 96));
    }
}
