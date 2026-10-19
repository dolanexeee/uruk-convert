#!/usr/bin/env bash
# Uruk Convert — X11 end-to-end proof (Stage 3 convert + Stage 4
# tools/actions).
#
# Boots the real product (agent mode) under Xvfb and runs THREE real
# XDND v5 drags of a PNG fixture against it:
#   1. CONVERT: SHIFT mid-drag -> wheel -> drop on JPG segment.
#      Asserts XdndFinished(accepted), a converted JPG on disk, a
#      history entry (target jpg, origin overlay), a wheel screenshot.
#   2. TOOLS (Stage 4): SHIFT+ALT -> tools wheel -> drop on Crop ->
#      the floating HUD panel appears; the driver sets W=100, H=80 in
#      the panel and applies. Asserts the cropped PNG on disk is
#      exactly 100x80 and history records target "crop".
#   3. ACTIONS (Stage 4): CONTROL -> actions wheel -> drop on Copy
#      Path. Asserts the X clipboard then holds the fixture path.
# Artifacts (screenshot, outputs, logs) land in qa/x11-e2e/artifacts/.
#
# Usage: qa/x11-e2e/run-e2e.sh [path-to-uruk-convert-binary]
set -u
cd "$(dirname "$0")/../.."
ROOT="$PWD"
ART="$ROOT/qa/x11-e2e/artifacts"
mkdir -p "$ART"

export PATH="$HOME/.cargo/bin:$PATH"
APP_BIN="${1:-$ROOT/src-tauri/target/debug/uruk-convert}"
DRIVER="$ROOT/qa/x11-e2e-driver/target/release/uruk-x11-e2e-driver"

if [ ! -x "$APP_BIN" ]; then
  echo "building product binary (debug)…"
  (cd "$ROOT/src-tauri" && cargo build -p uruk-convert) || exit 1
fi
if [ ! -x "$DRIVER" ]; then
  echo "building e2e driver…"
  (cd "$ROOT/qa/x11-e2e-driver" && cargo build --release) || exit 1
fi

WORK="$(mktemp -d /tmp/uruk-e2e-XXXXXX)"
echo "workdir: $WORK"
export HOME="$WORK/home"
export XDG_CONFIG_HOME="$WORK/config"
export XDG_DATA_HOME="$WORK/data"
export XDG_RUNTIME_DIR="$WORK/runtime"
mkdir -p "$HOME" "$XDG_CONFIG_HOME/org.uruk.convert" "$XDG_DATA_HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

# Seed agent settings: onboarding already done (agent starts tray-only,
# no window), English, Shift summon.
cat > "$XDG_CONFIG_HOME/org.uruk.convert/settings.json" <<'JSON'
{"lang":"en","modifier":"shift","autostart":false,"sensing":true,"onboarding_done":true,"output_dir":null,"suffix":"converted","image_quality":92}
JSON

# Fixture: a real PNG made by the bundled ffmpeg sidecar.
FIX="$WORK/fixture.png"
"$ROOT/src-tauri/binaries/uruk-ffmpeg-x86_64-unknown-linux-gnu" -hide_banner -loglevel error \
  -f lavfi -i color=c=0x2a6fdb:s=320x240 -frames:v 1 "$FIX" || { echo "FIXTURE FAILED"; exit 1; }
echo "fixture: $FIX"

# Virtual display.
Xvfb :77 -screen 0 1280x800x24 >"$WORK/xvfb.log" 2>&1 &
XVFB_PID=$!
export DISPLAY=:77
sleep 1.5

# Launch the product agent.
export URUK_SIDECAR_DIR="$ROOT/src-tauri/binaries"
export URUK_SENSOR_LOG="$WORK/sensor.log"
export URUK_WHEEL_CAPTURE="$ART/wheel-mid-drag.png"
"$APP_BIN" >"$WORK/app.log" 2>&1 &
APP_PID=$!
echo "app pid: $APP_PID"

cleanup() {
  kill "$APP_PID" 2>/dev/null
  kill "$XVFB_PID" 2>/dev/null
}
trap cleanup EXIT

# Wait for the sensor to arm.
for i in $(seq 1 60); do
  grep -q "armed:" "$WORK/sensor.log" 2>/dev/null && break
  kill -0 "$APP_PID" 2>/dev/null || { echo "APP DIED EARLY"; cat "$WORK/app.log"; exit 1; }
  sleep 0.5
done
grep -q "armed:" "$WORK/sensor.log" || { echo "SENSOR NEVER ARMED"; cat "$WORK/sensor.log" 2>/dev/null; cat "$WORK/app.log"; exit 1; }
echo "sensor armed"

FAIL=0
note() { echo "  $1"; }
hist_file() { find "$WORK" -name history.json | head -1; }
wait_hist_entry() { # $1 = grep pattern
  for i in $(seq 1 40); do
    local h; h="$(hist_file)"
    [ -n "$h" ] && grep -q "$1" "$h" && return 0
    sleep 0.25
  done
  return 1
}

echo "══ scenario 1/3: CONVERT wheel (Shift -> JPG segment)"
SPIKE_URI="file://$FIX" SPIKE_SCENARIO=convert "$DRIVER" >"$WORK/driver-convert.log" 2>&1
DRIVER_RC=$?
cat "$WORK/driver-convert.log"
cp "$WORK/driver-convert.log" "$ART/driver-convert.log"

OUT_JPG="$WORK/fixture.converted.jpg"
for i in $(seq 1 40); do [ -f "$OUT_JPG" ] && break; sleep 0.25; done
wait_hist_entry '"target": "jpg"' || true

echo "assertions (convert):"
if [ "$DRIVER_RC" -eq 0 ]; then note "PASS driver: XdndFinished accepted the drop"; else note "FAIL driver exit=$DRIVER_RC"; FAIL=1; fi
if [ -f "$OUT_JPG" ] && [ "$(head -c 2 "$OUT_JPG" | od -An -tx1 | tr -d ' \n')" = "ffd8" ]; then
  note "PASS converted JPG exists next to fixture (JPEG magic ffd8, $(stat -c%s "$OUT_JPG") bytes)"
  cp "$OUT_JPG" "$ART/fixture.converted.jpg"
else
  note "FAIL converted JPG missing or not a JPEG: $OUT_JPG"; FAIL=1
fi
HIST="$(hist_file)"
if [ -n "$HIST" ] && grep -q '"origin": "overlay"' "$HIST" && grep -q '"target": "jpg"' "$HIST" && grep -q 'fixture.png' "$HIST"; then
  note "PASS history.json records the overlay conversion (target jpg, origin overlay)"
else
  note "FAIL history.json does not record the overlay conversion"; [ -n "$HIST" ] && cat "$HIST"; FAIL=1
fi
if [ -f "$ART/wheel-mid-drag.png" ] && [ "$(head -c 4 "$ART/wheel-mid-drag.png" | od -An -tx1 | tr -d ' \n')" = "89504e47" ]; then
  note "PASS wheel screenshot captured (PNG, $(stat -c%s "$ART/wheel-mid-drag.png") bytes)"
else
  note "FAIL wheel screenshot missing"; FAIL=1
fi

echo "══ scenario 2/3: TOOLS wheel (Shift+Alt -> Crop -> HUD panel W=100 H=80)"
SPIKE_URI="file://$FIX" SPIKE_SCENARIO=tools "$DRIVER" >"$WORK/driver-tools.log" 2>&1
DRIVER_RC=$?
cat "$WORK/driver-tools.log"
cp "$WORK/driver-tools.log" "$ART/driver-tools.log"

OUT_PNG="$WORK/fixture.converted.png"
for i in $(seq 1 40); do [ -f "$OUT_PNG" ] && break; sleep 0.25; done
wait_hist_entry '"target": "crop"' || true

echo "assertions (tools):"
if [ "$DRIVER_RC" -eq 0 ]; then note "PASS driver: tools drop accepted + panel driven"; else note "FAIL driver exit=$DRIVER_RC"; FAIL=1; fi
if [ -f "$OUT_PNG" ]; then
  DIMS="$(identify -format '%wx%h' "$OUT_PNG" 2>/dev/null)"
  if [ "$DIMS" = "100x80" ]; then
    note "PASS crop output exists with the panel's dimensions (100x80)"
    cp "$OUT_PNG" "$ART/fixture.converted.png"
  else
    note "FAIL crop output dimensions are '$DIMS', expected 100x80"; FAIL=1
  fi
else
  note "FAIL crop output missing: $OUT_PNG"; FAIL=1
fi
HIST="$(hist_file)"
if [ -n "$HIST" ] && grep -q '"target": "crop"' "$HIST"; then
  note "PASS history.json records the overlay tool run (target crop)"
else
  note "FAIL history.json has no crop entry"; [ -n "$HIST" ] && cat "$HIST"; FAIL=1
fi

echo "══ scenario 3/3: ACTIONS wheel (Control -> Copy path -> clipboard)"
SPIKE_URI="file://$FIX" SPIKE_SCENARIO=actions "$DRIVER" >"$WORK/driver-actions.log" 2>&1
DRIVER_RC=$?
cat "$WORK/driver-actions.log"
cp "$WORK/driver-actions.log" "$ART/driver-actions.log"

echo "assertions (actions):"
if [ "$DRIVER_RC" -eq 0 ]; then note "PASS driver: actions drop accepted"; else note "FAIL driver exit=$DRIVER_RC"; FAIL=1; fi
CLIP="$(xclip -o -selection clipboard 2>/dev/null | tr -d '\r\n')"
if [ "$CLIP" = "$FIX" ]; then
  note "PASS clipboard holds the fixture path after Copy Path ($CLIP)"
else
  note "FAIL clipboard content is '$CLIP', expected '$FIX'"; FAIL=1
fi

cp "$WORK/sensor.log" "$ART/sensor.log"
cp "$WORK/app.log" "$ART/app.log"
HIST="$(hist_file)"
[ -n "$HIST" ] && cp "$HIST" "$ART/history.json"

echo "sensor timings:"
grep -E "SUMMONED|first paint|CONVERT-REQUESTED" "$WORK/sensor.log" | sed 's/^/  /'

if [ "$FAIL" -eq 0 ]; then
  echo "X11-E2E: ALL ASSERTIONS PASSED"
else
  echo "X11-E2E: FAILURES PRESENT"
fi
exit "$FAIL"
