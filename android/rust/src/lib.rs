//! Minimal JNI bridge for the window-stream Android track.
//!
//! This crate is intentionally independent of the (future) shared
//! `crates/` workspace described in `docs/IMPLEMENTATION.md`: the
//! Android track builds it on its own with cargo-ndk so the Kotlin
//! shell can be verified end to end before the shared core exists.
//! When the shared core lands, this crate becomes the thin JNI shim
//! in front of it and the exported symbols below stay the same.
//!
//! Exported JNI functions (class `net.windowstream.android.NativeBridge`,
//! which is a Kotlin `object`, so the functions are looked up on the
//! singleton class):
//!  - `nativeVersion()` -> String
//!  - `nativeProtocolVersion()` -> int

use jni::objects::JClass;
use jni::sys::{jint, jstring};
use jni::JNIEnv;

/// Core version string, surfaced verbatim on the diagnostics screen.
pub const CORE_VERSION: &str = concat!("window-stream-core ", env!("CARGO_PKG_VERSION"));

/// Protocol version. v1 is fixed by the design: five messages, VP9 only.
pub const PROTOCOL_VERSION: jint = 1;

#[no_mangle]
pub extern "system" fn Java_net_windowstream_android_NativeBridge_nativeVersion(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    match env.new_string(CORE_VERSION) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_net_windowstream_android_NativeBridge_nativeProtocolVersion(
    _env: JNIEnv,
    _class: JClass,
) -> jint {
    PROTOCOL_VERSION
}
