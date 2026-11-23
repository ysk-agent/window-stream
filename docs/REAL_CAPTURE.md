# Real-capture end-to-end verification (macOS)

How to verify the real frame path on a Mac:

```text
ScreenCaptureKit (per-window, BGRA)
  -> crates/e2e RealCaptureSource (BGRA -> I420, odd sizes cropped)
  -> VP9 encode (AckGated, bitrate scaled to frame size)
  -> proto Frame datagrams over WebTransport
  -> VP9 decode -> client FrameSink (CountingSink headless, or
     Compositor with --display)
  -> FrameAck -> encoder reference constraint
```

This is the part of track/real-capture that **cannot** run on the
development VM or in CI: it needs a GUI session and the Screen
Recording grant. Everything up to it — the `FrameSource` seam, the
BGRA -> I420 conversion, the encode / fragment / decode / ack path
fed by captured frames — is verified in-process on Linux / CI
(`crates/e2e/tests/inprocess.rs`, `bgra_captured_frames_end_to_end_inprocess`).

## What is already verified, and what is not

| Step | Status |
|---|---|
| ScreenCaptureKit enumeration on a real Mac (`platform_probe --list`: 132 windows) | Verified 2026-11-23 (m1, macOS 15.7.7) |
| ScreenCaptureKit capture on a real Mac (`platform_probe --window <id> --frames 10`: 3360x2100 BGRA frames, i.e. a Retina 2x window) | Verified 2026-11-23 |
| BGRA -> I420 conversion (unit tests, platform crate) | Verified on Linux + CI |
| Captured BGRA frames -> VP9 -> WebTransport datagrams -> decode -> sink, in-process | Verified on Linux + CI (this track) |
| Pattern source -> VP9 -> real WebTransport -> decode -> sink (network path) | Verified in CI (core workflow) |
| **Real capture -> VP9 -> real WebTransport -> decode -> client display, on a Mac** | **Not verified — this procedure** |
| `platform_probe` capture of a window that is resized / closed mid-capture; permission revocation mid-session | Not verified (v1 has no handling; see "Known limits") |

## Prerequisites

- A Mac with macOS 13 or newer (ScreenCaptureKit per-window
  capture needs 12.3+; the Retina backing-scale refinement needs
  14+, on 13 capture falls back to point size — see
  `crates/platform/docs/MACOS_VERIFICATION.md`).
- Rust stable (`rustup`), Xcode Command Line Tools
  (`xcode-select --install`).
- **A GUI session.** Screen Recording permission is granted per
  binary / terminal, and the system prompt can only appear in a
  GUI session. Do **not** run this over SSH: an SSH-launched
  process cannot show the prompt and fails (on the m1 verification
  run, SSH-launched capture failed with TCC `-3801`, correctly
  classified as permission denied). Launch Terminal.app (or your
  terminal of choice) on the Mac itself and run every command below
  from there.
- The Mac used for development so far (m1) is on a tethered mobile
  connection and may be offline. Do **not** retry SSH repeatedly
  if it does not answer, and do **not** build heavy crates on it
  over tethering: the probe binary is built by the `platform-mac`
  CI workflow (artifact `platform_probe-macos-arm64`), and the e2e
  binaries should be built once, in one session, not rebuilt
  repeatedly.

## Permissions

See `crates/platform/docs/PERMISSIONS.md` for the full notes. In
short:

1. **Screen Recording** (required for enumeration and capture):
   System Settings > Privacy & Security > Screen Recording
   (macOS 15: "Screen & System Audio Recording"), enable the entry
   for your terminal, then **restart the terminal** — TCC decisions
   are cached per running process.
2. **Accessibility** is only needed for input injection, not for
   this capture verification — skip it here.

The first `platform_probe --list` run normally triggers the system
prompt. If no prompt appears and enumeration returns redacted
titles / an empty list, the grant is missing: check the settings
pane, restart the terminal, and run again.

## Procedure

Work in a GUI terminal on the Mac. Build once (or use the CI
artifacts where available):

```sh
git clone https://github.com/ysk-agent/window-stream.git
cd window-stream
git checkout track/real-capture
cargo build -p window-stream-e2e --bin e2e-host --bin e2e-client
cargo build -p window-stream-platform --bin platform_probe
```

### 1. Enumerate windows and pick one

```sh
./target/debug/platform_probe --list
# Expected: one line per capturable window with a non-zero id,
# app name, title, and bounds. An empty list with no error is a failure.
```

Pick a window that is on screen and has visible content (e.g. a
browser or editor window), and note its id.

### 2. Sanity-check capture alone (no streaming yet)

```sh
./target/debug/platform_probe --window <id> --frames 5 --out ./capture-out
cat capture-out/frames.txt
# Expected: 5 files, each width*height*4 bytes (allowing for stride
# padding recorded in frames.txt). On a Retina display the frame
# size is about 2x the point size from --list (e.g. a 1680x1050 pt
# window -> ~3360x2100 px frames), not the point size itself.
```

### 3. Real capture -> VP9 -> WebTransport -> decode (headless)

In one GUI terminal, start the host with the real source:

```sh
./target/debug/e2e-host --port 4433 --frames 30 --source real --window <id> --fps 30
# Expected: "cert-hash: <hex>" then "listening: https://127.0.0.1:4433",
# then "done: frames_encoded=30 frames_sent=30 ... acks_received=[0, 1, ..., 29]"
```

In a second GUI terminal, run the headless client against it
(copy the cert hash from the host output):

```sh
./target/debug/e2e-client --url https://127.0.0.1:4433 --cert-hash <hex> --frames 30
# Expected: "done(network): ... decoded=[0, 1, ..., 29] decode_errors=0
# acks_sent=[0, 1, ..., 29] sink_frames=30"
# Note: min_psnr_db is 0.0 here by design — the network client has no
# reference frame for real captured content, so no PSNR is computed
# (see crates/e2e/src/lib.rs: receive_decode_ack).
```

The `Window` the client receives carries the captured window's
pixel size and title (check the host log line
`e2e-host sending VP9 frames`, which records `width`, `height`
and `title`). The bitrate is scaled to the frame size
(`bitrate_for_size`: 800 kbit/s at 640x360, up to 8000 kbit/s for
a 3360x2100 Retina capture).

### 4. Real capture -> client display

Same host as step 3, then:

```sh
./target/debug/e2e-client --display --frames 30 --hold-secs 5 \
  --url https://127.0.0.1:4433 --cert-hash <hex>
# Expected: a client window opens showing the captured window's
# content, live, for 30 frames; "display done:
# frames_submitted_to_compositor=30".
```

Confirm by eye that the displayed content matches the captured
window (text legible, colours right — a wrong BGRA channel order
or a flipped image shows up immediately here, and nowhere else).

### 5. Report back

Record: macOS version, window app + point size + pixel size
(from `frames.txt` / the host log), the host `done:` line, the
client `done(network):` line, and whether the displayed content
matched. Any failure: paste the exact error text (permission
failures name the permission; a missing window names the id).

## Troubleshooting

| Symptom | Likely cause / fix |
|---|---|
| `list_windows` fails / empty list / redacted titles | Screen Recording not granted to this terminal. Grant it in System Settings, restart the terminal, run again. |
| `start_capture` fails with permission denied / TCC `-3801` | Same as above — or the binary was launched from SSH / a context that cannot show the prompt. Run from a GUI terminal. |
| `window <id> not found` | The window closed, or the id came from a different session. Re-run `--list` and pick a fresh id. |
| Host bails with `window resized mid-stream` | The window was resized while streaming. v1 has no mid-stream encoder resize: restart `e2e-host` (it re-reads the size from the first frame). Do not resize the window during a run. |
| Host stalls: no frames for 5 s, then `timed out ... waiting for a captured frame` | The capture stream died or the window closed/minimised. Check the error: if it names a stream error, that is the OS stream death (`stream_error()`); restart the host. |
| Decoded frames arrive but the display is garbled / wrong colours | BGRA channel order or stride handling in the capture copy path — report the `frames.txt` strides and a screenshot. |
| `--source real` fails immediately on Linux / CI | Expected: the stub backend has no windows, so it fails fast with `window <id> not found`. That is the designed behaviour, not a bug. |

## Known limits (v1, by design)

- No mid-stream resize: the encoder is sized from the first
  frame; a resize stops the host with an error (restart it).
- Odd pixel sizes are cropped by one row / column to even I420
  dimensions (see `crates/e2e/src/source.rs`).
- Frame pacing is the capture rate (up to `--fps`, default 30);
  a stale frame is dropped at the depth-1 channel, never queued.
- No FEC, no retransmission, VP9 fixed — unchanged from the
  frozen design decisions (`README.md`).
- The e2e crate cannot be cross-checked for macOS from the Linux
  VM: `cargo check --target aarch64-apple-darwin -p window-stream-e2e`
  fails compiling `ring`'s C code (it needs a macOS SDK, not the
  Linux `cc`). That is a pre-existing limit of the transport
  dependency, not of this track — the macOS build of `e2e-host` /
  `e2e-client` is verified by building on the Mac in the procedure
  above (the platform crate alone, which has no such dependency,
  is what the `platform-mac` CI workflow cross-checks).
