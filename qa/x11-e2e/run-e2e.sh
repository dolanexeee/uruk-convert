#!/usr/bin/env bash
# Uruk Convert — Stage 3 X11 end-to-end proof.
#
# Boots the real product (agent mode) under Xvfb, drags a real PNG fixture
# across the root window with a real XDND v5 source, presses SHIFT
# mid-drag, drops on the wheel's JPG segment, and asserts:
#   1. the driver got XdndFinished(accepted)
#   2. a converted JPG exists next to the fixture (JPEG magic bytes)
#   3. the app's history.json records the conversion with origin "overlay"
#   4. the app captured a screenshot of the real wheel mid-drag
# Artifacts (screenshot, logs) land in qa/x11-e2e/artifacts/.
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

# Run the drag.
SPIKE_URI="file://$FIX" "$DRIVER" >"$WORK/driver.log" 2>&1
DRIVER_RC=$?
cat "$WORK/driver.log"
cp "$WORK/sensor.log" "$ART/sensor.log"
cp "$WORK/driver.log" "$ART/driver.log"
cp "$WORK/app.log" "$ART/app.log"

FAIL=0
note() { echo "  $1"; }

# Conversion outcomes are asynchronous by design (the sensor answers
# XdndFinished, then the engine converts): wait for them like a user
# watching the toast would.
OUT_JPG="$WORK/fixture.converted.jpg"
for i in $(seq 1 40); do [ -f "$OUT_JPG" ] && break; sleep 0.25; done
for i in $(seq 1 40); do find "$WORK" -name history.json | grep -q . && break; sleep 0.25; done

echo "assertions:"

if [ "$DRIVER_RC" -eq 0 ]; then note "PASS driver: XdndFinished accepted the drop"; else note "FAIL driver exit=$DRIVER_RC"; FAIL=1; fi

if [ -f "$OUT_JPG" ] && [ "$(head -c 2 "$OUT_JPG" | od -An -tx1 | tr -d ' \n')" = "ffd8" ]; then
  note "PASS converted JPG exists next to fixture (JPEG magic ffd8, $(stat -c%s "$OUT_JPG") bytes)"
  cp "$OUT_JPG" "$ART/fixture.converted.jpg"
else
  note "FAIL converted JPG missing or not a JPEG: $OUT_JPG"; FAIL=1
fi

HIST="$(find "$WORK" -name history.json | head -1)"
if [ -n "$HIST" ] && grep -q '"origin": "overlay"' "$HIST" && grep -q '"target": "jpg"' "$HIST" && grep -q 'fixture.png' "$HIST"; then
  note "PASS history.json records the overlay conversion (target jpg, origin overlay)"
  cp "$HIST" "$ART/history.json"
else
  note "FAIL history.json does not record the overlay conversion"; [ -n "$HIST" ] && cat "$HIST"; FAIL=1
fi

if [ -f "$ART/wheel-mid-drag.png" ] && [ "$(head -c 4 "$ART/wheel-mid-drag.png" | od -An -tx1 | tr -d ' \n')" = "89504e47" ]; then
  note "PASS wheel screenshot captured (PNG, $(stat -c%s "$ART/wheel-mid-drag.png") bytes)"
else
  note "FAIL wheel screenshot missing"; FAIL=1
fi

echo "sensor timings:"
grep -E "SUMMONED|first paint|CONVERT-REQUESTED" "$WORK/sensor.log" | sed 's/^/  /'

if [ "$FAIL" -eq 0 ]; then
  echo "X11-E2E: ALL ASSERTIONS PASSED"
else
  echo "X11-E2E: FAILURES PRESENT"
fi
exit "$FAIL"
