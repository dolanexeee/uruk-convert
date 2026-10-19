#!/usr/bin/env bash
# Fetch the sidecar tools and install them into src-tauri/binaries/ with the
# names the engine resolves:
#   uruk-ffmpeg-<triple>[.exe], uruk-ffprobe-<triple>[.exe],
#   uruk-magick-<triple>[.exe], uruk-7z-<triple>[.exe]
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
#   7-Zip    — official ip7z/7zip GitHub releases (LGPL).
#   ImageMagick — official portable builds (Apache-2.0 licence):
#              Windows portable zip from imagemagick.org; Linux: distro
#              ImageMagick is used via PATH fallback for dev builds (see
#              README "Development" note); macOS: PENDING — no official
#              static ImageMagick build exists; tracked in PARITY.md.
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
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
SEVENZIP_VER="25.01"
SEVENZIP_TAG="25.01"

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
      echo "WARN: 7-Zip publishes no standalone macOS binary in the release zip set;" >&2
      echo "      macOS 7-Zip sidecar is PENDING (see PARITY.md platform notes)." >&2
      ;;
  esac
}

fetch_magick() {
  case "$TRIPLE" in
    x86_64-pc-windows-msvc)
      # Official portable ImageMagick for Windows (Q16, dynamic) — the zip
      # ships many DLLs; the portable single-dir layout is copied wholesale
      # into binaries/magick-win/ by CI. For the single-file sidecar layout
      # used in Stage 1 we record the step as PENDING (see PARITY.md).
      echo "WARN: Windows ImageMagick sidecar packaging is PENDING (see PARITY.md)." >&2
      ;;
    *-apple-darwin)
      echo "WARN: macOS ImageMagick sidecar is PENDING — no official static build (see PARITY.md)." >&2
      ;;
    *unknown-linux-gnu)
      echo "NOTE: Linux dev/release builds resolve ImageMagick from PATH" >&2
      echo "      (distro imagemagick package) until the AppImage sidecar lands (see PARITY.md)." >&2
      ;;
  esac
}

fetch_ffmpeg
fetch_7z
fetch_magick
chmod +x "$DEST"/uruk-* 2>/dev/null || true
echo "Sidecars for $TRIPLE in $DEST:"
ls -la "$DEST" | grep "$TRIPLE" || true
