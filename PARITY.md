# Uruk Convert — Parity Checklist

Uruk Convert is a clean-room reimplementation of the feature set of the
macOS utility **Tangerine: File Converter** (used as a *feature reference
only* — no name, artwork, text, or code is copied). This file is the
program's roadmap and its honesty contract:

- A box is checked **only** when the feature ships in a released build and
  is exercised by automated tests or a documented manual proof.
- The reference markets **275 conversion options, 26 advanced tools, and
  6 file actions**. Our checklist enumerates **260 discrete conversions**
  derived from the format lists published on the reference's official
  site; the residual difference is counting methodology (for example,
  per-family archive creation). We track only what we can enumerate and
  test — never the marketing number.

Legend: `[x]` shipped & proven · `[~]` implemented, platform-gated (see
note) · `[ ]` not yet built.

**Stage 1 status: foundation shipped** — app shell, the sun-disc wheel,
queue, settings, and the image / audio / video / archive engines.

---

## Images — 57 conversions

Targets: JPG PNG WebP HEIC TIFF AVIF BMP PDF · SVG is input-only.
Engine: ImageMagick (AVIF export falls back to FFmpeg/libsvtav1 where the
ImageMagick build cannot write AVIF).

- [x] JPG → PNG, WebP, TIFF, AVIF, BMP, PDF
- [x] PNG → JPG, WebP, TIFF, AVIF, BMP, PDF
- [x] WebP → JPG, PNG, TIFF, AVIF, BMP, PDF
- [x] TIFF → JPG, PNG, WebP, AVIF, BMP, PDF
- [x] AVIF → JPG, PNG, WebP, TIFF, BMP, PDF
- [x] BMP → JPG, PNG, WebP, TIFF, AVIF, PDF
- [x] SVG → JPG, PNG, WebP, TIFF, AVIF, BMP, PDF
- [~] JPG/PNG/WebP/TIFF/AVIF/BMP/SVG → HEIC — implemented, but HEIC
  *encoding* needs an encoder the open toolchains do not ship on
  Linux/Windows (no HEIF muxer in FFmpeg builds, decode-only libheif in
  ImageMagick). The engine refuses with a plain-language error today;
  a platform-native encoder is a later stage.
- [ ] HEIC → JPG, PNG, WebP, TIFF, AVIF, BMP, PDF — decoding is present
  in the bundled tools; no HEIC fixture can be generated with open tools
  locally, so this row stays unchecked until it is proven.

## Audio — 56 conversions

MP3 M4A WAV FLAC OGG Opus AIFF WMA, every input to every other format.
Engine: FFmpeg. (Family pipeline proven in tests from WAV and MP3
sources to every target.)

- [x] MP3 → M4A, WAV, FLAC, OGG, Opus, AIFF, WMA
- [x] M4A → MP3, WAV, FLAC, OGG, Opus, AIFF, WMA
- [x] WAV → MP3, M4A, FLAC, OGG, Opus, AIFF, WMA
- [x] FLAC → MP3, M4A, WAV, OGG, Opus, AIFF, WMA
- [x] OGG → MP3, M4A, WAV, FLAC, Opus, AIFF, WMA
- [x] Opus → MP3, M4A, WAV, FLAC, OGG, AIFF, WMA
- [x] AIFF → MP3, M4A, WAV, FLAC, OGG, Opus, WMA
- [x] WMA → MP3, M4A, WAV, FLAC, OGG, Opus, AIFF

## Video — 48 conversions

MP4 MOV MKV WebM AVI WMV GIF to every other video format, plus MP3
audio export (GIF inputs have no audio track, so no GIF → MP3).
Engine: FFmpeg.

- [x] MP4 → MOV, MKV, WebM, AVI, WMV, GIF, MP3
- [x] MOV → MP4, MKV, WebM, AVI, WMV, GIF, MP3
- [x] MKV → MP4, MOV, WebM, AVI, WMV, GIF, MP3
- [x] WebM → MP4, MOV, MKV, AVI, WMV, GIF, MP3
- [x] AVI → MP4, MOV, MKV, WebM, WMV, GIF, MP3
- [x] WMV → MP4, MOV, MKV, WebM, AVI, GIF, MP3
- [x] GIF → MP4, MOV, MKV, WebM, AVI, WMV

## PDF — 9 conversions (later stage)

- [ ] PDF → DOCX, Pages, PPTX, XLSX, JPG, PNG, EPUB, TXT, MD
  (all pages; images at 300 DPI)

## Documents — 43 conversions (later stage)

Inputs: DOCX, Pages, Markdown, TXT, EPUB, plus RTF / RTFD / text
clippings (input-only). Targets: DOCX, Pages, Markdown, TXT, EPUB, HTML.

- [ ] DOCX → Pages, MD, TXT, EPUB, HTML
- [ ] Pages → DOCX, MD, TXT, EPUB, HTML
- [ ] Markdown → DOCX, Pages, TXT, EPUB, HTML
- [ ] TXT → DOCX, Pages, MD, EPUB, HTML
- [ ] EPUB → DOCX, Pages, MD, TXT, HTML
- [ ] RTF → DOCX, Pages, MD, TXT, EPUB, HTML
- [ ] RTFD → DOCX, Pages, MD, TXT, EPUB, HTML
- [ ] Text clippings → DOCX, Pages, MD, TXT, EPUB, HTML

## Slides — 6 conversions (later stage)

- [ ] PPTX → PDF, JPG, PNG
- [ ] PPT (input only) → PDF, JPG, PNG

## Spreadsheets — 21 conversions (later stage)

- [ ] XLSX → ODS, CSV, TSV, PDF
- [ ] ODS → XLSX, CSV, TSV, PDF
- [ ] CSV → XLSX, ODS, TSV, PDF
- [ ] TSV → XLSX, ODS, CSV, PDF
- [ ] XLS (input only) → XLSX, ODS, CSV, TSV, PDF

## Subtitles — 4 conversions (later stage)

- [ ] SRT → VTT, TXT
- [ ] VTT → SRT, TXT

## Archives — 16 operations

Engine: 7-Zip. GZIP holds a single stream: a single file becomes `.gz`;
folders, multiple files, and repacked archives become `.tar.gz` (the app
says so in the result).

- [x] Create ZIP / TAR / GZIP from files and folders
- [x] Extract ZIP, TAR, GZIP
- [ ] Extract RAR — wired through 7-Zip like the others; no RAR fixture
  can be produced locally (RAR creation is proprietary), so it stays
  unchecked until proven with a real archive.
- [x] Repack ZIP/TAR/GZIP → ZIP, TAR, GZIP (contents are repacked,
  never nested)
- [ ] Repack RAR → ZIP, TAR, GZIP (same fixture limitation)

---

## Advanced tools — 26 (reference count)

The reference's public site names only five tools explicitly; the
remaining 21 are not individually enumerated in its public materials
and will be catalogued from the shipping app in a later research pass.
We list what is named — and nothing we cannot verify.

- [x] **Compress** (images: percentage + target-size modes)
- [ ] Crop
- [ ] Trim (video/audio)
- [ ] Split
- [ ] Merge
- [ ] 21 further tools — to be enumerated from the reference app

## File actions — 6

Reference actions, with our cross-platform equivalents:

- [ ] AirDrop → system share sheet / quick-send (Windows Nearby Share,
  Linux Warpinator-style share, macOS AirDrop)
- [ ] Float → floating preview window pinned above other apps
- [ ] Copy (duplicate in place)
- [ ] Move (to a chosen folder)
- [ ] Copy Path (to clipboard)
- [ ] Terminal → open a terminal in the file's folder

Shipped extras (not reference items):

- [x] Reveal in folder (Explorer / Finder / Files)
- [x] Open with the OS default application

## Core experience

- [x] Drag-and-drop into the window: single file, multiple files, folders
- [x] **The wheel** — radial sun-disc menu on drop of a single file,
  showing only valid targets for that file's family; click a segment
  to convert
- [ ] Hold-a-key wheel during OS-wide drags (the reference's Shift-key
  overlay) — needs platform drag interception; later stage
- [x] Conversion queue: per-file progress, cancel, plain-language errors
- [x] Output next to the source (`name.converted.ext`) or to a chosen
  folder; never overwrites the source
- [x] Settings: output folder, name suffix, image quality, language,
  wheel behavior (documented placeholder)
- [x] Arabic + English UI (Arabic first, full RTL)
- [x] Fully local: zero network use; bundled FFmpeg / ImageMagick /
  7-Zip sidecars

## Platform notes (Stage 1)

- Linux: production `.deb`/AppImage builds bundle FFmpeg + FFprobe +
  7-Zip sidecars; ImageMagick resolves from the distro package via PATH
  until the portable sidecar packaging lands.
- Windows: FFmpeg + 7-Zip sidecars are fetched by
  `scripts/fetch-sidecars.sh`; ImageMagick portable packaging is
  pending.
- macOS: FFmpeg sidecars are fetched by the script; ImageMagick and
  7-Zip standalone sidecars are pending (no official static builds) —
  macOS bundles are therefore not yet at parity and are not released
  until they are.
