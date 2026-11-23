//! Input injection via CoreGraphics `CGEvent` (the `core-graphics`
//! crate).
//!
//! Events are created from an HID-system-state event source and
//! posted at the HID tap location, which is the path that reaches
//! the focused application like real hardware input. This requires
//! Accessibility permission for the posting process (and, depending
//! on macOS version, Input Monitoring); see `docs/PERMISSIONS.md`.
//!
//! Coordinate systems: [`PointerEvent`] coordinates are top-left
//! origin (the same global space as [`crate::types::WindowInfo::bounds`]), but
//! Quartz global coordinates -- what `CGEvent` wants -- are
//! bottom-left origin. Every pointer event therefore goes through
//! [`to_quartz`], which flips `y` against the top of the virtual
//! desktop (`y_quartz = global_top - y`). Getting this wrong flips
//! every click vertically, which is exactly the bug this conversion
//! exists to prevent; step 4 of `docs/MACOS_VERIFICATION.md` checks
//! it on hardware.
//!
//! Pressed-state tracking: `release_all` must not be a no-op (the
//! trait promises no stuck keys), so this module tracks pressed keys
//! and buttons process-wide in a `Mutex` and synthesises the
//! matching releases. The host runs a single backend per process,
//! so process-wide state is the right scope for v1.
//!
//! UNVERIFIED ON HARDWARE -- see `mod.rs`.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use core_graphics::display::CGDisplay;
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

/// Top of the virtual desktop in Quartz coordinates: the highest
/// `origin.y + height` over all active displays.
fn global_top_quartz() -> Option<f64> {
    let ids = CGDisplay::active_displays().ok()?;
    let mut top = f64::NEG_INFINITY;
    for id in ids {
        let bounds = CGDisplay::new(id).bounds();
        top = top.max(bounds.origin.y + bounds.size.height);
    }
    if top.is_finite() {
        Some(top)
    } else {
        None
    }
}

/// Convert a top-left-origin point to Quartz (bottom-left) coordinates.
///
/// Falls back to the main display's height when the display list is
/// unavailable; in that case multi-display `y` may be off, but the
/// common single-display case stays correct.
fn to_quartz(x: f64, y: f64) -> CGPoint {
    let top = global_top_quartz().unwrap_or_else(|| {
        let bounds = CGDisplay::main().bounds();
        bounds.origin.y + bounds.size.height
    });
    CGPoint::new(x, top - y)
}

// ---------------------------------------------------------------------------
// Pressed-state tracking (S7).

#[derive(Debug, Default)]
struct PressedState {
    keys: HashSet<u16>,
    buttons: HashSet<PointerButton>,
    last_position: Option<(f64, f64)>,
}

static PRESSED: OnceLock<Mutex<PressedState>> = OnceLock::new();

fn pressed() -> &'static Mutex<PressedState> {
    PRESSED.get_or_init(|| Mutex::new(PressedState::default()))
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
    // Record only on success: the event reached the OS.
    if let Ok(mut state) = pressed().lock() {
        if event.down {
            state.keys.insert(event.key_code);
        } else {
            state.keys.remove(&event.key_code);
        }
    }
    Ok(())
}

/// Map a pointer button to a `CGMouseButton`, or fail loudly.
/// `Other(n)` has no Quartz equivalent and must not silently become
/// Center (the old behaviour).
fn cg_button(b: PointerButton) -> Result<CGMouseButton> {
    match b {
        PointerButton::Left => Ok(CGMouseButton::Left),
        PointerButton::Right => Ok(CGMouseButton::Right),
        PointerButton::Center => Ok(CGMouseButton::Center),
        PointerButton::Other(n) => Err(PlatformError::Unsupported(format!(
            "pointer button {n} has no CGEvent equivalent on macOS \
             (only left/right/center are supported)"
        ))),
    }
}

pub fn inject_pointer(event: PointerEvent) -> Result<()> {
    match event {
        PointerEvent::Move { x, y } => {
            let cg = CGEvent::new_mouse_event(
                source()?,
                CGEventType::MouseMoved,
                to_quartz(x, y),
                CGMouseButton::Left,
            )
            .map_err(|_| PlatformError::Os("CGEventCreateMouseEvent (move) failed".into()))?;
            post(cg);
            if let Ok(mut state) = pressed().lock() {
                state.last_position = Some((x, y));
            }
        }
        PointerEvent::ButtonDown { button, x, y } => {
            let kind = match button {
                PointerButton::Left => CGEventType::LeftMouseDown,
                PointerButton::Right => CGEventType::RightMouseDown,
                _ => CGEventType::OtherMouseDown,
            };
            let cg = CGEvent::new_mouse_event(source()?, kind, to_quartz(x, y), cg_button(button)?)
                .map_err(|_| PlatformError::Os("CGEventCreateMouseEvent (down) failed".into()))?;
            post(cg);
            if let Ok(mut state) = pressed().lock() {
                state.buttons.insert(button);
                state.last_position = Some((x, y));
            }
        }
        PointerEvent::ButtonUp { button, x, y } => {
            let kind = match button {
                PointerButton::Left => CGEventType::LeftMouseUp,
                PointerButton::Right => CGEventType::RightMouseUp,
                _ => CGEventType::OtherMouseUp,
            };
            let cg = CGEvent::new_mouse_event(source()?, kind, to_quartz(x, y), cg_button(button)?)
                .map_err(|_| PlatformError::Os("CGEventCreateMouseEvent (up) failed".into()))?;
            post(cg);
            if let Ok(mut state) = pressed().lock() {
                state.buttons.remove(&button);
                state.last_position = Some((x, y));
            }
        }
        PointerEvent::Scroll { x, y, dx, dy } => {
            // Move to the target point first: scroll events are
            // delivered to the window under the cursor.
            inject_pointer(PointerEvent::Move { x, y })?;
            // CGEventCreateScrollWheelEvent2: wheel1 is the vertical
            // axis, wheel2 the horizontal axis; positive wheel1
            // scrolls up, positive wheel2 scrolls right.
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

/// Release everything tracked as pressed: synthesise a key-up for
/// every key still down and a button-up for every button still down
/// (at the last known pointer position). Best effort -- failures are
/// logged, not returned, because this runs on teardown where there
/// is nothing useful the caller could do with the error; the state
/// is cleared regardless so a later call does not re-release.
pub fn release_all() -> Result<()> {
    let state = match pressed().lock() {
        Ok(mut guard) => std::mem::take(&mut *guard),
        Err(poisoned) => {
            tracing::error!("input pressed-state Mutex poisoned; recovering state");
            std::mem::take(&mut *poisoned.into_inner())
        }
    };
    for key_code in state.keys {
        match CGEvent::new_keyboard_event(
            match source() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(key_code, "release_all: no event source: {e}");
                    continue;
                }
            },
            key_code,
            false,
        ) {
            Ok(cg) => post(cg),
            Err(_) => tracing::warn!(key_code, "release_all: key-up event creation failed"),
        }
    }
    if let Some((x, y)) = state.last_position {
        for button in state.buttons {
            let kind = match button {
                PointerButton::Left => CGEventType::LeftMouseUp,
                PointerButton::Right => CGEventType::RightMouseUp,
                _ => CGEventType::OtherMouseUp,
            };
            let Ok(cg_button) = cg_button(button) else {
                continue;
            };
            match CGEvent::new_mouse_event(
                match source() {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("release_all: no event source: {e}");
                        break;
                    }
                },
                kind,
                to_quartz(x, y),
                cg_button,
            ) {
                Ok(cg) => post(cg),
                Err(_) => tracing::warn!(?button, "release_all: button-up event creation failed"),
            }
        }
    } else if !state.buttons.is_empty() {
        tracing::warn!(
            "release_all: {} button(s) pressed but no known pointer position; \
             button-up events skipped",
            state.buttons.len()
        );
    }
    Ok(())
}
