# Uruk Convert — محوّل أوروك

**A clean-room, open-source offline file converter for macOS, Windows and Linux.**
Drag a file anywhere, tap a modifier key mid-drag, and a golden sun-disc
wheel appears at your cursor — drop on a format and the file is converted,
right there. Every conversion happens on your machine. Nothing is
uploaded, ever.

Uruk Convert is the first app of **URUK Trilogy Two**, and a from-scratch
reimplementation of the *feature set* of the macOS utility "Tangerine:
File Converter" (feature reference only — no name, artwork, text, or code
is shared). The full parity roadmap lives in [PARITY.md](PARITY.md) and
is kept honest: a box is checked only when the feature ships and is
proven by tests.

## What ships in v1.0

**A background agent, not an app window.** The app lives in the tray /
menu bar. Three wheels share one gesture — change the chord mid-drag and
the wheel swaps:

- **Convert wheel** (Shift): the sun-disc of target formats for the
  dragged file. Images, video, audio, archives, documents, PDF, slides,
  spreadsheets, subtitles — 237 conversions, each proven by a
  fixture → convert → re-probe test.
- **Tools wheel** (Shift+Alt): 14 real tools with floating HUD panels —
  Crop, Resize, Rotate/Flip, Add Background, Annotate, Metadata
  view/strip, Compress, Trim, PDF Split / Extract / Merge / Rotate /
  Compress, and **Redact** that provably removes the text (affected
  pages are rebuilt from a render; a paint-over was rejected and does
  not ship).
- **Actions wheel** (Control): Copy file, Copy path, Move to…, Reveal
  in folder, Open terminal here, Share (macOS share sheet only — the
  segment is not offered where no system sheet is wired, rather than
  faked).

The window is only Settings / History / fallback-drop. Arabic + English
UI (Arabic first, full RTL). Output lands next to the source as
`name.converted.ext` (or a folder you choose) and never overwrites the
original.

### Platform status — stated exactly

- **Linux / X11 — proven end-to-end in-app** (scripted XDND proof:
  drag → wheel → drop → converted file + history). Wayland cannot
  observe other apps' drags by design, so it ships a standing-orb tier
  (wlroots/KDE) or the fallback window (GNOME) — the one OS-imposed
  asterisk.
- **macOS / Windows — build-proven in CI, runtime drag proof pending a
  real machine.** CI runners cannot grant macOS Accessibility or offer
  a real interactive drag desktop, so the sensors and panels are
  implemented and type-checked, the installers are CI-built, and a
  headless launch smoke runs in CI — but the mid-drag gesture itself
  is not claimed as runtime-proven until it runs on real hardware.
- Engines are bundled sidecars (FFmpeg, ImageMagick, 7-Zip, Pandoc) —
  see the per-platform notes in PARITY.md for exactly what is bundled
  where.

### LibreOffice — the decision, plainly

Documents, slides, spreadsheets and PDF export are powered by
**LibreOffice, which is detected from a system install and is NOT
bundled** (~284 MB — bundling it would roughly triple the download).
This is the final packaging decision, not a placeholder: if LibreOffice
is absent, those families are unavailable and every affected conversion
says so in plain language; the in-app tools report (Settings) states
the policy and shows whether an install was detected. PDF raster/text
work uses **poppler** (bundled on Windows; the Linux `.deb` declares
`poppler-utils`; macOS: `brew install poppler`).

### Honest limits

- **HEIC export**: no open toolchain encodes HEIC. On macOS the engine
  routes HEIC export through Apple's built-in `sips` (present on every
  Mac; argument construction is unit-tested, runtime proof pending a
  real Mac). On Linux/Windows HEIC export is refused with a plain
  explanation; HEIC *input* converts fine everywhere.
- **PDF → XLSX is refused**, in-app and by design: a PDF stores drawn
  text, not cells, and no honest route rebuilds spreadsheet structure.
  PDF → DOCX/PPTX/EPUB are text-faithful rebuilds and are labelled so.
- Pages and RAR-creation are not offered — no open toolchain writes
  them.

## Architecture

```
src/                 React + TypeScript UI (wheel, queue, settings, i18n)
src-tauri/           Tauri 2 shell — thin: tray agent, commands, events
src-tauri/crates/engine-core/
                     The engine layer (Rust, Tauri-free): tool
                     resolution, format families, command construction,
                     job execution with progress + cancellation.
                     Both the GUI and the test CLI drive this code.
src-tauri/crates/drag-sensor/
                     One DragSensor trait + one shared native wheel /
                     panel renderer behind per-OS modules (X11, macOS,
                     Windows, Wayland orb tier).
src-tauri/binaries/  Sidecar tools (not in git — see below)
tests/engine/        Integration tests: real conversions, re-probed
qa/x11-e2e/          Scripted in-app drag proof for the X11 path
```

All conversions run through bundled sidecar builds of **FFmpeg** (GPLv3
builds, shipped unmodified), **ImageMagick** (Apache-2.0), **7-Zip**
(LGPL) and **Pandoc** (GPLv2+), plus system-resolved **LibreOffice**
(MPLv2/LGPLv3), **poppler** (GPLv2) and **Ghostscript** — all resolved
by one engine layer: bundled sidecar first, `URUK_SIDECAR_DIR`, then
`PATH` as a development fallback. The subtitle engine is pure Rust
inside engine-core.

## Development

```bash
npm install
bash scripts/fetch-sidecars.sh   # downloads the sidecar tools for your OS
npm run tauri dev                # run the app
npm run test:engine              # engine integration tests (real conversions)
cargo test --workspace           # unit tests (from src-tauri/)
npm run tauri build              # production installers
```

On Linux, ImageMagick resolves from the system `imagemagick` package
(the engine finds `magick`/`convert` on PATH); the `.deb` declares
`imagemagick`, `poppler-utils` and `ghostscript` as dependencies.

## Releases

Tagging `v*` runs `.github/workflows/release-desktop.yml`, which builds
installers for Linux (deb/rpm/AppImage), Windows (msi/exe) and macOS
(dmg, Intel + Apple Silicon) and attaches them to the GitHub release —
the same pipeline proven by Uruk Cut. Builds are **unsigned** (no code
signing exists in this project): on macOS, right-click the app →
Open on first launch; on Windows, expect the SmartScreen prompt →
"More info" → "Run anyway".

## License

Apache-2.0. The bundled tools keep their own licenses (FFmpeg GPLv3
builds are shipped unmodified as standalone executables; 7-Zip is LGPL;
ImageMagick is Apache-2.0; Pandoc is GPLv2+, shipped unmodified as a
standalone executable; system LibreOffice, poppler and Ghostscript keep
their own licenses and are never redistributed by this project).
