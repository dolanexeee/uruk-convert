#!/usr/bin/env bash
# Fetch the sidecar tools and install them into src-tauri/binaries/ with the
# names the engine resolves:
#   uruk-ffmpeg-<triple>[.exe], uruk-ffprobe-<triple>[.exe],
#   uruk-magick-<triple>[.exe], uruk-7z-<triple>[.exe],
#   uruk-pandoc-<triple>[.exe]
# Windows poppler tools keep their plain upstream names (pdftoppm.exe …)
# and sit flat in the same directory; the engine searches the sidecar
# dirs for plain names too. The macOS ImageMagick bundle additionally
# stages a `magick-home/` tree (dylibs + modules + config) next to a
# `uruk-magick-<triple>` wrapper script that sets MAGICK_HOME.
#
# Usage:
#   scripts/fetch-sidecars.sh                # auto-detect host triple
#   scripts/fetch-sidecars.sh <triple>       # explicit target triple
#
# The binaries are NOT committed to git; this script is the single source of
# truth for obtaining them — the release workflow calls it per matrix entry.
#
# Upstreams (all shipped UNMODIFIED as standalone executables next to the app):
#   FFmpeg   — Linux: johnvansickle.com static builds (GPLv3 build);
#              Windows: gyan.dev "release essentials" (GPLv3 build);
#              macOS: evermeet.cx / osxexperts static builds.
#   7-Zip    — official ip7z/7zip GitHub releases (LGPL), including the
#              macOS tarball (7z*-mac.tar.xz, universal 7zz).
#   Pandoc   — official jgm/pandoc GitHub releases (GPLv2+): a single
#              static binary per OS (Linux tarball, Windows zip, macOS
#              zip). ~144 MB on disk; it is the document-format engine.
#   ImageMagick — Windows: official portable STATIC build (Apache-2.0),
#              a single self-contained magick.exe. macOS arm64: the
#              Homebrew bottle, relocated with dylibbundler + a
#              MAGICK_HOME tree (CI runners only; failure is a WARN, the
#              engine then falls back to a system `magick` on PATH).
#              macOS x86_64: PENDING (the CI fleet is arm64; a system
#              `brew install imagemagick` is picked up via PATH).
#              Linux: distro ImageMagick via PATH (the .deb declares it
#              in Depends); see README "Development" note.
#   poppler  — Windows: oschwartz10612/poppler-windows release builds
#              (GPLv2), staged flat (exes + their DLLs). Linux: distro
#              package (the .deb declares poppler-utils in Depends).
#              macOS: system install (`brew install poppler`) via PATH.
#
# NOT sidecars (resolved from the system, by design — see PARITY.md):
#   LibreOffice — FINAL DECISION (Stage 5): system-detected, NEVER
#              bundled. A full office suite (~284 MB installed on Linux,
#              more with per-OS installers); bundling it would roughly
#              triple the download. The engine resolves a system install
#              via PATH/`soffice` and says so plainly when it is missing.
#   Ghostscript — system-resolved like LibreOffice (the .deb declares
#              it in Depends).
set -euo pipefail

TRIPLE="${1:-}"
if [ -z "$TRIPLE" ]; then
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)            TRIPLE="x86_64-unknown-linux-gnu" ;;
    Linux-aarch64|Linux-arm64) TRIPLE="aarch64-unknown-linux-gnu" ;;
    Darwin-arm64)            TRIPLE="aarch64-apple-darwin" ;;
    Darwin-x86_64)           TRIPLE="x86_64-apple-darwin" ;;
    MINGW64_NT*-x86_64|MSYS_NT*-x86_64) TRIPLE="x86_64-pc-windows-msvc" ;;
    *) echo "fetch-sidecars: cannot auto-detect triple; pass one explicitly" >&2; exit 1 ;;
  esac
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/src-tauri/binaries"
mkdir -p "$DEST"
# The macOS ImageMagick resource tree must exist (possibly empty) on every
# platform so the Tauri resources glob always matches — tauri-build fails
# on a glob with zero matches, and globstars skip dotfiles, hence a real
# named placeholder.
mkdir -p "$DEST/magick-home"
if [ ! -f "$DEST/magick-home/README.txt" ]; then
  printf '%s\n' "Staging tree for the macOS ImageMagick bundle (dylibs, modules, config)." \
    "Filled by scripts/fetch-sidecars.sh on arm64 macOS hosts; empty elsewhere." \
    > "$DEST/magick-home/README.txt"
fi
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
SEVENZIP_VER="25.01"
SEVENZIP_TAG="25.01"
PANDOC_VER="3.5"
# Pinned fallback for the Windows ImageMagick portable build; the fetch
# first tries to discover the current filename from the download page.
MAGICK_WIN_PINNED="ImageMagick-7.1.2-11-portable-Q16-x64.7z"

fetch_ffmpeg() {
  case "$TRIPLE" in
    x86_64-unknown-linux-gnu)
      curl -fL --retry 3 -o "$TMP/ff.tar.xz" https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-amd64-static.tar.xz
      tar -xJf "$TMP/ff.tar.xz" -C "$TMP"
      cp "$TMP"/ffmpeg-*-amd64-static/ffmpeg  "$DEST/uruk-ffmpeg-$TRIPLE"
      cp "$TMP"/ffmpeg-*-amd64-static/ffprobe "$DEST/uruk-ffprobe-$TRIPLE"
      ;;
    aarch64-unknown-linux-gnu)
      curl -fL --retry 3 -o "$TMP/ff.tar.xz" https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-arm64-static.tar.xz
      tar -xJf "$TMP/ff.tar.xz" -C "$TMP"
      cp "$TMP"/ffmpeg-*-arm64-static/ffmpeg  "$DEST/uruk-ffmpeg-$TRIPLE"
      cp "$TMP"/ffmpeg-*-arm64-static/ffprobe "$DEST/uruk-ffprobe-$TRIPLE"
      ;;
    x86_64-pc-windows-msvc)
      curl -fL --retry 3 -o "$TMP/ff.zip" https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip
      unzip -q "$TMP/ff.zip" -d "$TMP"
      cp "$TMP"/ffmpeg-*-essentials_build/bin/ffmpeg.exe  "$DEST/uruk-ffmpeg-$TRIPLE.exe"
      cp "$TMP"/ffmpeg-*-essentials_build/bin/ffprobe.exe "$DEST/uruk-ffprobe-$TRIPLE.exe"
      ;;
    x86_64-apple-darwin)
      curl -fL --retry 3 -o "$TMP/ffmpeg.zip" https://evermeet.cx/ffmpeg/getrelease/ffmpeg/zip
      unzip -q "$TMP/ffmpeg.zip" -d "$TMP"
      cp "$TMP/ffmpeg" "$DEST/uruk-ffmpeg-$TRIPLE"
      curl -fL --retry 3 -o "$TMP/ffprobe.zip" https://evermeet.cx/ffmpeg/getrelease/ffprobe/zip
      unzip -q "$TMP/ffprobe.zip" -d "$TMP"
      cp "$TMP/ffprobe" "$DEST/uruk-ffprobe-$TRIPLE"
      ;;
    aarch64-apple-darwin)
      curl -fL --retry 3 -o "$TMP/ffmpeg.zip" https://www.osxexperts.net/ffmpeg7arm.zip
      unzip -q "$TMP/ffmpeg.zip" -d "$TMP"
      cp "$TMP/ffmpeg" "$DEST/uruk-ffmpeg-$TRIPLE"
      curl -fL --retry 3 -o "$TMP/ffprobe.zip" https://www.osxexperts.net/ffprobe7arm.zip
      unzip -q "$TMP/ffprobe.zip" -d "$TMP"
      cp "$TMP/ffprobe" "$DEST/uruk-ffprobe-$TRIPLE"
      ;;
  esac
}

fetch_7z() {
  case "$TRIPLE" in
    x86_64-unknown-linux-gnu)
      curl -fL --retry 3 -o "$TMP/7z.tar.xz" "https://github.com/ip7z/7zip/releases/download/${SEVENZIP_TAG}/7z${SEVENZIP_VER//./}-linux-x64.tar.xz"
      tar -xJf "$TMP/7z.tar.xz" -C "$TMP"
      cp "$TMP/7zz" "$DEST/uruk-7z-$TRIPLE"
      ;;
    aarch64-unknown-linux-gnu)
      curl -fL --retry 3 -o "$TMP/7z.tar.xz" "https://github.com/ip7z/7zip/releases/download/${SEVENZIP_TAG}/7z${SEVENZIP_VER//./}-linux-arm64.tar.xz"
      tar -xJf "$TMP/7z.tar.xz" -C "$TMP"
      cp "$TMP/7zz" "$DEST/uruk-7z-$TRIPLE"
      ;;
    x86_64-pc-windows-msvc)
      curl -fL --retry 3 -o "$TMP/7z.zip" "https://github.com/ip7z/7zip/releases/download/${SEVENZIP_TAG}/7z${SEVENZIP_VER//./}-x64.zip" || \
      curl -fL --retry 3 -o "$TMP/7z.zip" "https://www.7-zip.org/a/7z${SEVENZIP_VER//./}-x64.zip"
      unzip -q "$TMP/7z.zip" -d "$TMP/7zwin" || mkdir -p "$TMP/7zwin"
      cp "$TMP/7zwin/7z.exe" "$DEST/uruk-7z-$TRIPLE.exe" 2>/dev/null || \
        { echo "WARN: 7z.exe not found in archive; Windows 7-Zip sidecar pending" >&2; }
      ;;
    *-apple-darwin)
      # 7-Zip ships an official macOS tarball: a universal `7zz`.
      curl -fL --retry 3 -o "$TMP/7z.tar.xz" "https://github.com/ip7z/7zip/releases/download/${SEVENZIP_TAG}/7z${SEVENZIP_VER//./}-mac.tar.xz"
      tar -xJf "$TMP/7z.tar.xz" -C "$TMP"
      cp "$TMP/7zz" "$DEST/uruk-7z-$TRIPLE"
      ;;
  esac
}

fetch_magick_windows() {
  # Official portable STATIC build: one self-contained magick.exe.
  # Discover the current filename from the download page; fall back to
  # the pinned name. Any failure is a WARN (the build continues; the
  # engine then reports ImageMagick missing on Windows, as documented).
  local name="$MAGICK_WIN_PINNED" page found=""
  page="$(curl -fsL --max-time 60 https://imagemagick.org/script/download.php 2>/dev/null || true)"
  found="$(printf '%s' "$page" | grep -oE 'ImageMagick-7\.[0-9]+\.[0-9]+-[0-9]+-portable-Q16-x64\.7z' | head -1 || true)"
  [ -n "$found" ] && name="$found"
  echo "fetch-magick: Windows portable build: $name"
  if ! curl -fL --retry 3 -o "$TMP/magick.7z" "https://imagemagick.org/archive/binaries/$name"; then
    echo "WARN: could not download $name; Windows ImageMagick sidecar pending (see PARITY.md)." >&2
    return 0
  fi
  # Extract with a 7z we trust: system 7z, else the sidecar fetched above.
  local seven=""
  if command -v 7z >/dev/null 2>&1; then seven="7z";
  elif [ -f "$DEST/uruk-7z-$TRIPLE.exe" ]; then seven="$DEST/uruk-7z-$TRIPLE.exe"; fi
  if [ -z "$seven" ]; then
    echo "WARN: no 7z available to unpack the ImageMagick portable build." >&2
    return 0
  fi
  mkdir -p "$TMP/magickwin"
  "$seven" x -y -o"$TMP/magickwin" "$TMP/magick.7z" >/dev/null
  if [ -f "$TMP/magickwin/magick.exe" ]; then
    cp "$TMP/magickwin/magick.exe" "$DEST/uruk-magick-$TRIPLE.exe"
    # The static portable build still reads its XML config from its own
    # directory; stage those flat next to the exe (engine dir search and
    # MAGICK_HOME-by-proximity both work off the sidecar directory).
    cp "$TMP/magickwin"/*.xml "$DEST/" 2>/dev/null || true
  else
    echo "WARN: magick.exe not found in the portable archive." >&2
  fi
}

fetch_magick_macos() {
  # Relocate the Homebrew bottle with dylibbundler. arm64 hosts only
  # (the CI fleet); x86_64 targets fall back to a system install via
  # PATH. Failure anywhere here is a WARN, never a build failure.
  if [ "$TRIPLE" != "aarch64-apple-darwin" ] || [ "$(uname -m)" != "arm64" ]; then
    echo "NOTE: macOS ImageMagick bundle only on arm64 hosts; $TRIPLE uses a system magick via PATH (brew install imagemagick)." >&2
    return 0
  fi
  if ! command -v brew >/dev/null 2>&1; then
    echo "WARN: Homebrew not available; macOS ImageMagick sidecar pending." >&2
    return 0
  fi
  brew list imagemagick >/dev/null 2>&1 || brew install imagemagick || {
    echo "WARN: brew install imagemagick failed; sidecar pending." >&2; return 0; }
  brew list dylibbundler >/dev/null 2>&1 || brew install dylibbundler || {
    echo "WARN: brew install dylibbundler failed; sidecar pending." >&2; return 0; }
  local prefix ver home
  prefix="$(brew --prefix imagemagick)"
  ver="$(ls "$prefix/lib" | grep -oE 'ImageMagick-[0-9]+\.[0-9]+\.[0-9]+' | head -1 || true)"
  if [ -z "$ver" ] || [ ! -f "$prefix/bin/magick" ]; then
    echo "WARN: unexpected Homebrew ImageMagick layout under $prefix." >&2
    return 0
  fi
  home="$DEST/magick-home"
  rm -rf "$home/lib" "$home/lib-dylibs" "$home/etc"
  mkdir -p "$home/lib" "$home/etc"
  cp -R "$prefix/lib/$ver" "$home/lib/"
  # etc config (policy.xml & friends) lives in the shared brew prefix.
  if [ -d "$(brew --prefix)/etc/ImageMagick-7" ]; then
    cp -R "$(brew --prefix)/etc/ImageMagick-7" "$home/etc/"
  fi
  # Relocate the main binary: its dylibs land in magick-home/lib-dylibs,
  # referenced via @executable_path from the binary's own directory.
  cp "$prefix/bin/magick" "$DEST/magick-bin-$TRIPLE"
  dylibbundler -od -b -x "$DEST/magick-bin-$TRIPLE" \
    -d "$home/lib-dylibs" -p "@executable_path/magick-home/lib-dylibs" || {
      echo "WARN: dylibbundler failed on the magick binary; sidecar pending." >&2
      rm -f "$DEST/magick-bin-$TRIPLE"; return 0; }
  # Relocate the coder/filter modules the same way: each .so must find
  # the bundled dylibs instead of the Cellar paths it was linked with.
  local so lib orig base
  find "$home/lib/$ver" -name '*.so' | while read -r so; do
    for lib in "$home"/lib-dylibs/*.dylib; do
      [ -f "$lib" ] || continue
      base="$(basename "$lib")"
      orig="$(otool -L "$so" 2>/dev/null | grep -oE "/[^ ]*/$base" | head -1 || true)"
      if [ -n "$orig" ]; then
        install_name_tool -change "$orig" "@executable_path/magick-home/lib-dylibs/$base" "$so" 2>/dev/null || true
      fi
    done
  done
  # Apple Silicon refuses to execute binaries whose ad-hoc signature was
  # invalidated by the relocation edits above — re-sign everything
  # ad-hoc, innermost files first, the main binary last.
  find "$home/lib-dylibs" -name '*.dylib' -type f 2>/dev/null | while read -r f; do
    codesign --force --sign - "$f" >/dev/null 2>&1 || true
  done
  find "$home/lib/$ver" \( -name '*.so' -o -name '*.dylib' \) -type f 2>/dev/null | while read -r f; do
    codesign --force --sign - "$f" >/dev/null 2>&1 || true
  done
  codesign --force --sign - "$DEST/magick-bin-$TRIPLE" >/dev/null 2>&1 || true
  # The wrapper the engine actually resolves: sets MAGICK_HOME and the
  # module/config paths into the staged tree, then execs the real binary.
  cat > "$DEST/uruk-magick-$TRIPLE" <<'EOF'
#!/bin/sh
# Uruk Convert — bundled ImageMagick launcher (macOS).
# Points ImageMagick at the resource tree staged next to this script
# (magick-home/) and execs the relocated binary.
DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
HOME_TREE="$DIR/magick-home"
export MAGICK_HOME="$HOME_TREE"
for d in "$HOME_TREE"/lib/ImageMagick-*; do
  [ -d "$d" ] || continue
  export MAGICK_CONFIGURE_PATH="$d/config-Q16"
  export MAGICK_CODER_MODULE_PATH="$d/modules-Q16/coders"
  export MAGICK_FILTER_MODULE_PATH="$d/modules-Q16/filters"
done
exec "$DIR/magick-bin-__TRIPLE__" "$@"
EOF
  sed -i '' "s/__TRIPLE__/$TRIPLE/" "$DEST/uruk-magick-$TRIPLE"
  chmod +x "$DEST/uruk-magick-$TRIPLE" "$DEST/magick-bin-$TRIPLE"
  echo "fetch-magick: macOS bundle staged (wrapper + magick-bin + magick-home tree)."
}

fetch_magick() {
  case "$TRIPLE" in
    x86_64-pc-windows-msvc)
      fetch_magick_windows || true
      ;;
    *-apple-darwin)
      fetch_magick_macos || true
      ;;
    *unknown-linux-gnu)
      echo "NOTE: Linux dev/release builds resolve ImageMagick from PATH" >&2
      echo "      (distro imagemagick package; the .deb declares it) (see PARITY.md)." >&2
      ;;
  esac
}

fetch_poppler() {
  case "$TRIPLE" in
    x86_64-pc-windows-msvc)
      # Community release builds of poppler for Windows (DLL tree in
      # Library/bin). Discover the latest tag from the release redirect
      # (no API token needed); stage exes + DLLs flat into DEST.
      local latest tag url
      latest="$(curl -fsIL -o /dev/null -w '%{url_effective}' https://github.com/oschwartz10612/poppler-windows/releases/latest 2>/dev/null || true)"
      tag="$(printf '%s' "$latest" | grep -oE 'v[0-9]+\.[0-9]+\.[0-9]+-[0-9]+' | head -1 || true)"
      if [ -z "$tag" ]; then
        echo "WARN: could not resolve the latest poppler-windows release; Windows poppler pending." >&2
        return 0
      fi
      url="https://github.com/oschwartz10612/poppler-windows/releases/download/$tag/Release-${tag#v}.zip"
      echo "fetch-poppler: $url"
      if curl -fL --retry 3 -o "$TMP/poppler.zip" "$url"; then
        unzip -q "$TMP/poppler.zip" -d "$TMP/poppler"
        local bindir
        bindir="$(find "$TMP/poppler" -type d -name bin | head -1 || true)"
        if [ -n "$bindir" ]; then
          cp "$bindir"/*.exe "$bindir"/*.dll "$DEST/"
          echo "fetch-poppler: staged $(ls "$bindir"/*.exe | wc -l) tools + DLLs flat into binaries/."
        else
          echo "WARN: poppler bin dir not found in the release zip." >&2
        fi
      else
        echo "WARN: poppler-windows download failed; Windows poppler pending." >&2
      fi
      ;;
    *-apple-darwin)
      echo "NOTE: macOS resolves poppler from PATH (brew install poppler) (see PARITY.md)." >&2
      ;;
    *unknown-linux-gnu)
      echo "NOTE: Linux resolves poppler from the distro poppler-utils package (the .deb declares it)." >&2
      ;;
  esac
}

fetch_pandoc() {
  case "$TRIPLE" in
    x86_64-unknown-linux-gnu)
      curl -fL --retry 3 -o "$TMP/pandoc.tar.gz" "https://github.com/jgm/pandoc/releases/download/${PANDOC_VER}/pandoc-${PANDOC_VER}-linux-amd64.tar.gz"
      tar -xzf "$TMP/pandoc.tar.gz" -C "$TMP"
      cp "$TMP/pandoc-${PANDOC_VER}/bin/pandoc" "$DEST/uruk-pandoc-$TRIPLE"
      ;;
    aarch64-unknown-linux-gnu)
      curl -fL --retry 3 -o "$TMP/pandoc.tar.gz" "https://github.com/jgm/pandoc/releases/download/${PANDOC_VER}/pandoc-${PANDOC_VER}-linux-arm64.tar.gz"
      tar -xzf "$TMP/pandoc.tar.gz" -C "$TMP"
      cp "$TMP/pandoc-${PANDOC_VER}/bin/pandoc" "$DEST/uruk-pandoc-$TRIPLE"
      ;;
    x86_64-pc-windows-msvc)
      curl -fL --retry 3 -o "$TMP/pandoc.zip" "https://github.com/jgm/pandoc/releases/download/${PANDOC_VER}/pandoc-${PANDOC_VER}-windows-x86_64.zip"
      unzip -q "$TMP/pandoc.zip" -d "$TMP"
      cp "$TMP/pandoc-${PANDOC_VER}/pandoc.exe" "$DEST/uruk-pandoc-$TRIPLE.exe"
      ;;
    x86_64-apple-darwin)
      curl -fL --retry 3 -o "$TMP/pandoc.zip" "https://github.com/jgm/pandoc/releases/download/${PANDOC_VER}/pandoc-${PANDOC_VER}-x86_64-macOS.zip"
      unzip -q "$TMP/pandoc.zip" -d "$TMP"
      cp "$TMP/pandoc-${PANDOC_VER}/bin/pandoc" "$DEST/uruk-pandoc-$TRIPLE"
      ;;
    aarch64-apple-darwin)
      curl -fL --retry 3 -o "$TMP/pandoc.zip" "https://github.com/jgm/pandoc/releases/download/${PANDOC_VER}/pandoc-${PANDOC_VER}-arm64-macOS.zip"
      unzip -q "$TMP/pandoc.zip" -d "$TMP"
      cp "$TMP/pandoc-${PANDOC_VER}/bin/pandoc" "$DEST/uruk-pandoc-$TRIPLE"
      ;;
  esac
}

fetch_ffmpeg
fetch_7z
fetch_pandoc
fetch_magick
fetch_poppler
chmod +x "$DEST"/uruk-* 2>/dev/null || true
echo "Sidecars for $TRIPLE in $DEST:"
ls -la "$DEST" | grep "$TRIPLE" || true
ls "$DEST" | grep -E '^(pdftoppm|pdftotext|pdfinfo|pdfimages|pdfunite|magick-bin)' || true
