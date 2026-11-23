# macOS verification

## What has been verified, and where

| Check | Environment | Result |
|---|---|---|
| `cargo check`, `cargo test` (3 tests, BGRA -> I420) | Linux VM, Rust 1.100.0, stub backend | **Passed** at commit time. `platform_probe --list` on the stub prints `backend: stub` / `0 window(s)`; capture correctly fails with an `Unsupported` error (exit 2). |
| `cargo check --target x86_64-apple-darwin` | Linux VM, cross target, no macOS SDK / linker | **Passed** at commit time. This type-checks the full macOS configuration (ScreenCaptureKit enumeration/capture, CGEvent input) against the real `objc2` bindings; it does not link or run anything, so it is not evidence the code works on a Mac. It did catch real API-shape errors during development (objc2 0.6 `define_class!` syntax, a `Fn` vs `FnMut` completion-handler mismatch, `CaptureSession: Send`), all fixed before commit. |
| `cargo check --target aarch64-apple-darwin` | Linux VM, cross target | **Passed** at commit time (the m1 target architecture). Same caveat as above. |
| `cargo clippy --target x86_64-apple-darwin` | Linux VM | **Passed**, no warnings, at commit time. |
| Window enumeration on a real Mac | -- | **Not done.** |
| Per-window capture on a real Mac | -- | **Not done.** |
| Input injection on a real Mac | -- | **Not done.** |

## On-device verification (m1, or any Apple-silicon / Intel Mac)

Prerequisites: Rust stable (`rustup`), macOS 13 or newer
(ScreenCaptureKit per-window capture needs 12.3+; this crate is
developed against 13+ behaviour), Xcode Command Line Tools
(`xcode-select --install`).

```sh
git clone https://github.com/ysk-agent/window-stream.git
cd window-stream
git checkout track/platform-mac
cd crates/platform

# 1. Type-check and test the crate itself (stub parts included).
cargo check
cargo test

# 2. Enumerate windows. First run: expect the Screen Recording
#    prompt; grant it (see PERMISSIONS.md) and run again.
cargo run --bin platform_probe -- --list
#    Expected: one line per capturable window with a non-zero id,
#    app name, title (titles may be empty until permission is
#    granted), and bounds. An empty list with no error is a failure.

# 3. Capture a few frames of one window from the list.
cargo run --bin platform_probe -- --window <id> --frames 5 --out ./capture-out
ls -l capture-out        # frame-0000.bgra .. frame-0004.bgra + frames.txt
cat capture-out/frames.txt
#    Expected: 5 files, each width*height*4 bytes (allowing for
#    stride padding recorded in frames.txt), non-zero size.
#    Sanity-check one frame visually, e.g. by converting the raw
#    BGRA to PNG with ffmpeg:
#      ffmpeg -f rawvideo -pix_fmt bgra -s <WxH> -i frame-0000.bgra frame.png
#    (use the width/height/strides from frames.txt; if the stride
#    exceeds width*4, crop with ffmpeg's crop filter or re-run with
#    an explicit size.)
#
#    Repeat with --format i420 and confirm .yuv files of
#    width*height*3/2 bytes (plus any stride padding).

# 4. Input injection (no automated check ships in v1).
#    Write a tiny caller against the library traits, or extend
#    platform_probe locally: inject a pointer move and a harmless
#    key (e.g. in a text editor) and confirm the effect by eye.
#    Requires Accessibility permission (PERMISSIONS.md).

# 5. Report back: macOS version, which steps passed, any compiler
#    or linker errors in src/macos/* (those files were only
#    cross-type-checked from Linux, never compiled/linked against
#    a real macOS SDK by the original author -- small issues may
#    remain), and the frames.txt from a successful capture.
```

## Known unverified areas

* `src/macos/capture.rs`: the `define_class!` `SCStreamOutput`
  object (runtime registration and callback dispatch), the
  `CVPixelBuffer` lock/copy path (hand-declared FFI for the
  CoreVideo entry points), the `SCStreamConfiguration` property
  setters, and the completion-handler bridging for start/stop
  capture. All of this type-checks cross-target; none of it has run.
* `src/macos/enumerate.rs`: the `SCShareableContent` completion
  handler bridging and `NSArray` access pattern.
* `src/macos/input.rs`: `CGEvent` creation/posting semantics
  (compile-level API is from the documented `core-graphics` crate;
  behavioural verification -- events actually reaching another app --
  is step 4 above).
* Retina / scaled displays: capture output size defaults to the
  window's size in *points*; whether ScreenCaptureKit returns
  point- or pixel-sized frames for a Retina window, and the
  resulting stride, must be confirmed in step 3 (frames.txt records
  the actual values).
* Window close / minimise during capture, and permission revocation
  mid-session: no handling is implemented or tested in v1.
