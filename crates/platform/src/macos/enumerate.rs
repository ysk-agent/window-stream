//! Window enumeration via `SCShareableContent`.
//!
//! `SCShareableContent` queries are asynchronous (completion-handler
//! based). This module bridges them to a blocking call with a
//! channel and a timeout, extracting plain [`WindowInfo`] values
//! inside the handler so no Objective-C object crosses threads.
//! The exceptions are [`fetch_window`] / [`fetch_window_with_display`],
//! which must hand `SCWindow` / `SCDisplay` objects to the capture
//! code; they wrap the retained objects in explicitly-`Send`
//! carriers (single transfer, then used from one thread only).

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{SCDisplay, SCShareableContent, SCWindow};

use super::error::classify_block_error;
use crate::types::{PlatformError, Rect, Result, WindowId, WindowInfo};

/// First-run timeout. The Screen Recording prompt can sit in front
/// of the user for a while on the first run; 10s was easy to exceed
/// just by reading it, so the query (and the capture start, which
/// can trigger the same prompt) allows 60s. Later runs return in
/// well under a second once permission is granted.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(60);

/// Carrier that moves a retained `SCWindow` from the completion
/// handler to the calling thread exactly once.
///
/// Safety: `SCWindow` is an immutable descriptor object; we never
/// share it between threads concurrently -- the channel transfer is
/// a hand-off, after which only the receiving thread touches it.
pub struct SendWindow(pub Retained<SCWindow>);
unsafe impl Send for SendWindow {}

/// Same hand-off carrier for an `SCDisplay`.
pub struct SendDisplay(pub Retained<SCDisplay>);
unsafe impl Send for SendDisplay {}

fn ns_string(value: &objc2_foundation::NSString) -> String {
    value.to_string()
}

fn window_info(window: &SCWindow) -> WindowInfo {
    // SAFETY: all accessors are plain property reads on a live
    // SCWindow obtained from a just-completed shareable-content query.
    unsafe {
        let frame = window.frame();
        let app = window.owningApplication();
        WindowInfo {
            id: WindowId(window.windowID()),
            title: window.title().map(|t| ns_string(&t)),
            app_name: app.as_ref().map(|a| ns_string(&a.applicationName())),
            app_pid: app.as_ref().map(|a| a.processID()),
            bounds: Rect::new(
                frame.origin.x,
                frame.origin.y,
                frame.size.width,
                frame.size.height,
            ),
            layer: window.windowLayer() as i64,
            is_on_screen: window.isOnScreen(),
            is_active: window.isActive(),
        }
    }
}

/// Intersection area (in points) of two rects, for picking the
/// display that contains most of a window.
fn overlap_area(a: &Rect, b: &Rect) -> f64 {
    let x = (a.x + a.width).min(b.x + b.width) - a.x.max(b.x);
    let y = (a.y + a.height).min(b.y + b.height) - a.y.max(b.y);
    if x <= 0.0 || y <= 0.0 {
        0.0
    } else {
        x * y
    }
}

/// Run one shareable-content query and pass its windows and displays
/// to `f` *inside* the completion handler (so `f` may retain objects).
fn with_shareable_content<T: Send + 'static>(
    f: impl FnOnce(&[Retained<SCWindow>], &[Retained<SCDisplay>]) -> T + Send + 'static,
) -> Result<T> {
    let (tx, rx) = mpsc::channel::<std::result::Result<T, PlatformError>>();
    // Completion handlers are `Fn`, not `FnMut`: keep the one-shot
    // closure behind a Mutex so it can be taken through a shared ref.
    let f = std::sync::Mutex::new(Some(f));
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            if !error.is_null() || content.is_null() {
                // SAFETY: error is null or a valid NSError inside
                // this completion handler.
                let _ = tx.send(Err(unsafe { classify_block_error(error, None) }));
                return;
            }
            // SAFETY: non-null content is a valid SCShareableContent
            // for the duration of the handler.
            let (windows, displays) = unsafe {
                let content = &*content;
                let windows = {
                    let array = content.windows();
                    (0..array.count())
                        .map(|i| array.objectAtIndex(i))
                        .collect::<Vec<Retained<SCWindow>>>()
                };
                let displays = {
                    let array = content.displays();
                    (0..array.count())
                        .map(|i| array.objectAtIndex(i))
                        .collect::<Vec<Retained<SCDisplay>>>()
                };
                (windows, displays)
            };
            // The handler is documented to run once; if the one-shot
            // closure is already gone (or the Mutex is poisoned),
            // report it instead of panicking -- a panic here would
            // cross the FFI boundary into ScreenCaptureKit.
            let take = match f.lock() {
                Ok(mut guard) => guard.take(),
                Err(poisoned) => {
                    tracing::error!("shareable-content handler Mutex poisoned");
                    poisoned.into_inner().take()
                }
            };
            match take {
                Some(take) => {
                    let _ = tx.send(Ok(take(&windows, &displays)));
                }
                None => {
                    let _ = tx.send(Err(PlatformError::Os(
                        "SCShareableContent completion handler invoked more than once".into(),
                    )));
                }
            }
        },
    );
    // SAFETY: the block matches the declared completion-handler type
    // and stays alive (RcBlock + the recv below) until it has run.
    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&block);
    }
    match rx.recv_timeout(QUERY_TIMEOUT) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(match e {
            // Re-attach the hint for the common permission case.
            PlatformError::PermissionDenied(msg) => PlatformError::PermissionDenied(format!(
                "{msg} (System Settings > Privacy & Security > Screen Recording); \
                 see docs/PERMISSIONS.md"
            )),
            other => other,
        }),
        Err(_) => Err(PlatformError::Timeout(format!(
            "SCShareableContent query did not complete within {QUERY_TIMEOUT:?}"
        ))),
    }
}

/// List all capturable windows.
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    with_shareable_content(|windows, _| windows.iter().map(|w| window_info(w)).collect())
}

/// Fetch the window for `id` together with the display that contains
/// most of it. The display is needed for the
/// `SCContentFilter(initWithDisplay:includingWindows:)` filter used
/// by capture, and for the window's backing scale factor.
pub fn fetch_window_with_display(id: WindowId) -> Result<(SendWindow, SendDisplay)> {
    let (found, window_found) = with_shareable_content(move |windows, displays| {
        let window = match windows.iter().find(|w| unsafe { w.windowID() } == id.0) {
            Some(w) => w,
            None => return (None, false),
        };
        // SAFETY: property read on a live SCWindow.
        let frame = unsafe { window.frame() };
        let window_rect = Rect::new(
            frame.origin.x,
            frame.origin.y,
            frame.size.width,
            frame.size.height,
        );
        // Pick the display with the largest overlap; fall back to the
        // first display if the window overlaps none (e.g. a window
        // whose frame is stale or off-screen).
        let display = match displays
            .iter()
            .max_by(|a, b| {
                let area = |d: &SCDisplay| {
                    // SAFETY: property read on a live SCDisplay.
                    let f = unsafe { d.frame() };
                    overlap_area(
                        &window_rect,
                        &Rect::new(f.origin.x, f.origin.y, f.size.width, f.size.height),
                    )
                };
                area(a)
                    .partial_cmp(&area(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .or_else(|| displays.first())
        {
            Some(d) => d,
            // The window exists but there is no display at all: that
            // is an OS-state problem, not a missing window.
            None => return (None, true),
        };
        (
            Some((SendWindow(window.clone()), SendDisplay(display.clone()))),
            true,
        )
    })?;
    match (found, window_found) {
        (Some(pair), _) => Ok(pair),
        (None, true) => Err(PlatformError::Os(
            "SCShareableContent returned a window but no displays; \
             cannot build a display filter"
                .into(),
        )),
        (None, false) => Err(PlatformError::WindowNotFound(id)),
    }
}
