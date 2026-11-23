# window-stream Android viewer

The display-side client for Android: package `net.windowstream.viewer`,
a separate app from the Android host (`net.windowstream.android`).

Why separate: the host needs MediaProjection and accessibility
permissions plus a foreground service; the viewer needs only
`INTERNET`. The two apps share the Rust core crates — this viewer
links `crates/client` (the wgpu Compositor behind the `FrameSink`
seam), `crates/transport` (WebTransport), `crates/codec` (libvpx VP9
decode) and `crates/proto` — but no code is shared at the package
level.

## Layout

- `app/` — Gradle project. `MainActivity` is the connection form;
  `ViewerActivity` is a GameActivity whose native glue loads
  `libwindowstream_viewer.so` and calls its `android_main`.
- `rust/` — standalone cdylib crate (built with cargo-ndk, like
  `android/rust`), path-depending on the shared crates. `src/lib.rs`
  is the entry point and config loading, `src/pipeline.rs` the frame
  producers (network client + in-process demo source), `src/app.rs`
  the winit shell that drains sink events into the Compositor.
- `scripts/build-libvpx.sh` — builds the static PIC libvpx per ABI
  that the codec crate links via pkg-config (CI only; there is no
  system libvpx for Android).

## Using it

1. Install the APK (CI artifact `window-stream-android-viewer-debug`).
2. Open the app. Either:
   - **Connect**: enter the host URL (`https://<host-ip>:<port>`) and
     the certificate hash the host prints (`cert-hash: ...`, 64 hex
     chars), then tap Connect. The values are written to
     `viewer_config.json` in the app's internal storage, which the
     Rust side reads at startup; they are also kept in
     SharedPreferences to prefill the form next time.
   - **Demo pattern (no host)**: renders the VP9 test pattern through
     the full codec + compositor path on the device GPU. Use this to
     smoke-test a fresh install.

Remote windows are fit into the screen preserving aspect ratio
(title bar included, as on the desktop client) and refit on resize /
rotation.

## Known limitations (v1)

- **No touch input yet.** Display first; touch -> host `Input`
  datagrams are the next stage.
- **The host does not listen on the LAN yet.** `e2e-host` currently
  binds `127.0.0.1` only, so today the network mode can be exercised
  against a host reachable at a loopback-equivalent address (e.g.
  `adb reverse` / emulator aliasing), not from a phone on Wi-Fi. A
  `--bind` option plus a LAN SAN on the host side is the remaining
  step for a real phone-to-Mac run; it is deliberately not part of
  this track.
- The wgpu surface is created once at `resumed`; a suspend/resume
  cycle that replaces the native window may require restarting the
  app.
- The viewer runs until the host closes the session (the current
  `e2e-host` serves a fixed `--frames` budget per session).

## Building

Gradle builds run on GitHub Actions (`.github/workflows/android-viewer.yml`):
libvpx per ABI -> cargo-ndk (3 ABIs) -> `assembleDebug`, with the APK's
package, version and signing certificate printed in the workflow log.
