//! `NSError` classification for ScreenCaptureKit failures.
//!
//! The previous code mapped every SCK failure to
//! `PlatformError::PermissionDenied`, which hid real bugs (a window
//! that vanished mid-capture is not a permission problem). This
//! module classifies by the error's domain and code first, and only
//! falls back to a description heuristic for errors whose domain we
//! do not recognise.
//!
//! Known codes (`SCStreamErrorCode`, `SCStreamErrorDomain` =
//! `com.apple.ScreenCaptureKit.SCStreamErrorDomain`):
//! * `-3801 UserDeclined`, `-3803 MissingEntitlements` -> permission.
//!   `UserDeclined` is what the user declining / TCC denying the
//!   Screen Recording prompt surfaces as; `MissingEntitlements` is
//!   the entitlement variant of the same class of problem.
//! * `-3813 NoWindowList`, `-3815 NoCaptureSource` -> the window is
//!   gone (when a window id is in scope) or a generic OS error.
//! * Everything else in that domain -> `Os`, with domain/code in
//!   the message so the next person can extend this table.
//!
//! The domain/code table is only as good as Apple's documentation
//! and our (so far zero) runtime observations; the description
//! heuristic is a second net, and `MACOS_VERIFICATION.md` asks the
//! on-device tester to report the exact domain/code they see.

use objc2_foundation::NSError;
use objc2_screen_capture_kit::{SCStreamErrorCode, SCStreamErrorDomain};

use crate::types::{PlatformError, WindowId};

/// "domain (code): description" for one NSError.
pub fn describe_ns_error(error: &NSError) -> String {
    format!(
        "{} ({}): {}",
        error.domain(),
        error.code(),
        error.localizedDescription()
    )
}

/// Classify a live NSError.
pub fn classify_ns_error(error: &NSError, window: Option<WindowId>) -> PlatformError {
    let (domain, code, description) = (
        error.domain().to_string(),
        error.code(),
        error.localizedDescription().to_string(),
    );
    let detail = format!("{domain} ({code}): {description}");
    // SAFETY: reading the framework's error-domain constant.
    let sc_domain = unsafe { SCStreamErrorDomain.to_string() };
    if domain == sc_domain {
        let code = SCStreamErrorCode(code);
        if code == SCStreamErrorCode::UserDeclined || code == SCStreamErrorCode::MissingEntitlements
        {
            return PlatformError::PermissionDenied(format!(
                "ScreenCaptureKit refused the capture: {detail}. \
                 See docs/PERMISSIONS.md (Screen Recording permission)"
            ));
        }
        if code == SCStreamErrorCode::NoWindowList || code == SCStreamErrorCode::NoCaptureSource {
            if let Some(id) = window {
                return PlatformError::WindowNotFound(id);
            }
            return PlatformError::Os(format!("no capture source: {detail}"));
        }
        return PlatformError::Os(format!("ScreenCaptureKit error: {detail}"));
    }
    // Unrecognised domain: fall back to the description.
    let lower = description.to_lowercase();
    if lower.contains("permission")
        || lower.contains("not authorized")
        || lower.contains("declined")
        || lower.contains("screen recording")
    {
        return PlatformError::PermissionDenied(format!(
            "ScreenCaptureKit failed: {detail}. See docs/PERMISSIONS.md"
        ));
    }
    PlatformError::Os(format!("ScreenCaptureKit error: {detail}"))
}

/// Classify a raw NSError pointer from a completion handler
/// (`*mut NSError`, null on success).
///
/// # Safety
/// `error` must be null or point to a valid `NSError` for the
/// duration of the call (what ScreenCaptureKit guarantees inside a
/// completion handler).
pub unsafe fn classify_block_error(error: *mut NSError, window: Option<WindowId>) -> PlatformError {
    if error.is_null() {
        return PlatformError::Os("ScreenCaptureKit reported an unknown error".into());
    }
    classify_ns_error(unsafe { &*error }, window)
}
