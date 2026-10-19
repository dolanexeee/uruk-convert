# Uruk Convert — Stage 4 Report: tools wheel, actions wheel, floating HUD panels

**Version:** 0.4.0 (package.json / tauri.conf.json / app crate /
drag-sensor 0.4.0, engine-core 0.3.0)
**Date:** 2026-10-19
**Status:** SHIPPED on Linux/X11 with in-app scripted proof for all
three wheels. macOS/Windows Stage-4 surfaces are implemented on the
same shared framework and type-check, pending CI runtime proof —
the same status their Stage-3 sensors carry.

---

## What Stage 4 is

Stage 3 shipped the background agent and the convert wheel. Stage 4
completes the reference interaction model: **three wheels on one
gesture**, plus the floating HUD panels that make the tools real.

- **Convert wheel** — summon key (Shift default). Unchanged.
- **Tools wheel** — summon + tools key (Shift+Alt default). Segments
  are the tools valid for the dragged file's family; selecting one
  opens its floating HUD panel at the drop point.
- **Actions wheel** — actions key (Control default, alone). Six
  file actions (one platform-conditional), executed immediately or
  through a panel (Move to…).

All three chords are configurable in Settings (each key is
Shift/Control/Alt), and one shared chord rule
(`drag-sensor::chord_for`) decides the wheel on every platform:
the largest matching chord wins. Changing the chord *under a live
wheel* swaps the wheel for the new kind.

## Architecture

- **engine-core** gained a `JobSpec::Tool` family — 14 tools as
  pure, UI-free transformations over the same resolved toolchains
  (ImageMagick, FFmpeg, Ghostscript + poppler, all sidecar→PATH
  resolved like conversions). A `metadata` reader serves the
  Metadata panel. History records tool runs with origin `overlay`
  and the tool id as the target.
- **drag-sensor protocol** grew: `WheelRequest`/`WheelModel` carry a
  `WheelKind`; new events `ToolRequested`, `ActionRequested`,
  `ActionDone`, `PanelClosed`, `PanelPageRequest`; new commands
  `ShowPanel`, `UpdatePanelCanvas`, `ClosePanel`.
- **One shared panel framework** (`drag-sensor::panel`): layout,
  interaction state machine (text fields, chips, toggles, canvas
  shape drawing with a pager, keyboard model incl. Arabic input via
  the X11 unicode keysym rule), and a tiny-skia renderer in the
  wheel's lapis/gold family. Each platform supplies only a native
  host window: X11 override-redirect window (keyboard grab), macOS
  `NSPanel`, Windows a dedicated window class with real focus.
- **The shell** composes panels per tool (fields, aspect chips,
  canvas previews rendered by ImageMagick/pdftoppm into temp PNGs),
  turns panel outcomes into engine jobs, and executes actions
  (Move with collision-safe naming, Reveal, Terminal per platform;
  Share on macOS via the sensor's `NSSharingServicePicker`).

## Shipped tools — with test evidence

Engine suite: **98 checks green** (75 conversion checks from Stages
1–2 + 23 new), `npm run test:engine`. One test per tool, each
fixture → tool → re-probe:

| Tool | Proof |
|---|---|
| Crop | 320×240 fixture → panel-sized crop re-probed at exactly 100×80 (also the E2E path) |
| Resize | re-probed at requested dims, aspect toggle honoured |
| Rotate/Flip | 90° swaps dims; flips verified per-pixel |
| Add Background | alpha fixture: corner pixel becomes the chosen colour after flatten |
| Annotate | rect + arrow + text label rendered; output differs from source in the drawn regions |
| Metadata view | reads back a planted EXIF Make/Model + comment; PDF + media metadata read |
| Metadata strip | the same planted EXIF/comment are **gone** after strip (identify re-probe) |
| Compress (image) | percentage tiers wired into the wheel (engine modes pre-proven) |
| Trim | stream-copy tail trim (start 0) and seek re-encode both probed for duration |
| PDF Split | range outputs carry exactly the pages requested |
| PDF Extract | ranges united into one output with the right page count |
| PDF Merge | pdfunite output page count = sum of inputs |
| PDF Rotate | `pdfinfo` reports `Page rot: 90` (and 270); text layer intact |
| PDF Compress | raster fixture shrinks materially on the screen tier |
| **Redact** | page-1 fixture word `SECRETREDACT` is **absent from `pdftotext`** after redaction; `KEEPTEXT` (page 2) and page 3 survive; page count preserved. Affected pages are rebuilt from a 300 dpi render so no text layer remains under the black boxes — paint-over was rejected and does not ship; the rebuild is disclosed in the job note |

Audio normalize was evaluated and **not shipped** (no padding).
The reference markets 26 tools but does not publicly enumerate
them; PARITY.md states this plainly and claims no equivalence —
our 14 stand on their own tests.

## PROVEN — Linux / X11 (in-app, scripted, repeatable)

`qa/x11-e2e/run-e2e.sh` now runs **three scenarios** against one
agent instance, all assertions passing against both the debug build
and the shipped 0.4.0 release binary:

1. **Convert** (Stage 3 scenario, unchanged): PNG drag → Shift →
   JPG drop → converted JPG on disk, history `{target: jpg,
   origin: overlay}`, wheel screenshot.
2. **Tools**: PNG drag → **Shift+Alt** → tools wheel → drop on
   **Crop** → the `Uruk Convert Panel` window appears (348×399,
   the shared layout's exact size) → the driver clicks the W field,
   clears it, types `100`; same for H=`80`; presses Return → the
   cropped PNG on disk is **exactly 100×80** and history records
   `{target: crop, origin: overlay}`.
3. **Actions**: PNG drag → **Control** → actions wheel → drop on
   **Copy path** → `xclip` reads the X clipboard: exactly the
   fixture's path.

Summon latency (release binary): modifier edge → wheel mapped
**9–13 ms** across the three wheels. Artifacts (driver logs,
converted/cropped outputs, history.json, sensor log, wheel
screenshot) in `qa/x11-e2e/artifacts/`.

## IMPLEMENTED, NOT PROVEN — macOS

Chord tracking via `CGEventFlags` through the same `chord_for`;
drop dispatch by wheel kind; copy-file/copy-path via
`NSPasteboard`; Share via `NSSharingServicePicker` anchored on the
wheel panel; HUD panel = the pre-created non-activating `NSPanel`
resized/framed per panel, fed mouse + keys through the existing
event tap (the panel is key, so the source app never sees the
keystrokes). Type-checks clean for `aarch64-apple-darwin` and
`x86_64-apple-darwin` including test code. Runtime proof awaits
the Stage-5 CI matrix (no Mac in this environment).

## IMPLEMENTED, NOT PROVEN — Windows

The keyboard hook now tracks all three modifier families and emits
chord records; the gesture tracker (pure, unit-tested incl. a new
chord-change test) summons/resummons by kind; DragEnter resolves
with the pending kind; Drop dispatches convert/tools/actions, with
copy actions written natively (`CF_HDROP` DROPFILES blob,
`CF_UNICODETEXT`); the HUD panel is a third window class
(`UrukConvertPanel`, `WS_POPUP` + layered, `SetForegroundWindow` +
focus, WM_CHAR/WM_KEYDOWN input). Type-checks clean for
`x86_64-pc-windows-gnu` including test code. Same CI caveat.

## Wayland

Unchanged orb tier: no bystander drag observation exists, so no
wheels of any kind; the fallback window keeps the full converter.
The engine tools are platform-complete and wait for any future
surface.

## Regression state

- Engine: **98/98** (`npm run test:engine`), 1 known gap carried
  from Stage 2 (documented in the suite output).
- engine-core unit tests: **17/17**. drag-sensor unit tests:
  **43/43** (37 from Stage 3 + chord rule, uri-list, panel
  form/canvas; the Windows tracker's new chord tests compile under
  the Windows target's `--all-targets` check — the module is
  target-gated, as in Stage 3).
- Frontend: `npm run build` clean. Full workspace `cargo test`
  green; `cargo check` green for both Apple targets and the
  Windows GNU target.
- Linux release build: **deb 65.16 MiB · rpm 65.17 MiB · AppImage
  185.57 MiB** at 0.4.0, E2E re-proven against the release binary.

## Asterisks carried forward

- macOS/Windows runtime proof (sensors + panels) → Stage 5 CI
  matrix, together with the 7-installer release.
- HEIC export and the other Stage-2 honest `[~]` items are
  unchanged.
- Metadata **Strip** is images-only (the panel says so; PDF/media
  metadata is view-only).
- Redacted PDF pages are image rebuilds (disclosed in PARITY and
  in the job note); unredacted pages keep their text layer.
- Share exists only where a system share sheet is wired (macOS);
  on Linux/Windows the segment is not offered rather than faked.
