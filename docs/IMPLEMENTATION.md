# Implementation policy

Language: **Rust** (chosen over Go: no GC pauses in the frame path, cheap
FFI to the native capture/encode APIs that dominate this project, and the
reference implementation RustDesk is Rust). Go's quic-go stack is more
mature, so a throwaway Go prototype purely for latency/power measurement
remains acceptable — measurements transfer, code does not need to.

## Workspace layout (our crates)

Dependency direction is one-way:
`host / client → transport / codec / platform → proto`.

| Crate | Role |
|---|---|
| `proto` | Message definitions only. Kept dependency-light. |
| `transport` | WebTransport wrapper: session, per-purpose stream allocation, datagram send/receive. |
| `codec` | VP9 encoder/decoder trait + implementations behind it. |
| `platform` | OS adapters behind common traits, `cfg`-switched per OS. |
| `host` | Server binary: one pipeline per window (capture → encode → send). |
| `client` | Client binary: decode → composite → input capture. |
| `dummy` | Synthetic host/window emitting test patterns. Built first, so the protocol can be verified with no OS capture involved. |

Rules:

- Frame queues have depth 1. A stale frame is dropped, never queued —
  latency must not accumulate in our own buffers.
- Capture and encode run on dedicated threads; tokio tasks do I/O only.
- Every log line carries stream id + window id (`tracing` spans), so a
  stall can be attributed to one stream — this is why streams are
  separated in the first place.

## External crates

| Concern | Choice | Notes |
|---|---|---|
| Async runtime | `tokio` | |
| WebTransport / QUIC | `wtransport` (on `quinn`) | Server + client. Dev certificates via `rcgen` (self-signed; verify by certificate hash where a browser is involved). |
| Control messages | `prost` (protobuf) | Hello / Window / FrameAck etc. |
| Datagram headers | `bytes`, hand-rolled | Small fixed header; not protobuf. Fragment size from QUIC's detected max datagram size. |
| VP9 (macOS) | `libvpx-sys` (FFI) | Hidden behind the `codec` trait. RustDesk's vpx usage is the tuning reference (realtime mode, minimal references, error resilience). |
| VP9 (Android) | MediaCodec via JNI | Hardware decode lives here; libvpx is only a fallback for encode on devices without HW VP9 encode. |
| macOS capture | `objc2` family (ScreenCaptureKit) | Per-window capture. |
| macOS input injection | `core-graphics` (CGEvent) | `enigo` acceptable for MVP. |
| Client rendering | `winit` + `wgpu` | Remote windows are textures; GPU compositing, YUV→RGB in shader. `pixels` acceptable to get a first picture up. |
| Logging | `tracing` + `tracing-subscriber` | |
| Errors | `thiserror` (libraries), `anyhow` (binaries) | |

## Platform adapters

Per-OS server/client code is adapter-form (the RustDesk pattern): the
core protocol is OS-independent; everything OS-specific sits behind the
`platform` traits (capture, input injection, codec capability, client
surface/compositing).

### macOS

- Capture: ScreenCaptureKit, per window.
- Codec: libvpx (VP9 software). VideoToolbox has no VP9 encode; do not
  plan around it.
- Build: plain cargo.

### Android

- The app shell is **Kotlin**: MediaProjection (foreground service),
  MediaCodec and the Accessibility service are Java/Kotlin APIs; do not
  fight them from Rust.
- The Rust core is built with `cargo-ndk` into a `.so` and called from
  Kotlin over JNI (`jni` crate). Split: Kotlin = permissions and OS APIs,
  Rust = protocol and transport.
- Client rendering: hand the Kotlin Surface to Rust/`wgpu` (Vulkan), or
  for MVP render MediaCodec output directly to the Surface.
- Build: cargo-ndk + Gradle. **Project convention: Gradle Android builds
  run on GitHub Actions in this (public) repository** — commit, push, let
  CI build, then download the APK and verify package name, version,
  signature and SHA-256 before delivering.

## Build order

1. `proto` + `dummy` + `transport`: synthetic window over WebTransport,
   logged per stream.
2. `client` with `wgpu` compositing and libvpx decode: first picture.
3. `Input` round-trip with a dummy injector.
4. Real capture adapter (macOS ScreenCaptureKit) — last, because it is
   the most OS-specific and the least protocol-relevant.
5. Android: Kotlin shell + JNI core, MediaCodec adapter.

## Non-goals for v1

No codec negotiation, no FEC, no retransmission, no clipboard/audio/file
transfer, no adaptive quality, no Windows/Linux support. Anything added
later enters as an independent stream enabled by negotiation, never by
complicating the five core messages.
