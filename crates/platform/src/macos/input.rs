//! Input injection via CoreGraphics `CGEvent` (the `core-graphics`
//! crate).
//!
//! Events are created from an HID-system-state event source and
//! posted at the HID tap location, which is the path that reaches
//! the focused application like real hardware input. This requires
//! Accessibility permission for the posting process (and, depending
//! on macOS version, Input Monitoring); see `docs/PERMISSIONS.md`.
//!
//! UNVERIFIED ON HARDWARE -- see `mod.rs`.

use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

use crate::types::{KeyEvent, PlatformError, PointerButton, PointerEvent, Result};

fn source() -> Result<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| PlatformError::Os("CGEventSourceCreate failed".into()))
}

fn post(event: CGEvent) {
    event.post(CGEventTapLocation::HID);
}

pub fn inject_key(event: KeyEvent) -> Result<()> {
    let cg = CGEvent::new_keyboard_event(source()?, event.key_code, event.down)
        .map_err(|_| PlatformError::Os("CGEventCreateKeyboardEvent failed".into()))?;
    let mut flags = CGEventFlags::CGEventFlagNull;
    if event.shift {
        flags |= CGEventFlags::CGEventFlagShift;
    }
    if event.control {
        flags |= CGEventFlags::CGEventFlagControl;
    }
    if event.alt {
        flags |= CGEventFlags::CGEventFlagAlternate;
    }
    if event.meta {
        flags |= CGEventFlags::CGEventFlagCommand;
    }
    if flags != CGEventFlags::CGEventFlagNull {
        cg.set_flags(flags);
    }
    post(cg);
    Ok(())
}

fn cg_button(b: PointerButton) -> CGMouseButton {
    match b {
        PointerButton::Left => CGMouseButton::Left,
        PointerButton::Right => CGMouseButton::Right,
        PointerButton::Center | PointerButton::Other(_) => CGMouseButton::Center,
    }
}

pub fn inject_pointer(event: PointerEvent) -> Result<()> {
    match event {
        PointerEvent::Move { x, y } => {
            let cg = CGEvent::new_mouse_event(
                source()?,
                CGEventType::MouseMoved,
                CGPoint::new(x, y),
                CGMouseButton::Left,
            )
            .map_err(|_| PlatformError::Os("CGEventCreateMouseEvent (move) failed".into()))?;
            post(cg);
        }
        PointerEvent::ButtonDown { button, x, y } => {
            let kind = match button {
                PointerButton::Left => CGEventType::LeftMouseDown,
                PointerButton::Right => CGEventType::RightMouseDown,
                _ => CGEventType::OtherMouseDown,
            };
            let cg =
                CGEvent::new_mouse_event(source()?, kind, CGPoint::new(x, y), cg_button(button))
                    .map_err(|_| {
                        PlatformError::Os("CGEventCreateMouseEvent (down) failed".into())
                    })?;
            post(cg);
        }
        PointerEvent::ButtonUp { button, x, y } => {
            let kind = match button {
                PointerButton::Left => CGEventType::LeftMouseUp,
                PointerButton::Right => CGEventType::RightMouseUp,
                _ => CGEventType::OtherMouseUp,
            };
            let cg =
                CGEvent::new_mouse_event(source()?, kind, CGPoint::new(x, y), cg_button(button))
                    .map_err(|_| PlatformError::Os("CGEventCreateMouseEvent (up) failed".into()))?;
            post(cg);
        }
        PointerEvent::Scroll { x, y, dx, dy } => {
            // Move to the target point first: scroll events are
            // delivered to the window under the cursor.
            inject_pointer(PointerEvent::Move { x, y })?;
            let cg = CGEvent::new_scroll_event(
                source()?,
                ScrollEventUnit::PIXEL,
                2,
                dy as i32,
                dx as i32,
                0,
            )
            .map_err(|_| PlatformError::Os("CGEventCreateScrollWheelEvent failed".into()))?;
            post(cg);
        }
    }
    Ok(())
}

/// Best-effort release: there is no OS "release all" primitive, and
/// this backend is stateless (it does not track pressed keys), so
/// this is a no-op here. Stateful tracking belongs to the session
/// layer, which also sends periodic input-state snapshots per the
/// protocol's stuck-key policy.
pub fn release_all() -> Result<()> {
    Ok(())
}
