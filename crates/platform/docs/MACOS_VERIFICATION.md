# macOS verification

## What has been verified, and where

| Check | Environment | Result |
|---|---|---|
| `cargo check`, `cargo test` (4 tests, BGRA -> I420 incl. truncated input) | Linux VM, Rust 1.100.0, stub backend | **Passed** after the NEEDS-FIXES round (see below). `platform_probe --list` on the stub prints `backend: stub` / `0 window(s)`; capture correctly fails with an `Unsupported` error (exit 2). |
| `cargo check --target x86_64-apple-darwin` | Linux VM, cross target, no macOS SDK / linker | **Passed** after the NEEDS-FIXES round. This type-checks the full macOS configuration (ScreenCaptureKit enumeration/capture, CGEvent input) against the real `objc2` bindings; it does not link or run anything, so it is not evidence the code works on a Mac. It caught real API-shape errors during development and again in the fix round (missing `Hash` on `PointerButton`, extern-static access for `SCStreamErrorDomain`, clippy's `not_unsafe_ptr_arg_deref` on the error classifier), all fixed before commit. |
| `cargo check --target aarch64-apple-darwin` | Linux VM, cross target | **Passed** after the NEEDS-FIXES round (the m1 target architecture). Same caveat as above. |
| `cargo clippy` (Linux + both darwin targets), `cargo doc --no-deps` | Linux VM | **Passed**, zero warnings, after the NEEDS-FIXES round. |
| NEEDS-FIXES round (B1 + S1-S10 + optional items) | Linux VM, cross-checked only | All items addressed in the fix commit on `track/platform-mac` (see git log). The Y-flip fix, filter switch, Retina scaling, worker-thread conversion, delegate, error classification, input-state tracking and 60s timeout are implemented and type-checked; none of them has run on a Mac. |
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
#
#    Y-flip check (the B1 fix): move the pointer to a known point
#    near the TOP edge of a window (e.g. its title bar, y close to
#    the window's top in --list output) and confirm the cursor lands
#    there, not mirrored near the BOTTOM edge. Then click a control
#    near the top-left and one near the bottom-left of a window and
#    confirm both hit. On a multi-display setup, repeat for a window
#    on the second display.
#
#    release_all check (S7): hold a key down via inject_key, then
#    call release_all and confirm the key does not repeat / stick.
#
#    Scroll sign check: scroll with dy > 0 and confirm the content
#    scrolls up; dy < 0 scrolls down. Report if the sign is reversed.

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
* Retina / scaled displays (S2 fix): the default output size is
  now the window's point size multiplied by
  `SCShareableContentInfo.pointPixelScale`. Confirm in step 3 that
  `frames.txt` on a Retina display reports about 2x the point size
  from `--list` (e.g. a 800x600 pt window -> ~1600x1200 px frames),
  not the point size itself.
* Filter choice (S1 fix): capture now uses a display filter
  including exactly the target window
  (`initWithDisplay:includingWindows:`), after the earlier
  `initWithDesktopIndependentWindow` filter was judged wrong for
  ordinary app windows (it targets windows not attached to a
  desktop/space). Confirm step 3 captures a normal app window
  (Safari / Terminal / editor), not only floating panels.
* Stream death (S4 fix): close the captured window mid-capture and
  confirm `stream_error()` reports the stop (probe does not expose
  this yet -- a tiny caller is needed) and that no frames arrive
  after it.
* Error classification (S5 fix): deny Screen Recording, run
  `--list`, and record the exact error text; it should say
  permission denied, not a generic OS error. Then capture a window
  and close it mid-capture; the error should name the window, not
  permission.
* Window close / minimise during capture, and permission revocation
  mid-session: no handling is implemented or tested in v1.
