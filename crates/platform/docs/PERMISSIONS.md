# macOS permissions

The macOS backend needs two separate TCC (Transparency, Consent and
Control) permissions. They are granted **per binary / per app bundle**,
to the *responsible process* -- which is not always the binary you
think it is (see "Terminal trap" below).

## Screen Recording -- required for capture and enumeration

* Needed by: `SCShareableContent` queries (window enumeration) and
  `SCStream` capture.
* Without it: enumeration fails or returns windows with redacted
  titles, and `startCapture` fails. The backend surfaces this as
  `PlatformError::PermissionDenied` with a message pointing here.
* Grant it: **System Settings > Privacy & Security > Screen Recording**
  (macOS 13+; on macOS 15 this pane is called "Screen & System Audio
  Recording"), enable the entry for your binary / terminal / app, then
  **restart the process** -- TCC decisions are cached per running
  process, a grant does not take effect mid-run.
* The first `SCShareableContent` query from an unapproved binary
  normally triggers a system prompt. A binary run from a context that
  cannot show the prompt (e.g. an SSH session) may simply fail; run it
  once from a GUI session instead.
* There is no API to grant this programmatically, and
  `CGPreflightScreenCaptureAccess` / `CGRequestScreenCaptureAccess`
  (CoreGraphics) only preflight / request -- the user still decides.
  A future version of this crate may call the request function before
  the first query; v1 deliberately does not, so that the permission
  failure mode stays explicit and testable.

## Accessibility -- required for input injection

* Needed by: posting synthetic `CGEvent`s (`InputInjector`) that
  other applications accept as real input.
* Without it: `CGEvent` creation still succeeds and posting is
  silently ineffective (or the events are dropped) -- there is no
  reliable error to surface, which is why input injection is listed
  as a separate verification step in `MACOS_VERIFICATION.md`.
* Grant it: **System Settings > Privacy & Security > Accessibility**,
  enable the entry for your binary / terminal / app, then restart
  the process.
* Depending on macOS version and event kinds, **Input Monitoring**
  (same Privacy & Security section) may also be requested for
  keyboard events.

## The Terminal trap

TCC attributes a permission to the *responsible process*. A
`platform_probe` binary launched from Terminal.app / iTerm is usually
covered by the **terminal's** grant, while the same binary launched
from Finder, an IDE, or `launchd` needs its **own** grant and will
appear as a separate entry in the settings list. When a test "works
in the terminal but not in the app" (or vice versa), this is why.
For the eventual bundled host app, the grant belongs to the app
bundle and its code signature -- an ad-hoc signed or unsigned dev
binary may lose its grant whenever it is rebuilt, because TCC keys
on the bundle identity / signature.

## Resetting during development

```sh
# Reset Screen Recording decisions for one bundle id (or all)
tccutil reset ScreenCapture [bundle-id]
# Reset Accessibility decisions
tccutil reset Accessibility [bundle-id]
```

Resetting is useful to re-test the first-run prompt flow. It affects
the whole user account, not just this project.
