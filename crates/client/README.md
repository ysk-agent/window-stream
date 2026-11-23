# window-stream client

Client track implementation: a `winit` + `wgpu` client that composites
multiple remote windows locally as GPU textures.

Fixed design decisions (from the repository `README.md` /
`docs/IMPLEMENTATION.md`, not revisited here): window-level streaming with
client-side compositing, VP9, WebTransport.

## What is implemented

- **Compositing skeleton** (`src/compositor.rs`, `src/remote_window.rs`):
  each remote window is drawn as a quad in back-to-front list order on one
  `wgpu` surface, with a solid title bar. Window geometry is client-local.
- **I420 -> RGB in the shader** (`src/shader.wgsl`): each window owns three
  `R8Unorm` textures (Y, U, V planes). The fragment shader samples all three
  and does BT.601 limited-range conversion. There is no CPU colour
  conversion anywhere in the crate.
- **Demo mode** (`src/demo.rs`, default / `--demo`): no transport and no
  codec are connected in this track. Locally generated I420 test frames
  (gradient + moving coloured box + moving stripe, distinct per window) are
  uploaded to the same texture path a decoded VP9 frame will use.
  Three demo windows are shown.
- **Local window operations**: drag a window by its title bar to move it;
  click any window to bring it to the front (restack is a list reorder).
- **Input capture** (`src/input.rs`): keyboard, pointer move / button and
  wheel events from `winit` are converted to an internal `InputEvent`
  placeholder type (the real `proto` definitions are another track) and
  logged with `tracing`, including the target window id and window-relative
  pointer coordinates. Keys are routed to the frontmost window.

## Run

```sh
cd crates/client
cargo run -- --demo
# Smoke test without interaction (renders N frames, then exits):
cargo run -- --headless-frames 30
cargo test
```

On Linux a display is required (e.g. Xvfb: `DISPLAY=:99 cargo run -- --demo`).

## Workspace integration (for the workspace track)

This crate is currently standalone so it builds before the root workspace
manifest exists:

1. In the root `Cargo.toml`, add `"crates/client"` to `workspace.members`.
2. In `crates/client/Cargo.toml`, delete the empty `[workspace]` table (it
   is what currently detaches the crate from a parent workspace).
3. Rename the package if desired: it is currently `window-stream-client`
   to avoid colliding with a future workspace member named `client`; the
   binary target is the package's default binary.
4. When the `proto` crate lands, replace the placeholder types in
   `src/input.rs` with conversions to the generated `Input` message; that
   module is the only place that knows about `winit` event types.

## Not in this track / not yet verified

- No WebTransport session, no VP9 decode: demo frames substitute for them.
- Platform scope in the design is macOS / Android; this skeleton was built
  and run on Linux/Xvfb for development verification only.

## Verification (2026-11-23, Linux/Xvfb development VM)

- `cargo build` and `cargo test`: pass (3 tests: I420 plane sizes, demo
  animation, hit-test/restack).
- Run on Xvfb (`DISPLAY=:99`, wgpu Vulkan/llvmpipe software rendering):
  `--headless-frames 30` completed and exited cleanly; the interactive
  demo was screenshot-verified (`screenshot-demo.png`: three overlapping
  windows composited, moving box and stripes visible, title bars drawn).
- Synthetic X input via `xdotool` verified the input path end to end:
  pointer move/button events logged with the correct window id,
  click-to-front (`bring_to_front`) logged, a title-bar drag moved the
  window by the injected (+60, +40) delta, and key events were logged
  routed to the frontmost window.
- Not verified: real GPU hardware rendering, macOS/Android surfaces,
  transport/codec integration (other tracks, by design).
