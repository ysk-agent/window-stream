# codec — VP9 encoder/decoder for window-stream

Implements the `codec` crate of `docs/IMPLEMENTATION.md`: a VP9
encoder/decoder trait with a libvpx-backed implementation, tuned for
realtime, low-latency window streaming. VP9 is fixed by the project
design (8-bit 4:2:0); AV1 is out of scope, and there is no codec
negotiation.

This crate is **standalone**: it has its own `Cargo.toml` and is not yet a
member of any workspace (the root workspace belongs to another track).
See "Joining the workspace" below.

## API sketch

```rust
use codec::{EncoderConfig, Vp9Encoder, Vp9Decoder, DecoderConfig,
            VideoEncoder, VideoDecoder};

let mut enc = Vp9Encoder::new(EncoderConfig::realtime(1280, 720, 2500))?;
let packets = enc.encode(&i420_frame)?;        // Vec<EncodedFrame>
enc.set_reference_constraint(Some(41));        // last FrameAck seq
enc.invalidate_reference();                    // a frame was lost

let mut dec = Vp9Decoder::new(DecoderConfig::default())?;
let frames = dec.decode(&packets[0])?;         // Vec<I420Frame>
```

* Input: `I420Frame` (tightly packed I420, even dimensions, `seq` =
  protocol sequence number / pts).
* Output: `EncodedFrame { data, is_keyframe, seq }` — one VP9 packet per
  input frame in the realtime configuration.
* `pattern::moving_rectangle` generates the deterministic synthetic
  frames used by tests and the bench (and available to the `dummy` host
  later).
* `ivf::IvfWriter` / `IvfReader`: dump a stream to IVF (`DKIF` header,
  FourCC `VP90`) so it can be replayed with `ffplay` / `ffmpeg`, and read
  it back.

## Realtime configuration

Modelled on WebRTC's and RustDesk's VP9 usage:

| Setting | Value | Why |
|---|---|---|
| `g_lag_in_frames` | 0 | No frame is ever held back: no B-frame equivalent, no encoder latency. |
| Deadline | `VPX_DL_REALTIME` | Per-frame time budget over quality. |
| Rate control | CBR | Predictable per-frame bytes; VBR smoothing needs frames to wait, and ours may not. |
| `VP8E_SET_CPUUSED` | 6 (config) | Realtime speed/efficiency point (WebRTC uses 5–8 for VP9). |
| Auto alt-ref | off | An invisible future-reference frame is a B-frame equivalent. |
| ARNR | off (0 frames / 0 strength) | Alt-ref noise reduction filters against future frames. |
| Error resilience | on (`VPX_ERROR_RESILIENT_DEFAULT`) | Contains some decode-error propagation across frames. |
| `rc_dropframe_thresh` | 0 (config) | Dropping stale frames is the frame queue's job (depth 1), not the encoder's. |
| Periodic keyframes | off by default (config) | Recovery is driven by `FrameAck`, not a timer. |
| Row-MT | on when `threads != 1` | `VP9E_SET_ROW_MT`; harmless no-op failure is recorded in `control_warnings`. |

## Reference invalidation: what libvpx can and cannot do

The protocol rule is *a frame may only reference frames the client has
acknowledged* (`FrameAck` = last decoded seq). **libvpx cannot express
that rule directly**, and this crate does not pretend otherwise:

* There is no "reference only frames with seq <= N" API. libvpx manages
  references by *buffer slot* (last / golden / alt-ref), not by the
  caller's frame numbering.
* The closest mechanism, `VP9E_SET_SVC_REF_FRAME_CONFIG` (per-frame slot
  indices plus reference/update switches), is only honoured in SVC mode
  (`VP9E_SET_SVC`) and still speaks in slots whose contents the caller
  must track against libvpx's own assignments. It is exposed as
  `Vp9Encoder::set_svc_ref_frame_config_raw` for evaluation, not used by
  the default path.
* What the generic per-frame encode flags *do* provide:
  `VPX_EFLAG_FORCE_KF`, and `VP8_EFLAG_NO_REF_LAST/GF/ARF` /
  `VP8_EFLAG_NO_UPD_LAST/GF/ARF` (declared in `<vpx/vp8cx.h>`, shared by
  the VP9 encoder).

`ReferenceMode::AckGated` (the default) is the composition of those
pieces, stated precisely:

1. Inter frames never update golden / alt-ref (`NO_UPD_GF | NO_UPD_ARF`
   on every inter frame). Long-term references are only ever refreshed
   by keyframes, so an unacknowledged delta cannot contaminate them.
2. The `last` reference still advances frame to frame — VP9 prediction
   needs a previous frame and libvpx offers no way to pin it to the
   last acked frame while emitting a frame per input. A lost delta
   therefore breaks the chain until the next keyframe, as in any VP9
   stream.
3. Recovery is escalation: `set_reference_constraint(ack)` records the
   client's position and, in AckGated mode, `encode()` forces a keyframe
   once acks lag more than `ack_stall_threshold_frames` (default 30)
   behind the frame being encoded; `invalidate_reference()` forces the
   next frame to be a keyframe immediately and must be called promptly
   on known loss — the stall escalation is only a safety net for
   unnoticed loss. A lost keyframe is replaced by a *new*
   keyframe, never retransmitted — the protocol's loss policy.

`ReferenceMode::Normal` drops rule 1 (ordinary VP9 behaviour, for
comparison), and `ReferenceMode::KeyframesOnly` removes inter prediction
entirely (every frame independently decodable, at several times the
bitrate) as a safe fallback / measurement baseline.

## FFI approach: generated bindings, not `libvpx-sys`

The task brief suggested the `libvpx-sys` crate. Its current crates.io
release (1.4.2) ships pre-generated bindings from the PNaCl era whose
`vpx_codec_enc_cfg_t` layout predates fields libvpx 1.14 carries in the
middle of that struct (`rc_2pass_vbr_corpus_complexity`, the vizier
rate-control rationals, ...). `vpx_codec_enc_config_default` writes the
*library's* layout, so those bindings against a 1.14 library are an ABI
mismatch, not a style issue. This crate therefore generates bindings
with `bindgen` at build time from the installed `<vpx/*.h>` headers
(`build.rs`, `wrapper.h`), which guarantees the FFI layout matches the
linked library, and links via `pkg-config`. The `vp8_enc_control_id`
values bindgen cannot see (the enum is referenced only from macros) are
pinned as constants in `src/sys.rs` with their source noted.

**bindgen version matters (2026-11-23 correction).** An earlier
diagnosis blamed libvpx 1.16 for breaking this FFI on Ubuntu 26.04;
that was wrong. The cfg struct layouts are identical in libvpx 1.14
and 1.16 (`vpx_codec_enc_cfg` is 504 bytes in both). The actual cause
was bindgen 0.71.1 with libclang 22: LLVM 22 changed
`clang_getTypeDeclaration()` to return forward declarations, and
bindgen < 0.72.1 silently generated the cfg structs as opaque types
(`pub struct vpx_codec_enc_cfg { pub _address: u8 }`), so every field
access failed at compile time. This crate therefore requires
**bindgen >= 0.72.1**, enables bindgen's layout tests, asserts the
cfg struct sizes in `src/sys.rs`, and logs the libvpx and libclang
versions at build time — so the next toolchain skew fails loudly and
legibly instead of masquerading as a libvpx layout change.

## Build prerequisites (Linux)

Toolchain requirements: **Rust >= 1.77** (the crate's `rust-version`;
older toolchains fail because `rustc-hash` 2.x requires >= 1.77),
**libvpx >= 1.14** (headers + library; 1.16 verified), clang /
libclang for bindgen, and **bindgen >= 0.72.1** (see the FFI note
above — 0.71.x mis-generates bindings under libclang 22).

```sh
# libvpx headers + library, pkg-config, a C toolchain, libclang for bindgen
sudo apt-get install -y libvpx-dev pkg-config clang libclang-dev build-essential
# Rust (any recent stable; rustup is the usual route)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Originally verified in this environment with Ubuntu 24.04 packages:
libvpx 1.14.0 (`libvpx.so.9`), clang 18, bindgen 0.71. The bindgen
0.72.1 fix was additionally verified against libclang 22 and libvpx
1.16 headers (Ubuntu 26.04), where 0.71.1 generated opaque bindings.
If `LIBCLANG_PATH` is not
picked up automatically, set it, e.g.
`LIBCLANG_PATH=/usr/lib/llvm-18/lib`.

On macOS (the host platform per `docs/IMPLEMENTATION.md`), the same
crate is expected to build against a Homebrew/vcpkg libvpx; that build
is **not yet verified** — only the Linux build is.

## Build / test / bench

```sh
cd crates/codec
cargo build
cargo test            # round-trip + IVF + invalidation tests
cargo run --release --example bench
```

Tests encode a 45-frame moving-rectangle pattern (640x360, 1.5 Mbit/s),
decode it, and measure quality. Measured on the development VM:
**mean luma PSNR 48.39 dB** (asserted floor: 30 dB). The tests also
assert that the first frame is a keyframe, that AckGated mode emits no
periodic keyframes, that `invalidate_reference()` forces a keyframe
which the *same* (stalled) decoder resynchronises on — keyframe plus
the following delta both decode, after a gap of dropped frames — that
KeyframesOnly packets each decode standalone, and that an IVF dump
parses back byte-identically and decodes end-to-end. Ack-stall
escalation is covered: with acks stuck, a keyframe is forced once the
lag passes 30 frames (and not before); a bitrate change mid-stream
keeps the periodic keyframe interval; `luma_psnr` on mismatched
dimensions returns NaN instead of panicking. A bitstream-level test
parses the VP9 uncompressed header (`ivf::vp9_header_info`,
cross-checked against libvpx 1.14's decoder) and asserts that in
AckGated mode no inter frame refreshes the golden / alt-ref reference
slots — only the `last` slot is refreshed — with a Normal-mode control
that does refresh golden, so the parser cannot be returning false
negatives.

Bench (60 frames per size, `cargo run --release --example bench`),
development VM — a shared, CPU-throttled sandbox (1.5 vCPU cap), so
treat these as smoke numbers, not hardware truth; two runs differed
wildly with neighbour load:

| Size | Encode | Decode |
|---|---|---|
| 1280x720 @ 2.5 Mbit/s | 14–56 ms/frame (18–71 fps) | 4–8 ms/frame (126–263 fps) |
| 1920x1080 @ 5 Mbit/s | 26–130 ms/frame (8–38 fps) | 6–26 ms/frame (38–158 fps) |

Threading experiments on this VM (`g_threads` 2/4, `cpu_used` 8) did not
improve 1080p encode — the vCPU cap is the binding constraint here. On
an unthrottled host, re-run the bench before drawing conclusions about
software VP9 realtime feasibility at 1080p; 720p already clears 30 fps
even in the slow run's neighbourhood at higher `cpu_used`.

## Joining the workspace

When the root workspace is ready for this crate:

1. Add `"crates/codec"` to the root `Cargo.toml` `[workspace] members`.
2. Depend on it from `host` / `client` with
   `codec = { path = "../codec" }` (package name `window-stream-codec`,
   library name `codec`).
3. Nothing in this crate depends on workspace inheritance
   (`workspace.package` / `workspace.dependencies` are not used), so no
   other change is needed. `crates/codec/Cargo.lock` is standalone and
   can be deleted at that point; the workspace lockfile takes over.
