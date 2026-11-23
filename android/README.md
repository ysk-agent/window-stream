# window-stream — Android track

Android shell for window-stream: a Kotlin app (`net.windowstream.android`)
around a small Rust core loaded over JNI. This track implements the
diagnostics skeleton only: the MediaProjection permission flow, the
capture foreground service, MediaCodec VP9 capability diagnostics, and
the JNI connection point. No streaming yet.

## Fixed design decisions (do not relitigate)

- The app shell is **Kotlin**. MediaProjection, MediaCodec and the
  foreground-service rules are Android framework APIs; the shell owns
  permissions and OS integration.
- The protocol/transport core is **Rust**, built with `cargo-ndk` into
  `libwindowstream_core.so` and called from Kotlin over **JNI**.
- The codec is **VP9 via MediaCodec** (hardware decode where present).
  libvpx is not used on Android.
- **No ADB.** Unlike scrcpy, nothing here rides on ADB, a pushed server
  jar, or shell privileges. Capture goes through the ordinary
  MediaProjection consent flow (see `docs/PERMISSIONS.md`).
- Android captures the whole display only, so the window model
  degenerates to one window = one display on this platform. The
  protocol is unaffected.

## Layout

```
android/
  settings.gradle.kts, build.gradle.kts   Gradle skeleton (one app module)
  app/                                    Kotlin shell
    src/main/java/net/windowstream/android/
      MainActivity.kt     Diagnostics screen + consent flow
      CaptureService.kt   MediaProjection foreground service
      CodecDiagnostics.kt MediaCodec VP9 capability report
      NativeBridge.kt     JNI bridge (loads the Rust .so, fail-soft)
  rust/                   Independent Rust crate (JNI shim for the core)
    Cargo.toml
    src/lib.rs            nativeVersion() / nativeProtocolVersion()
  docs/PERMISSIONS.md     MediaProjection consent constraints
```

## Building

Gradle builds run on **GitHub Actions** in this public repository
(project convention) — see `.github/workflows/android.yml`. Pushing to
`track/android` builds the Rust core with cargo-ndk for
`arm64-v8a`, `armeabi-v7a` and `x86_64`, packages it into
`app/src/main/jniLibs/`, runs `assembleDebug`, and uploads the APK as
a workflow artifact.

Locally (with an Android SDK, NDK, JDK 17, Rust and cargo-ndk):

```sh
cd android/rust
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 \
  -o ../app/src/main/jniLibs build --release
cd ..
./gradlew assembleDebug
# APK: app/build/outputs/apk/debug/app-debug.apk
```

The Kotlin shell tolerates a missing `.so` (the diagnostics screen
reports the Rust core as unavailable), so a Gradle-only build still
produces a runnable APK — but the CI build always includes the core.

## What the diagnostics screen shows

- Rust core version string and protocol version (proves the JNI path).
- Capture state (stopped / running).
- MediaCodec report: VP9 decoder presence, hardware vs software,
  maximum supported size, whether a decoder can be instantiated, the
  VP9 encoder list, and a one-line verdict.
