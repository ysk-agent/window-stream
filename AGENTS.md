# AGENTS.md

Working notes for agents (mio, Moka) operating in this repository.

- Design summary: `README.md`. Implementation policy: `docs/IMPLEMENTATION.md`.
  Read both before changing anything; they record decisions made with the
  user on 2026-11-23 and the reasons behind them.
- Long-term documents in this repository are written in **English**.
- Fixed decisions, do not relitigate without the user: transport is
  WebTransport (WebRTC is NOT used), codec is VP9 (AV1 is excluded as too
  heavy to decode), no FEC, no retransmission, five core messages only.
- Android builds that use Gradle run on GitHub Actions in this public
  repository; verify the APK (package name, version, signature, SHA-256)
  before delivering it.
