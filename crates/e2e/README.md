# window-stream-e2e

End-to-end wiring of the real frame path, built on the existing crates
without changing any of their responsibilities:

```
source::FrameSource (PatternSource by default; RealCaptureSource
  pulls ScreenCaptureKit BGRA on macOS and converts it to I420)
  -> codec::Vp9Encoder (AckGated)
  -> proto::fragment_frame (Frame datagrams)
  -> transport::Session datagrams (WebTransport/QUIC)
     [or the in-memory datagram loop in run_inprocess]
  -> proto::FrameReassembler -> codec::Vp9Decoder
  -> client::sink::FrameSink (Compositor, or CountingSink headless)
  -> proto FrameAck datagram -> host VideoEncoder::set_reference_constraint
     (a gap in acks calls invalidate_reference() promptly)
```

Why a new crate: `dummy`'s contract is a protocol round trip with
payloads standing in for VP9 (no codec); `client`'s contract is
compositing behind `FrameSink`. The wiring owns neither, so it is a
third crate depending one-way on all of them, in the shape of
`docs/IMPLEMENTATION.md` (`host / client -> transport / codec -> proto`).
The VP9 encoder/decoder are `!Send`, so the host runs on a dedicated
thread with a current-thread runtime (encode on dedicated threads;
tokio does I/O only).

## Binaries

- `e2e-host [--port 4433] [--frames 30] [--source pattern|real] [--window <id>] [--fps 30]` — WebTransport host (VP9).
  `--source real` captures one macOS window via ScreenCaptureKit
  (BGRA -> I420, Retina sizes included); on Linux / CI the stub
  backend fails fast instead of faking frames. See
  `docs/REAL_CAPTURE.md` for the on-device procedure.
- `e2e-client` — headless, in-process loop; prints numeric stats.
- `e2e-client --url https://127.0.0.1:PORT --cert-hash HEX` — headless
  over real WebTransport.
- `e2e-client --display [--frames N] [--hold-secs S] [--url ...]` —
  winit window whose `Compositor` is the FrameSink (Xvfb-friendly);
  without `--url` the decoded frames come from the in-process loop on a
  worker thread.

## Frame sources

The host pulls I420 frames from a `FrameSource` (`crates/e2e/src/source.rs`):
`PatternSource` (deterministic moving rectangle, the default and the
only source with a reproducible reference for the PSNR check),
`CapturedFrameSource` (platform `CapturedFrame`s off a depth-1 channel,
BGRA -> I420 with odd sizes cropped to even), and `RealCaptureSource`
(the channel source fed by a live platform capture session). The
encoder is sized from the first frame a source produces, and the
bitrate scales with pixel count (800 kbit/s at 640x360, up to
8000 kbit/s), so a Retina capture needs no configuration. A mid-stream
size change is an error in v1 — restart the host.

## Verification status on the Muse VM (2026-11-23)

Localhost UDP is blocked by the VM sandbox: a plain Python UDP echo to
127.0.0.1 fails with `EPERM (Operation not permitted)` on send, the
pre-existing dummy roundtrip test times out the same way locally
(it passes in CI), and an `e2e-host`/`e2e-client` network run times out
at connect. The in-process loop therefore carries the verification
here: it runs the identical proto fragmentation/reassembly, VP9
encode/decode, FrameSink and FrameAck->constraint code; only
wtransport/QUIC itself is not exercised on this VM.
