# Uruk Convert — Stage 3 Report: the interaction layer

**Claim discipline (house rule): a platform claim is worth exactly its
proof.** Every status below names its proof level. "Implemented" never
means "proven".

## What Stage 3 is

The app stops being a window with a converter inside and becomes what
the reference is: a **background agent**. It launches to the tray /
menu bar with no window; the user drags a file **anywhere in the OS**,
taps a modifier mid-drag (Shift by default, configurable to Control or
Alt), and the sun-disc wheel appears **at the cursor** as a native
overlay. Dropping on a segment converts through engine-core and answers
with a small native floating toast. The old window survives as the
Settings / History / fallback-drop surface, opened from the tray,
hiding instead of closing. Overlay conversions and window jobs share
one output policy (folder, suffix, quality, language) persisted
Rust-side, and both write into one persistent Rust-side history.

## Architecture

- `src-tauri/crates/drag-sensor` — new crate, one `DragSensor` trait
  (the spike DESIGN's contract) with per-OS modules: `x11`, `macos`,
  `windows`, `wayland` (orb tier). The webview never sees an OS drag.
- One shared native wheel renderer (`render.rs` + `geom.rs` +
  `text.rs` + `arabic.rs`: tiny-skia rasterization, real TTF outlines,
  an Arabic presentation-forms shaper with simplified bidi) renders
  the Stage 1 wheel design — lapis disc, gold/bronze segments,
  Shamash hub, the dragged file's ext + name in the hub — into a
  pixmap that each platform blits its own way (X11 PutImage, Win32
  UpdateLayeredWindow, macOS NSImageView). Segment sets come from
  engine-core's family map via a host resolver; nothing about formats
  lives in the sensor crate.
- Shell (`src-tauri/src/lib.rs`): agent lifecycle, tray, on-demand
  window, sensor event pump (ConvertRequested → engine-core →
  history + toast), agent settings + history persistence, Wayland
  tier startup.

## PROVEN — Linux / X11 (in-app, scripted, repeatable)

Harness: `qa/x11-e2e/run-e2e.sh` — Xvfb, the real release binary in
agent mode (seeded settings, no window), and a real XDND v5 drag
source adapted from the spike (`qa/x11-e2e-driver`): press at
(150,150), drag diagonally, SHIFT at step 26, discover the product's
wheel window by name, compute the JPG segment point from its real
geometry, drop, await XdndFinished.

Result (release build, 2026-10-19): **ALL ASSERTIONS PASSED**

- `XdndFinished(accepted)` returned by the product overlay;
  33 XdndStatus accepts during the drag; the source answered 2
  selection requests (bystander peek + drop payload).
- `fixture.converted.jpg` exists next to the fixture (JPEG magic
  `ffd8`).
- `history.json` records `{fixture.png → fixture.converted.jpg,
  target: jpg, status: done, origin: overlay}`.
- The app captured a screenshot of the real wheel mid-drag
  (`qa/x11-e2e/artifacts/wheel-mid-drag.png`): 7 image segments in
  engine order (JPG, WEBP, HEIC, TIFF, AVIF, BMP, PDF), hub showing
  "PNG / fixture.png".

Latency ledger (release; sensor log in artifacts):

| Stage | Measured |
| --- | --- |
| Drag-owner detection after button press | ~111 ms is drag-source cadence; the XFixes event itself is effectively instant (spike: ~1 ms) |
| Classification (`XdndTypeList` read) | same event-loop turn as owner claim |
| Bystander peek (paths known pre-drop) | ~125 ms after owner claim (scheduled) |
| **SHIFT edge → wheel mapped (incl. engine analyze/resolve)** | **10 ms** |
| **SHIFT edge → first paint complete** | **17 ms** |
| XdndDrop → payload → ConvertRequested | ~30 ms |
| Renderer warmup (once, at agent startup) | 48 ms |

Context for the paint number: the spike's 1–3 ms was core-X arcs with
no text. Ours rasterizes the full product wheel (segments, Arabic-
capable text) with tiny-skia — 17 ms edge-to-painted, same order of
magnitude, and the window is already mapped and participating in the
drag at 10 ms. Debug builds are ~10× slower (140 ms) — a build-mode
artifact, not a product number.

## IMPLEMENTED, NOT PROVEN — macOS

`drag-sensor/src/macos.rs`: listen-only `CGEventTap` (left-drag +
flags-changed) on a dedicated CFRunLoop thread; classification by
drag-pasteboard type inspection; summon only for file drags (the
resolver gates on real paths); overlay = pre-created non-activating
`NSPanel` (popUpMenu level, canJoinAllSpaces, transparent,
`ignoresMouseEvents=false`) blitting the shared pixmap into an
`NSImageView`; a runtime-Objective-C `NSDraggingDestination` subclass
(`objc_allocateClassPair`) receives the real drop; result toasts are
native windows; autostart = per-user LaunchAgent plist; the
Accessibility ladder emits a `permission_missing` notice → the shell
shows a plain-language toast (ar/en) instead of failing silently, and
the degraded mode (menu-bar/Dock drop target, in-window drops) stays
available.

Verification status: `cargo check -p uruk-drag-sensor` passes for
**both** `aarch64-apple-darwin` and `x86_64-apple-darwin` (real
target API type-checking), and the module's pure logic is unit-tested
under `cfg(macos)`. **Not runtime-proven**: no Mac exists in this
environment. First CI run on a Mac runner is the proof gate; until
then PARITY.md says *implemented-pending-CI-proof*.

## IMPLEMENTED, NOT PROVEN — Windows

`drag-sensor/src/windows.rs`: `WH_MOUSE_LL` + `WH_KEYBOARD_LL`
low-level hooks sense the gesture (button + travel + modifier edge);
on summon a `WS_POPUP | WS_EX_LAYERED | WS_EX_TOPMOST` overlay is
created and registered with a hand-rolled `IDropTarget` COM vtable —
OLE itself verifies the drag: `DragEnter` queries `CF_HDROP`; file
drags paint segments, non-file drags get `DROPEFFECT_NONE`, an
immediate hide and `RevokeDragDrop` — the documented hand-back, with
a <50 ms budget so the drag continues to its real target. Rendering:
shared pixmap → premultiplied BGRA → `UpdateLayeredWindow`
(per-pixel alpha). Autostart = `HKCU\…\Run`. Gesture state machine +
`CF_HDROP` parser are pure and unit-tested under `cfg(windows)`.

Verification status: `cargo check -p uruk-drag-sensor --target
x86_64-pc-windows-gnu` passes clean. **Not runtime-proven** (no
Windows machine here). Asterisk carried: UIPI blocks drops *from*
elevated processes into a non-elevated overlay — a Windows rule, not
a bug; Per-Monitor-V2 DPI awareness is declared so coordinates stay
honest on scaled displays.

## WAYLAND — orb tier (the one OS-imposed asterisk)

Wayland forbids bystander drag observation *by design*; no client can
watch another client's drag, so the global summon gesture cannot
exist there — for any app, including the reference's imitators. What
ships, per the spike DESIGN's maximum:

- Session tier probed at runtime (env facts + a live registry
  roundtrip for `zwlr_layer_shell_v1` and the data-control globals)
  by a pure, unit-tested `decide_tier`.
- **wlroots / KDE tier**: a persistent 96×96 layer-shell orb
  (bottom-right, Shamash mark) that is a real `wl_data_device` drop
  surface; drops deliver `text/uri-list`, open the main window, and
  route the files into the normal queue/wheel pipeline.
- **GNOME / Mutter tier**: neither global exists → no orb; the
  fallback drop window is the surface.
- Settings → "Session tier" states which tier the user is on and why,
  in Arabic and English (`tier_explanation`). 23 Wayland unit tests
  cover the decision matrix, URI parsing, and surface lifecycle;
  `cargo check` for the crate is clean. No Wayland compositor exists
  in this environment, so the orb is code-complete and
  protocol-reviewed, runtime proof pending a wlroots/KDE session.

## Engine + regression state

- Engine integration suite (`npm run test:engine`): **75 passed,
  0 failed** (plus the suite's 1 pre-existing known gap, unchanged
  from Stage 2). The engine was not modified by Stage 3.
- drag-sensor unit tests: 37 passed (geometry, Arabic shaping, URI
  parsing, Wayland tier matrix, platform pure logic).
- Frontend `npm run build` (tsc + vite): green.
- Cross-checks: windows-gnu, aarch64-apple-darwin,
  x86_64-apple-darwin — all exit 0 for the sensor crate; the shell
  compiles on the host.
- Linux build re-proof: `npm run tauri build` finished 3 bundles at
  v0.3.0 — deb 64.95 MiB, rpm 64.95 MiB, AppImage 185.39 MiB (agent
  mode is the binary's only launch mode). The E2E above was re-run
  against the binary extracted from the shipped .deb itself:
  **ALL ASSERTIONS PASSED** (summon 15 ms, first paint 22 ms in that
  run — the two release runs bracket 10–15 ms / 17–22 ms).

## Decisions worth knowing

- **The wheel is anchored at the summon point and does not follow the
  cursor.** The spike's overlay followed; following makes the segments
  unreachable — reaching a segment *is* the interaction. (Deliberate
  divergence from the spike, recorded here.)
- Classification ladder on X11: `XdndTypeList` → bystander peek →
  entered-target conversion → drop payload. If a source refuses every
  peek, the wheel still participates via XDND and resolves at drop
  time; a drop before segments paint (hub drop) is a cancel, never a
  guess.
- Esc dismisses the wheel and suppresses re-summon until the button
  is released; `XdndLeave` does not dismiss; a 600 ms post-release
  grace absorbs sources that send `XdndDrop` after the physical
  release (spike finding).
- Tray failure is never fatal (this environment has no tray host at
  all — `libayatana` absent — and the agent still sensed, summoned,
  and converted through the whole E2E).
- The hub shows the file's extension + name (the Stage 1 product
  wheel's design), not an OS file icon.

## Asterisks carried forward

1. macOS + Windows: implemented-pending-CI-proof (above). The first
   CI runs on real runners are their proof gate.
2. Wayland: orb tier is the protocol maximum; GNOME sessions get the
   fallback window. Stated in-app, not just here.
3. Tools/actions wheels (Shift+Option = tools HUD, Control = file
   actions from the reference paradigm) are NOT in Stage 3 scope —
   the summon gesture family currently ships the conversion wheel
   only. The modifier is configurable, but secondary wheels are a
   later stage.
4. HEIC *export* on this Linux toolchain remains unavailable
   (pre-existing engine asterisk from Stage 1/2 — a HEIC segment is
   offered from the family map; the engine answers with its plain-
   language `heic_encode_unavailable` error, surfaced as an error
   toast rather than a silent failure).
5. X11 proof is under Xvfb with the spike-lineage driver: real XDND
   protocol, synthetic pointer. A human drag on a real desktop is the
   same code path but remains Mustafa's hands-on verdict.
6. The E2E converts a single file; multi-file drops share the same
   code path (sensor emits all paths; the shell converts each and
   summarizes in one toast) but are not separately scripted.
