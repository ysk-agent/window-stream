//! Placeholder input types mirroring the future `proto` `Input` message.
//!
//! The `proto` crate is owned by another track. When it lands, these types
//! are replaced by (or converted into) the generated types; the conversion
//! functions in this module are the only place that knows about `winit`.

use winit::event::{ElementState, KeyEvent, MouseButton};
use winit::keyboard::{Key, PhysicalKey};

pub type WindowId = u64;

#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    Key {
        window_id: Option<WindowId>,
        /// Placeholder key identifier: physical code debug name, falling back
        /// to the logical key. The proto definition will fix the encoding.
        code: String,
        logical: String,
        pressed: bool,
        repeat: bool,
    },
    PointerMove {
        window_id: Option<WindowId>,
        /// Position relative to the target window's top-left, if inside one.
        x: f32,
        y: f32,
    },
    PointerButton {
        window_id: Option<WindowId>,
        button: PointerButtonKind,
        pressed: bool,
        x: f32,
        y: f32,
    },
    PointerWheel {
        window_id: Option<WindowId>,
        delta_x: f32,
        delta_y: f32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButtonKind {
    Left,
    Right,
    Middle,
    Other(u16),
}

impl From<MouseButton> for PointerButtonKind {
    fn from(b: MouseButton) -> Self {
        match b {
            MouseButton::Left => Self::Left,
            MouseButton::Right => Self::Right,
            MouseButton::Middle => Self::Middle,
            MouseButton::Back => Self::Other(3),
            MouseButton::Forward => Self::Other(4),
            MouseButton::Other(v) => Self::Other(v),
        }
    }
}

pub fn from_key_event(window_id: Option<WindowId>, ev: &KeyEvent) -> InputEvent {
    let code = match &ev.physical_key {
        PhysicalKey::Code(c) => format!("{:?}", c),
        PhysicalKey::Unidentified(c) => format!("Unidentified({:?})", c),
    };
    let logical = match &ev.logical_key {
        Key::Named(n) => format!("{:?}", n),
        Key::Character(s) => s.to_string(),
        Key::Unidentified(c) => format!("Unidentified({:?})", c),
        Key::Dead(c) => format!("Dead({:?})", c),
    };
    InputEvent::Key {
        window_id,
        code,
        logical,
        pressed: ev.state == ElementState::Pressed,
        repeat: ev.repeat,
    }
}
