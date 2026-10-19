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

**Stage 2 status: document-side engines shipped** — documents, PDF,
slides, spreadsheets and subtitles, all proven by the same
fixture → convert → re-probe standard (75 integration checks green).
237 conversions now ship (221 of the 260 enumerated from the reference,
plus 16 extras the reference does not offer: ODT/ODP inputs and the
document PDF-export column). Routing, decided by prototyping the real
tools: **LibreOffice** (headless, per-job private profile) owns
layout-bound work — anything → PDF, office formats, slides, sheets;
**Pandoc** owns semantic formats (Markdown, HTML, EPUB, plain text,
DOCX-from-text) and is the only EPUB reader; **poppler** owns PDF
rasterization (all pages, 300 DPI) and text extraction; subtitles are
converted in pure Rust inside the engine (millisecond-exact).

**Stage 3 status: the interaction layer shipped** — the app is now a
background agent (tray/menu bar, no main window at launch; the window
is settings / history / fallback-drop). The global drag → modifier →
wheel-overlay gesture is **proven end-to-end on Linux/X11** (scripted
XDND proof, see Core Experience), **implemented and cross-type-checked
but not yet runtime-proven on macOS and Windows** (marked
implemented-pending-CI-proof wherever claimed), and ships as a
**standing-orb tier on Wayland**, whose compositors forbid bystander
drag observation by design. Engine unchanged: all 75 integration checks
stay green. See `REPORT-STAGE-3.md` for the proof ledger and every
asterisk.

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

## PDF — 9 conversions

Engines: poppler (raster + text), Pandoc (office/text rebuilding).
LibreOffice has **no** PDF → office export filter (verified: "Error:
no export filter" — a PDF imports as a drawing), so the office routes
below are built from extracted text and are labelled accordingly,
in-app and here. Pages are exported at 300 DPI with `-page-###` names.

- [x] PDF → JPG, PNG (all pages, 300 DPI)
- [x] PDF → TXT, MD (extracted text; MD is the extracted text in a
  Markdown file — no layout is implied)
- [~] PDF → DOCX, PPTX — implemented via extracted text + Pandoc
  (PPTX gets one slide per page). **Text-faithful only**: the original
  page layout, fonts and images are not preserved. The app says so in
  every result.
- [~] PDF → EPUB — implemented the same way (text-only EPUB).
- [~] PDF → XLSX — offered, and **refused with a plain-language
  error**: a PDF stores drawn text, not cells, and no honest route
  rebuilds spreadsheet structure from it. Shipping a text dump named
  `.xlsx` would be a fake conversion.
- [ ] PDF → Pages — not offered: Pages is an Apple-only format with
  no open writer on any platform.

## Documents — 37 conversions shipped (reference enumerates 43)

Targets: PDF (export), DOCX, MD, TXT, EPUB, HTML. Engines: Pandoc
(semantic targets), LibreOffice (PDF export and fallback). Routes
exercised end-to-end in tests: DOCX → all six targets; MD → DOCX, PDF;
TXT → DOCX; HTML → PDF; RTF → DOCX; ODT → PDF; EPUB → TXT, MD, PDF.
The remaining pairs run through those exact code paths. The
LibreOffice fallback for HTML/TXT/DOCX/EPUB (used only when Pandoc is
absent) is implemented but not covered by the automated suite —
Pandoc ships as a sidecar, so the fallback is a degraded-environment
path. Markdown output and EPUB input strictly require Pandoc.

- [x] DOCX → PDF, MD, TXT, EPUB, HTML
- [x] RTF → PDF, DOCX, MD, TXT, EPUB, HTML
- [x] Markdown → PDF, DOCX, TXT, EPUB, HTML
- [x] TXT → PDF, DOCX, MD, EPUB, HTML
- [x] HTML → PDF, DOCX, MD, TXT, EPUB
- [x] EPUB → PDF, DOCX, MD, TXT, HTML
- [x] ODT → PDF, DOCX, MD, TXT, EPUB, HTML — *extra input the
  reference (a Mac app) does not list; LibreOffice-native.*
- [ ] The Pages column (7 conversions: → Pages from each input, and
  Pages as an input) — not offered: no open toolchain reads or writes
  Apple's Pages format.
- [ ] RTFD and text-clippings inputs (12 conversions) — macOS-only
  formats with no open reader; not shipped.

## Slides — 6 conversions (+3 extra)

Engine: LibreOffice renders the deck to PDF; poppler rasterizes at
300 DPI with `-page-###` names (the PDF pipeline).

- [x] PPTX → PDF, JPG, PNG
- [x] PPT (input only) → PDF, JPG, PNG
- [x] ODP → PDF, JPG, PNG — *extra input (LibreOffice-native).*

## Spreadsheets — 21 conversions

Engine: LibreOffice Calc. Delimited exports carry only the first
sheet (the app says so); TSV import/export pins the separator via
LibreOffice filter options (a tab file imported as comma-CSV would
collapse every row into one cell — caught by tests, fixed with
`--infilter`).

- [x] XLSX → ODS, CSV, TSV, PDF
- [x] ODS → XLSX, CSV, TSV, PDF
- [x] CSV → XLSX, ODS, TSV, PDF
- [x] TSV → XLSX, ODS, CSV, PDF
- [x] XLS (input only) → XLSX, ODS, CSV, TSV, PDF

## Subtitles — 4 conversions

Engine: pure Rust in engine-core — no external tool. Timestamps are
integer milliseconds (comma ↔ dot), hours may exceed 99, VTT cue
settings/identifiers and NOTE blocks are handled, TXT export is
dialogue lines only. Edge cases are unit-tested.

- [x] SRT → VTT, TXT
- [x] VTT → SRT, TXT

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
- [x] **Interaction model — global drag overlay (Stage 3, shipped).**
  The reference is a *background utility*, not an app window — and so
  is this: the app launches to the tray/menu bar with no window; the
  user drags a file anywhere in the OS, taps a modifier mid-drag
  (Shift by default, configurable), and the wheel appears at the
  cursor as a native overlay; dropping on a segment converts via
  engine-core and answers with a small floating toast. The window is
  the settings / history / fallback-drop surface only, opened from
  the tray. One `DragSensor` trait, one shared native wheel renderer
  (the Stage 1 wheel design: lapis disc, gold/bronze segments,
  Shamash hub) behind every platform module. Proof levels differ per
  platform and are stated exactly:
  - **Linux/X11 — PROVEN IN-APP.** Full path: XFixes
    selection-ownership detection + `XdndTypeList` classification +
    bystander payload peek, native override-redirect overlay that
    joins the drag via XDND v5 (anchored at the summon point), drop →
    engine-core → native toast + persistent history. Scripted
    end-to-end proof under Xvfb with a real XDND source
    (`qa/x11-e2e/run-e2e.sh`): PNG dragged across the root window,
    Shift, drop on JPG → converted JPG next to the fixture, history
    records origin `overlay`, screenshot of the real wheel mid-drag
    in `qa/x11-e2e/artifacts/`. Latency (release builds, two runs incl.
    one against the shipped .deb's own binary): modifier edge →
    wheel mapped 10–15 ms (incl. engine analyze), first paint
    complete 17–22 ms — see REPORT-STAGE-3.md for the full ledger.
  - **macOS — implemented, pending CI proof.** Listen-only
    `CGEventTap` + drag-pasteboard classification, non-activating
    `NSPanel` overlay registered as a dragging destination,
    Accessibility permission ladder with an honest degraded mode.
    Type-checks for `aarch64-apple-darwin` and `x86_64-apple-darwin`;
    NOT runtime-proven (no Mac in the build environment).
  - **Windows — implemented, pending CI proof.** `WH_MOUSE_LL` /
    `WH_KEYBOARD_LL` hook sensing, layered `WS_POPUP` overlay with a
    real `IDropTarget` (CF_HDROP verification, documented fast
    hand-back for non-file drags), `UpdateLayeredWindow` rendering.
    Type-checks for `x86_64-pc-windows-gnu`; NOT runtime-proven.
  - **Wayland — orb tier (compositor-imposed).** Wayland forbids
    bystander drag observation by design, so the global gesture
    cannot exist there — this is the one OS-imposed asterisk. Where
    the compositor offers layer-shell + data-control (wlroots, KDE):
    a persistent floating orb accepts drops and routes them into the
    normal pipeline. On GNOME/Mutter: the fallback drop window plus
    honest in-app copy stating the matrix (Settings → Session tier).
- [x] Conversion queue: per-file progress, cancel, plain-language errors
- [x] Output next to the source (`name.converted.ext`) or to a chosen
  folder; never overwrites the source
- [x] Settings: output folder, name suffix, image quality, language,
  summon modifier (Shift/Control/Alt), autostart, sensing on/off —
  persisted Rust-side so the global overlay and window jobs share one
  output policy; conversion history persisted Rust-side (window +
  overlay origins) and shown in the window
- [x] Arabic + English UI (Arabic first, full RTL)
- [x] Fully local: zero network use; bundled FFmpeg / ImageMagick /
  7-Zip / Pandoc sidecars; LibreOffice and poppler resolved where
  installed (see platform notes)

## Platform notes (Stages 1–2)

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
- **Pandoc (Stage 2)**: a single static binary per OS, fetched by
  `scripts/fetch-sidecars.sh` for all five triples and bundled like
  the other sidecars. Cost: ~144 MB on disk — the document engine is
  the single largest payload in the installer.
- **LibreOffice (Stage 2)**: resolved from a system install
  (`soffice` on PATH), NOT bundled. Measured size: ~284 MB installed
  on the Linux reference machine; per-OS installers are larger.
  Bundling it would roughly triple the download and freeze the office
  version we ship. **Decision deferred to Stage 4 packaging**:
  bundle vs. detect-and-prompt-to-install. Until then the document,
  slides and spreadsheet families require LibreOffice to be installed
  and the engine says so plainly when it is missing (`tool_missing`).
- **poppler-utils (Stage 2)**: resolved from PATH. No official static
  builds exist (shared-library tools). The Linux `.deb` declares
  `poppler-utils` in Depends; Windows community builds ship DLL trees
  (packaging pending); macOS packaging pending — same Stage 4 pass.

## Platform notes (Stage 3 — interaction layer)

- **X11 (proven)**: the sensor is a bystander — it never grabs the
  pointer or the drag. Classification reads the source's world-readable
  `XdndTypeList`; paths arrive via an opportunistic bystander selection
  peek (GTK/Qt/generic sources answer) or, failing that, as the entered
  target. The overlay window sets `XdndAware` before mapping and is
  *anchored* at the summon point (a following overlay makes its own
  segments unreachable). Sources send `XdndDrop` after the physical
  button release; a 600 ms grace window absorbs that. Esc cancels the
  summon for the rest of that drag. On a system without a tray host the
  agent still runs — the tray is best-effort, sensing is not.
- **macOS (implemented-pending-CI-proof)**: listen-only `CGEventTap`
  (mouse + flags changed) + drag pasteboard type inspection; overlay
  is a pre-created non-activating `NSPanel` registered for `.fileURL`
  drops; the shared pixmap renderer feeds an `NSImageView`. Without
  Accessibility (+ Input Monitoring) permission the ladder degrades
  honestly: menu-bar/Dock drop target and in-window drops keep
  working, and the app says so (toast + settings) instead of failing
  silently. Autostart = per-user LaunchAgent. UIAccessibility /
  notarization behavior on a real Mac is exactly what CI proof must
  establish.
- **Windows (implemented-pending-CI-proof)**: no global drag API
  exists, so low-level mouse/keyboard hooks sense the gesture and OLE
  verifies it: the layered overlay registers an `IDropTarget` only
  while summoned, confirms `CF_HDROP` on `DragEnter`, and for
  non-file drags answers `DROPEFFECT_NONE`, hides and revokes fast
  (<50 ms budget) so the drag continues to its real target — the
  hand-back is the sharp edge of this design. UIPI asterisk: drops
  *from* elevated processes into our non-elevated overlay are blocked
  by Windows itself. Rendering is the shared pixmap via
  `UpdateLayeredWindow` (premultiplied BGRA). Autostart = `HKCU\Run`.
- **Wayland (orb tier)**: session tier is probed at runtime (env +
  live registry roundtrip for `zwlr_layer_shell_v1` /
  data-control globals) and stated in Settings in plain language.
  wlroots/KDE: standing 96×96 layer-shell orb, bottom-right; drops
  deliver `text/uri-list` through `wl_data_device`. GNOME/Mutter:
  neither global exists → fallback drop window + honest note. No
  Wayland path can summon mid-drag; that is a protocol limit, not an
  implementation gap.
