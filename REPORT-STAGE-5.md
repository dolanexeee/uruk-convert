# Uruk Convert — Stage 5 Report: v1.0.0 tagged CI release (FINAL)

**Version:** 1.0.0 (package.json / tauri.conf.json / app crate /
drag-sensor / engine-core — all unified at 1.0.0)
**Date:** 2026-10-19
**Status:** SHIPPED. Tag `v1.0.0` on commit `e3b737b`; GitHub Actions
run `39319647515` built all seven installers in one green matrix run
and published them to the GitHub release. Linux/X11 is proven
end-to-end on the shipped artifact itself. macOS/Windows are
build-proven with CI launch smokes; their mid-drag runtime proof
remains pending a real machine, exactly as flagged from the start.

---

## What shipped

| Asset | Bytes | MiB |
|---|---|---|
| `Uruk.Convert_1.0.0_amd64.deb` | 181,389,640 | 173.0 |
| `Uruk.Convert-1.0.0-1.x86_64.rpm` | 181,387,530 | 173.0 |
| `Uruk.Convert_1.0.0_amd64.AppImage` | 240,392,696 | 229.3 |
| `Uruk.Convert_1.0.0_x64-setup.exe` | 114,356,081 | 109.1 |
| `Uruk.Convert_1.0.0_x64_en-US.msi` | 164,937,534 | 157.3 |
| `Uruk.Convert_1.0.0_x64.dmg` | 78,288,740 | 74.7 |
| `Uruk.Convert_1.0.0_aarch64.dmg` | 84,579,340 | 80.7 |

(Linux installers grew vs 0.4.0: the retired johnvansickle FFmpeg
source was replaced by BtbN's full-GPL static build, which is
larger but alive and license-complete for redistribution.)

## Proof ledger

**Linux — proven on the shipped artifact.**
- Engine suite in CI on the tagged tree: **99 passed, 0 failed,
  0 known gaps** (runs on every release build; the suite step also
  runs the real conversions, not mocks).
- The published `.deb` was downloaded back from the release,
  extracted, and its binary ran `qa/x11-e2e/run-e2e.sh` — **all
  assertions passed**: convert wheel (XDND drag → wheel → JPG on
  disk + history `origin: overlay`), tools wheel (Crop panel,
  output exactly 100×80), actions wheel (Copy Path → clipboard holds
  the fixture path). Wheel summon 8–12 ms after the modifier edge.
- Unit suites: engine-core 18/18, drag-sensor 43/43.

**Windows — built + smoke-proven, drag runtime pending.**
- All sidecars staged and exercised in the job: gyan FFmpeg, Pandoc
  3.5, full 7-Zip (extracted from the official SFX installer),
  poppler staged flat (12 tools + DLLs; `pdftoppm` 26.09.0 runs),
  ImageMagick portable static (from ImageMagick's GitHub releases)
  performed a real PNG→JPG conversion.
- Post-build launch smoke: agent alive after 5 s.
- The app crate had never compiled for Windows before this stage;
  one latent bug (the Linux-only Wayland orb code referenced
  unconditionally) was found by CI and cfg-gated.

**macOS — built + partially smoke-proven, drag runtime pending.**
- FFmpeg/FFprobe (osxexperts), Pandoc, 7-Zip staged and
  version-checked inside the shipped .app bundles; both dmgs built
  for x86_64 and aarch64 from the same run.
- Launch smoke (headless runner, no window server): the agent
  starts and its log shows `drag sensor running (macos)`; the
  process then intermittently aborts on an AppKit `NSMenu`
  assertion (`-[NSMenu _lockForMainMenuItemArray]`) — the x86_64
  build stayed alive in the final run, arm64 in an earlier run.
  Recorded honestly as a CI-environment limit: CI cannot grant
  Accessibility or provide an Aqua session, so **the real-Mac
  first launch + drag remains the verdict**, as the task brief
  predicted.
- **ImageMagick is not bundled on macOS** (see asterisks); the app
  resolves a system `magick` from PATH and says so when absent.

## Decisions finalized this stage

- **LibreOffice: system-detected, NOT bundled** (≈284 MB). The
  engine's tools report carries a plain-language `libreoffice_policy`
  line; Settings shows detected/absent; document rows degrade with a
  clear message, never silently.
- **HEIC encode:** the engine probes at runtime — `sips` (macOS) →
  ImageMagick HEIC writer → FFmpeg HEIF muxer — and refuses plainly
  where none exists. The Linux release build **passes a real
  HEIC-encode test in CI** (the suite's one former known gap is
  closed: 99/0/0). The `sips` route is unit-tested (target-gated)
  but not yet exercised on a real Mac — marked unverified in
  PARITY.

## The CI road (six runs, all fixes from job logs)

1. `39311428099` — cancelled after diagnosis: evermeet.cx FFmpeg
   retired (HTTP 410), 7-Zip publishes no x64 zip, johnvansickle
   stalls from CI egress, pandoc macOS zip layout misjudged.
2. `39313316395` — fetch fixed on mac (osxexperts), Windows (SFX
   extraction), Linux (BtbN); new failures: Ubuntu ImageMagick
   policy bans the suite's PDF fixture, Windows E0432 (Wayland
   cfg), pandoc folder assumption still wrong.
3. `39314558761` — **first full-matrix green**; seven assets
   published. Post-run evidence review found: macOS arm64 magick
   bundle silently un-staged (Homebrew layout guess), mac x64
   launch smoke exited silently.
4. `39316891356` — launch smoke now captures agent output (→ the
   NSMenu diagnosis above); magick discovery diagnostics show the
   true bottle layout (versionless `lib/ImageMagick`,
   `modules-Q16HDRI`).
5. `39318562325` — relocated magick binary runs and reports its
   delegates, but its coder modules do not load from the staged
   tree; the fetch smoke failed the whole arm64 job — a design
   flaw (a sidecar probe must not kill a release build).
6. `39319647515` — the fetch self-tests the bundle and degrades to
   the PATH fallback with diagnostics instead of shipping a broken
   bundle or failing the build. **Full matrix green; this is the
   release run.**

## Asterisks (unchanged honesty rules)

- macOS/Windows **runtime drag proof is pending a real machine** —
  "implemented, build-proven, runtime pending" in PARITY.md and the
  release notes. Nothing in this report upgrades that.
- macOS ImageMagick + poppler resolve from PATH
  (`brew install imagemagick poppler`); the tools report shows
  them absent until installed. Windows and Linux bundle/declare
  everything.
- Stock Debian/Ubuntu ImageMagick `policy.xml` blocks IM's PDF
  coder: on .deb installs, the engine's image→PDF path alone is
  affected until the user relaxes that one policy line (documented
  in PARITY.md). All other PDF work uses poppler/Ghostscript.
- Wayland remains the orb tier (protocol maximum). PDF→XLSX remains
  refused by design. No code signing anywhere — the release notes
  carry the Gatekeeper/SmartScreen first-launch steps.

## Verification commands

- `gh run view 39319647515` — the release run (4/4 jobs success).
- `gh release view v1.0.0` — the seven assets above.
- `qa/x11-e2e/run-e2e.sh <path-to-release-binary>` — the E2E proof
  run against the downloaded .deb's binary: ALL ASSERTIONS PASSED.
