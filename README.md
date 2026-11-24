# window-stream

A new remote-display protocol: **minimum latency without the client power cost
of a full-screen video stream**. Not for gaming; for interactive desktop work.

Status: implementation in progress. All five initial tracks are integrated
into `main` as one Cargo workspace: `proto` / `transport` / `dummy`
(WebTransport round trip: Hello -> Window -> Frame -> FrameAck), `codec`
(VP9 over libvpx), `client` (winit + wgpu compositing, demo mode),
`platform` (macOS ScreenCaptureKit adapter; stub elsewhere), and the
Android app (Kotlin shell + Rust core over JNI, built by GitHub Actions).
The e2e crate wires them together: a `FrameSource` (dummy pattern by
default, real ScreenCaptureKit capture with `--source real` on macOS)
feeds VP9 encode -> Frame datagrams -> decode -> client display, with
FrameAck driving the encoder reference constraint. The in-process and
CI network paths are verified, and the real-device end-to-end run
(capture on a Mac through to client display) was verified on
2026-11-23 — see `docs/REAL_CAPTURE.md` for the procedure and the
verification record.

## Goal

- Latency as low as a game-streaming stack (Sunshine/Moonlight class).
- Client power far below a game-streaming stack. A continuous full-screen
  video stream keeps the radio awake and the hardware decoder hot; that is
  the cost we are designing against.
- Client power model:
  `E ≈ E_radio_wake + bits × e_bit(link) + pixels × e_decode(codec) + E_composite`.
  Both terms matter: spending bandwidth to save decode (e.g. raw tiles)
  raises radio power, and spending decode to save bandwidth (e.g. AV1)
  raises SoC power. The optimum is a U-curve whose bottom moves with the
  link (WiFi vs mobile) — the protocol must be able to sit near that bottom.

## How existing systems fall short

- **Sunshine / Moonlight** — full-screen video over the NVIDIA GameStream
  protocol. Low latency, high client power. Most of its complexity is not
  its own design: it is compatibility with a reverse-engineered proprietary
  protocol (HTTP/XML control + RTSP + separate UDP ports + ENet, glued
  together). We carry no such compatibility requirement.
- **RDP** — drawing-oriented and bandwidth-frugal, but gives up latency to
  batching, frame pacing and head-of-line blocking.
- **scrcpy** — lean because transport and privilege ride on ADB
  (push a server jar, run it as shell via `app_process`, capture via a
  virtual display into MediaCodec, inject input via hidden APIs).
  Requiring ADB is the heavy part.
- **RustDesk** — our reference for the *periphery*, not the core: rendezvous
  / relay design, the Android host wiring without ADB (MediaProjection
  foreground service + Accessibility input), adaptive quality control, and
  its per-OS server/client adapter structure. Its core (single-connection
  message multiplexing, full-frame video) is what we are replacing.

## Design decisions

- **Window-level streaming.** The screen is not one video; each window is
  its own stream group and the client composites them locally. The focused
  window gets priority and quality; occluded/minimised windows are paused.
  Window moves and restacking are client-local operations.
- **Codec: VP9, fixed.** 8-bit 4:2:0, no codec negotiation in v1.
  AV1 is excluded: even with hardware decode it is heavy (its compression
  comes from tools that cost decoder compute and memory traffic).
  VP9 sits near the bottom of the U-curve — clearly better compression than
  H.264, far cheaper to decode than AV1, with mature hardware decoders.
  The VP9 tuning knowledge base lives in the WebRTC ecosystem and is
  reusable; **WebRTC itself is not used.**
- **Transport: WebTransport (over QUIC).** Streams are separated by
  purpose, which localises faults and makes debugging easy: one stream can
  be logged, replayed or stalled without affecting the others.
  QUIC's latency/power value is indirect but real: 1-RTT setup (0-RTT on
  resume), no cross-stream head-of-line blocking, fast loss detection,
  connection migration, packet coalescing — i.e. less radio awake time
  (race to sleep) and fewer reconnect/resync bursts. Per byte, QUIC crypto
  costs more CPU than raw UDP; the win is at session level.
- **Outer transport is not assumed.** The expected deployment is a direct
  Tailscale connection, but the outer layer may be WireGuard or itself
  QUIC. The protocol must not depend on outer properties: MTU/fragment
  size comes from QUIC's detected datagram limit (not a hard-coded 1280),
  sending is paced, and inner streams are never funnelled into a single
  outer stream (that would resurrect head-of-line blocking).

## Minimal protocol (v1)

Fixed instead of negotiated: VP9, 8-bit 4:2:0, TLS/auth inherited from
WebTransport. Five message types only:

| Message | Direction | Channel | Contents |
|---|---|---|---|
| `Hello` | client → host | stream | protocol version only |
| `Window` | host → client | reliable stream | window id, geometry; create/update/destroy in one type |
| `Frame` | host → client | datagram | window id, sequence number, keyframe flag, VP9 data |
| `Input` | client → host | datagram | window id, key / pointer event |
| `FrameAck` | client → host | datagram | window id and sequence number of last successfully decoded frame |

## Loss policy: no FEC, no retransmission

- FEC is a fixed tax (typically 10–20% extra bits) that only pays off on
  lossy, high-RTT links. On LAN / tailnet links, loss is near zero and the
  feedback loop is fast, so reacting after the fact is cheaper.
- Timed data is never retransmitted: a late frame is garbage, because the
  next frame already exists. Frames, cursor and input travel as datagrams
  and are dropped, not recovered.
- Holes are closed by **reference invalidation**: the client reports the
  last decoded frame (`FrameAck`); the host encodes the next frame
  referencing only acknowledged frames. A lost delta costs one skipped
  frame, nothing more. A lost keyframe is replaced by a *new* keyframe,
  not a retransmission of the old one.
- State (window geometry etc.) is fixed by **re-synchronisation** —
  sending the current full state again — not by recovering lost messages.
- Stuck keys after a lost key-up are prevented by periodic input-state
  snapshots, not retransmission.
- This no-retransmission rule is also what makes QUIC-over-QUIC safe:
  there is no TCP-over-TCP-style meltdown when nothing retransmits.

## Deferred (add only when measurements hurt)

Semantic layer (glyphs / draw commands), tile updates, tile/glyph caches,
clipboard, audio, file transfer, power-driven adaptive quality control,
session resume. First make the minimal set work and measure latency and
client power against Sunshine and RDP under identical conditions.

## Platforms (initial scope)

macOS and Android only.

- **macOS host**: true per-window capture via ScreenCaptureKit.
- **Android host**: MediaProjection captures the whole display only, so
  the window model degenerates to one window = one display. The protocol
  is unaffected; the window-level benefits simply do not apply there.
- **Android client**: hardware VP9 decode is available through MediaCodec,
  not libvpx — the codec adapter must use it (power).

See `docs/IMPLEMENTATION.md` for the Rust crate architecture and the
implementation policy.
