//! Common, OS-independent types shared by every platform backend.

use std::time::Duration;

/// Stable identifier of a top-level window.
///
/// On macOS this is the `CGWindowID` (as reported by
/// `SCWindow.windowID`). On stub/other backends it is an opaque
/// backend-assigned number. Values are only meaningful within one
/// process / one backend instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub u32);

impl std::fmt::Display for WindowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Axis-aligned rectangle in logical points, top-left origin,
/// matching macOS `CGRect` / ScreenCaptureKit `SCWindow.frame`.
/// (`x`, `y` may be negative on multi-display setups.)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// One enumerated top-level window.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    pub id: WindowId,
    /// Window title, if the OS exposes one. `None` is common
    /// (permission not granted, or the window simply has no title).
    pub title: Option<String>,
    /// Owning application name, if known.
    pub app_name: Option<String>,
    /// Owning process id, if known.
    pub app_pid: Option<i32>,
    /// Position and size in points.
    pub bounds: Rect,
    /// Window layer (macOS `SCWindow.windowLayer`, 0 for normal windows).
    pub layer: i64,
    pub is_on_screen: bool,
    pub is_active: bool,
}

/// Pixel layout of a captured frame.
///
/// The protocol fixes VP9 8-bit 4:2:0 downstream, so capture backends
/// deliver either raw `Bgra` (what ScreenCaptureKit produces most
/// cheaply) or `I420` directly. Conversion BGRA -> I420 lives in
/// [`crate::convert`] so the encoder track does not need to redo it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 32-bit BGRA, one plane, tightly described by `strides[0]`.
    Bgra8888,
    /// Planar YUV 4:2:0: Y plane, then U, then V. `strides` has 3
    /// entries, `data` is the planes concatenated in that order.
    I420,
}

impl PixelFormat {
    /// OSType / FourCC used by ScreenCaptureKit for the BGRA layout
    /// (`kCVPixelFormatType_32BGRA`).
    pub const fn macos_bgra_ostype() -> u32 {
        u32::from_be_bytes(*b"BGRA")
    }
    /// OSType for planar 4:2:0 (`kCVPixelFormatType_420YpCbCr8Planar`).
    pub const fn macos_i420_ostype() -> u32 {
        0x7934_3230 // 'y420'
    }
}

/// Capture parameters for one window stream.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureConfig {
    /// Output width in pixels. `0` means "window's current pixel size".
    pub width: u32,
    /// Output height in pixels. `0` means "window's current pixel size".
    pub height: u32,
    /// Maximum frames per second the backend should deliver.
    pub max_fps: u32,
    pub pixel_format: PixelFormat,
    pub show_cursor: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            max_fps: 30,
            pixel_format: PixelFormat::Bgra8888,
            show_cursor: false,
        }
    }
}

/// One captured frame, owned and self-contained.
///
/// The backend copies pixels out of any OS-owned buffer before
/// invoking the callback (ScreenCaptureKit / `CVPixelBuffer` storage
/// is recycled by the OS), so a `CapturedFrame` may be kept, queued
/// (depth 1, per the implementation policy) or moved across threads.
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedFrame {
    pub window_id: WindowId,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Row stride in bytes, per plane (1 entry for BGRA, 3 for I420).
    pub strides: Vec<usize>,
    /// Pixel bytes: planes concatenated in order.
    pub data: Vec<u8>,
    /// Presentation timestamp of the frame, if the backend provides one.
    pub timestamp: Option<Duration>,
    /// Monotonic per-session sequence number, starting at 0.
    pub sequence: u64,
}

/// Mouse / pointer buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerButton {
    Left,
    Right,
    Center,
    Other(u16),
}

/// Pointer input. Coordinates are in points, top-left origin, in the
/// same global space as [`WindowInfo::bounds`]; backends translate to
/// whatever the OS expects (macOS Quartz coordinates are bottom-left,
/// so the macOS backend flips `y` -- see `src/macos/input.rs`).
#[derive(Debug, Clone, PartialEq)]
pub enum PointerEvent {
    Move {
        x: f64,
        y: f64,
    },
    ButtonDown {
        button: PointerButton,
        x: f64,
        y: f64,
    },
    ButtonUp {
        button: PointerButton,
        x: f64,
        y: f64,
    },
    /// Scroll wheel at (`x`, `y`). Sign convention (Quartz wheel
    /// events, to be confirmed on hardware): `dy > 0` scrolls up,
    /// `dx > 0` scrolls right. `dx`/`dy` are in pixels for the
    /// macOS backend (it requests `ScrollEventUnit::PIXEL`).
    Scroll {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    },
}

/// Keyboard input.
///
/// `key_code` is a platform *virtual* key code (macOS: the
/// `kVK_*` hardware-independent code, e.g. `0x00` = A). Layout-dependent
/// text input is deliberately not part of v1; the client sends key
/// codes and modifiers, mirroring the protocol's `Input` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub key_code: u16,
    pub down: bool,
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub meta: bool,
}

impl KeyEvent {
    pub fn new(key_code: u16, down: bool) -> Self {
        Self {
            key_code,
            down,
            shift: false,
            control: false,
            alt: false,
            meta: false,
        }
    }
}

/// Errors every backend can return.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PlatformError {
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("window not found: {0}")]
    WindowNotFound(WindowId),
    #[error("capture already running for window {0}")]
    AlreadyCapturing(WindowId),
    #[error("backend not available on this platform: {0}")]
    Unsupported(String),
    #[error("OS error: {0}")]
    Os(String),
    #[error("timed out waiting for the OS: {0}")]
    Timeout(String),
}

pub type Result<T> = std::result::Result<T, PlatformError>;
