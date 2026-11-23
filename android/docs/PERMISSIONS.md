# Android permission constraints

How capture permission works on Android, and what it forces on the
window-stream Android host. Recorded so the constraints are designed
around, not rediscovered.

## MediaProjection consent

- **Consent is per session, every time.** Starting capture shows the
  system MediaProjection dialog and the user must approve it. There is
  no persistent grant, no pre-approval, and no silent/programmatic way
  to obtain a projection. Any UX that assumes "grant once, capture
  forever" is wrong on this platform.
- **The consent result is single-use.** The `Intent` returned for a
  successful consent can be passed to
  `MediaProjectionManager.getMediaProjection()` exactly once. It must
  be forwarded straight to the foreground service and never cached,
  persisted, or replayed after a service restart. A restarted service
  needs a fresh consent.
- **Whole display or single app is the user's choice.** On Android 14+
  the dialog lets the user share one app instead of the entire screen.
  The app cannot force entire-screen capture; it must handle whatever
  surface the user granted.
- **A foreground service is mandatory.** The projection must be owned
  by a foreground service declared with
  `android:foregroundServiceType="mediaProjection"`, and the app must
  hold `FOREGROUND_SERVICE` and
  `FOREGROUND_SERVICE_MEDIA_PROJECTION`. The service must call
  `startForeground()` with the mediaProjection type before consuming
  the consent result. On Android 14+, starting this service type from
  the background is restricted — start it while the app is in the
  foreground, directly from the consent result.
- **Callback registration (Android 14+).** A
  `MediaProjection.Callback` must be registered before
  `createVirtualDisplay()` is called, or the call throws. When the
  system stops the projection (user revokes it from the status bar /
  quick settings), `onStop()` fires and capture must tear down.
- **Re-consent after process death.** If the process or service dies,
  the projection is gone. Recovery is a new user consent, not a
  restore. Session-resume designs must treat Android capture as
  consent-bound, unlike the macOS host.

## Notifications

Posting the foreground-service notification on Android 13+ interacts
with `POST_NOTIFICATIONS` (a runtime permission). The app requests
it at runtime from `MainActivity` before capture starts (API 33+).
The service still runs if the permission is denied, but the
notification is then suppressed in the drawer; capture is never
blocked on the notification permission.

## What is NOT used

- **No ADB.** No `app_process` server, no shell privileges, no hidden
  APIs. That is a fixed design decision: scrcpy's model is explicitly
  not followed.
- **No Accessibility service yet.** RustDesk uses an Accessibility
  service for input injection on Android. Input is out of scope for
  the diagnostics skeleton; if it is added later it needs its own
  user-granted Accessibility consent, which is a separate, manually
  enabled system setting — it cannot be granted in-app either.
