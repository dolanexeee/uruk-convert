# Uruk Convert — محوّل أوروك

**A clean-room, open-source offline file converter for macOS, Windows and Linux.**
Drag a file onto the golden sun-disc wheel, pick the format, done — every
conversion happens on your machine. Nothing is uploaded, ever.

Uruk Convert is the first app of **URUK Trilogy Two**, and a from-scratch
reimplementation of the *feature set* of the macOS utility "Tangerine:
File Converter" (feature reference only — no name, artwork, text or code
is shared). The full parity roadmap lives in [PARITY.md](PARITY.md) and
is kept honest: a box is checked only when the feature ships and is
proven by tests.

## Stage 1 — what works today

- **The wheel**: drop a single file and a radial sun-disc menu opens at
  the drop point with only the valid target formats for that file.
- **Queue**: drop many files or folders; per-file progress, cancel, and
  plain-language errors.
- **Images** (ImageMagick): JPG PNG WebP TIFF AVIF BMP → each other +
  PDF export; SVG input; an image **Compress** tool with percentage and
  target-size modes.
- **Video** (FFmpeg): MP4 MOV MKV WebM AVI WMV GIF → each other + MP3
  audio export.
- **Audio** (FFmpeg): MP3 M4A WAV FLAC OGG Opus AIFF WMA → each other.
- **Archives** (7-Zip): create ZIP/TAR/GZIP, extract ZIP/TAR/GZIP,
  repack between formats.
- Output lands next to the source as `name.converted.ext` (or in a
  folder you choose) and never overwrites the original.
- Arabic + English interface (Arabic first, full RTL). Zero network use.

Known platform gaps are listed in PARITY.md — notably HEIC *export*
(the open toolchains ship no HEIC encoder on Linux/Windows yet) and
macOS sidecar packaging for ImageMagick/7-Zip.

## Architecture

```
src/                 React + TypeScript UI (wheel, queue, settings, i18n)
src-tauri/           Tauri 2 shell — thin: window, commands, events
src-tauri/crates/engine-core/
                     The engine layer (Rust, Tauri-free): tool
                     resolution, format families, command construction,
                     job execution with progress + cancellation.
                     Both the GUI and the test CLI drive this code.
src-tauri/binaries/  Sidecar tools (not in git — see below)
tests/engine/        Integration tests: real conversions, re-probed
```

All conversions run through bundled sidecar builds of **FFmpeg** (GPLv3
builds, shipped unmodified), **ImageMagick** (Apache-2.0) and **7-Zip**
(LGPL), resolved by one engine layer: bundled sidecar first, `PATH` as a
development fallback.

## Development

```bash
npm install
bash scripts/fetch-sidecars.sh   # downloads the sidecar tools for your OS
npm run tauri dev                # run the app
npm run test:engine              # engine integration tests (real conversions)
cargo test -p uruk-engine-core   # engine unit tests (from src-tauri/)
npm run tauri build              # production installers
```

On Linux, ImageMagick currently resolves from the system `imagemagick`
package (the engine finds `magick`/`convert` on PATH); the portable
sidecar packaging is tracked in PARITY.md.

## Releases

Tagging `v*` runs `.github/workflows/release-desktop.yml`, which builds
installers for Linux (deb/rpm/AppImage), Windows (msi/exe) and macOS
(dmg, Intel + Apple Silicon) and attaches them to the GitHub release —
the same pipeline proven by Uruk Cut.

## License

Apache-2.0. The bundled tools keep their own licenses (FFmpeg GPLv3
builds are shipped unmodified as standalone executables; 7-Zip is LGPL;
ImageMagick is Apache-2.0).
