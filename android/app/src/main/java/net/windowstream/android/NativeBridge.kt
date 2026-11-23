package net.windowstream.android

/**
 * JNI bridge to the Rust core in `android/rust/` (built with cargo-ndk
 * into `libwindowstream_core.so`).
 *
 * The native library is optional for the diagnostics shell: if the .so
 * was not bundled (for example a Gradle-only build without the Rust
 * step), the app still runs and reports the core as unavailable instead
 * of crashing on load.
 */
object NativeBridge {

    private val loadError: Throwable? = try {
        System.loadLibrary("windowstream_core")
        null
    } catch (t: Throwable) {
        t
    }

    val isLoaded: Boolean get() = loadError == null

    /** Implemented in Rust: `Java_net_windowstream_android_NativeBridge_nativeVersion`. */
    external fun nativeVersion(): String

    /** Implemented in Rust: protocol version as an integer (currently 1). */
    external fun nativeProtocolVersion(): Int

    fun versionOrUnavailable(): String {
        if (!isLoaded) {
            return "unavailable (native library not loaded: ${loadError?.javaClass?.simpleName}: ${loadError?.message})"
        }
        return try {
            "${nativeVersion()} (protocol v${nativeProtocolVersion()})"
        } catch (t: Throwable) {
            "unavailable (JNI call failed: ${t.message})"
        }
    }
}
