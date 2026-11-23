# window-stream-platform

OS adapters for window-stream, behind common traits. This crate is
the `platform` crate of the workspace described in the repository
root's `docs/IMPLEMENTATION.md`; it is kept standalone (its own
empty `[workspace]` table) so it can be built and checked without
the rest of the workspace.

## Contents

* `src/types.rs` -- `WindowInfo` (id, title, position, size),
  `CapturedFrame` (BGRA or I420), `CaptureConfig`, key / pointer
  event types, `PlatformError`.
* `src/traits.rs` -- `WindowEnumerator`, `WindowCapturer` /
  `CaptureSession` (frame delivery by callback), `InputInjector`,
  and the composed `PlatformBackend`. Entry point:
  `create_backend()`.
* `src/convert.rs` -- reference BGRA -> I420 conversion (BT.601
  limited range), unit-tested.
* `src/macos/` -- macOS backend:
  * enumeration + per-window capture: ScreenCaptureKit
    (`objc2-screen-capture-kit`), one `SCStream` per window with an
    `initWithDesktopIndependentWindow` content filter;
  * input injection: CoreGraphics `CGEvent` (`core-graphics` crate),
    posted at the HID tap.
* `src/stub.rs` -- non-macOS backend: empty enumeration, explicit
  `Unsupported` errors for capture/input. Exists so the crate builds
  and tests on Linux; never fakes frames.
* `src/bin/platform_probe.rs` -- manual sample binary: list windows,
  or capture N frames of one window to raw files.

## Usage

```sh
cargo check            # any OS (macOS code is cfg-gated)
cargo test             # conversion + trait-surface tests
cargo run --bin platform_probe -- --list
cargo run --bin platform_probe -- --window <id> --frames 5 --out ./capture-out
```

## Permissions and verification status

macOS Screen Recording permission is required for enumeration and
capture, Accessibility for input injection -- see
[`docs/PERMISSIONS.md`](docs/PERMISSIONS.md).

The macOS backend was written on a Linux machine and has **not**
been run on a Mac. What was and was not verified, and the exact
steps to verify it on a real machine (m1), are in
[`docs/MACOS_VERIFICATION.md`](docs/MACOS_VERIFICATION.md).
