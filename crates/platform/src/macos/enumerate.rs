//! Window enumeration via `SCShareableContent`.
//!
//! `SCShareableContent` queries are asynchronous (completion-handler
//! based). This module bridges them to a blocking call with a
//! channel and a timeout, extracting plain [`WindowInfo`] values
//! inside the handler so no Objective-C object crosses threads.
//! The one exception is [`fetch_window`], which must hand an
//! `SCWindow` to the capture code; it wraps the retained object in
//! an explicitly-`Send` carrier (single transfer, then used from
//! one thread only).

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{SCShareableContent, SCWindow};

use crate::types::{PlatformError, Rect, Result, WindowId, WindowInfo};

const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Carrier that moves a retained `SCWindow` from the completion
/// handler to the calling thread exactly once.
///
/// Safety: `SCWindow` is an immutable descriptor object; we never
/// share it between threads concurrently -- the channel transfer is
/// a hand-off, after which only the receiving thread touches it.
pub struct SendWindow(pub Retained<SCWindow>);
unsafe impl Send for SendWindow {}

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

fn describe_error(error: *mut NSError) -> String {
    if error.is_null() {
        return "unknown ScreenCaptureKit error".to_string();
    }
    // SAFETY: ScreenCaptureKit hands us a valid NSError for the
    // duration of the completion handler.
    unsafe {
        let error = &*error;
        error.localizedDescription().to_string()
    }
}

/// Run one shareable-content query and pass its windows to `f`
/// *inside* the completion handler (so `f` may retain objects).
fn with_shareable_windows<T: Send + 'static>(
    f: impl FnOnce(&[Retained<SCWindow>]) -> T + Send + 'static,
) -> Result<T> {
    let (tx, rx) = mpsc::channel::<std::result::Result<T, String>>();
    // Completion handlers are `Fn`, not `FnMut`: keep the one-shot
    // closure behind a Mutex so it can be taken through a shared ref.
    let f = std::sync::Mutex::new(Some(f));
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            if !error.is_null() || content.is_null() {
                let _ = tx.send(Err(describe_error(error)));
                return;
            }
            // SAFETY: non-null content is a valid SCShareableContent
            // for the duration of the handler.
            let windows: Vec<Retained<SCWindow>> = unsafe {
                let content = &*content;
                let array = content.windows();
                (0..array.count()).map(|i| array.objectAtIndex(i)).collect()
            };
            let take = f
                .lock()
                .ok()
                .and_then(|mut guard| guard.take())
                .expect("completion handler called once");
            let _ = tx.send(Ok(take(&windows)));
        },
    );
    // SAFETY: the block matches the declared completion-handler type
    // and stays alive (RcBlock + the recv below) until it has run.
    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&block);
    }
    match rx.recv_timeout(QUERY_TIMEOUT) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(msg)) => Err(PlatformError::PermissionDenied(format!(
            "ScreenCaptureKit shareable-content query failed: {msg}. \
             This usually means Screen Recording permission has not been \
             granted to this binary (System Settings > Privacy & Security \
             > Screen Recording); see docs/PERMISSIONS.md"
        ))),
        Err(_) => Err(PlatformError::Timeout(
            "SCShareableContent query did not complete".into(),
        )),
    }
}

/// List all capturable windows.
pub fn list_windows() -> Result<Vec<WindowInfo>> {
    with_shareable_windows(|windows| windows.iter().map(|w| window_info(w)).collect())
}

/// Fetch (and retain) the `SCWindow` for `id`, for capture setup.
pub fn fetch_window(id: WindowId) -> Result<SendWindow> {
    let found = with_shareable_windows(move |windows| {
        windows
            .iter()
            .find(|w| unsafe { w.windowID() } == id.0)
            .map(|w| SendWindow(w.clone()))
    })?;
    found.ok_or(PlatformError::WindowNotFound(id))
}
