# Uruk Convert — Stage 5.1 Report: the Windows sensor was silently dead (v1.0.1)

**Version:** 1.0.1 (all five version sites bumped from 1.0.0)
**Date:** 2026-10-19
**Trigger:** the owner's first real-machine test of v1.0.0 (Windows
laptop, app installed, tray icon visible): drag a file from the
desktop or an Explorer folder, tap Shift mid-drag — **nothing
appeared, in any combination** (both sources, text file and image
file). User error, drag-source specifics and file-type explanations
were eliminated by his answers; the sensor itself was dead.
**Status:** root cause found from code, fixed, shipped. Tag
`v1.0.1` on commit `72bb52a`; GitHub Actions run `39342926838`
built all seven installers green on the first attempt and
published the release. **The owner's retest is the remaining
verdict** — and it now produces evidence either way (see "What the
owner should see now").

---

## Diagnosis (evidence, file:line at v1.0.0 / commit b83411d)

### 1. ROOT CAUSE — the message pump never ticked

`src-tauri/crates/drag-sensor/src/windows.rs`, `imp::run`:

```rust
SetTimer(null_mut(), TIMER_ID, 8, None);              // TIMER_ID = 0x5543
// …
if msg.message == WM_TIMER && msg.wParam == TIMER_ID {
    core.tick();
}
```

Win32 contract: when `SetTimer`'s `hWnd` is **NULL**, the
`nIDEvent` parameter is **ignored** — the system assigns the timer
its own identifier, returned by `SetTimer`. The code discarded
that return value (and never checked for failure). Every
`WM_TIMER` therefore arrived with a system-assigned `wParam` ≠
0x5543, fell into the loop's `else` branch, and was
`DispatchMessageW`'d into the void.

`SensorCore::tick()` is the **only** place that:

- drains `HOOK_QUEUE` (the records the LL hooks enqueue),
- feeds them to the `GestureTracker` (candidate → summon),
- processes `SensorCommand`s (toasts, HUD panels, config, shutdown),
- runs the hand-back failsafe.

It **never executed once**. The hooks were healthy and queued
records forever; the gesture logic, the overlay and the OLE drop
target behind them were dead code at runtime. This explains the
symptom *exactly*: total silence across desktop + Explorer and
text + image, with the tray alive and **zero errors anywhere** —
nothing ever failed; the queue just never drained. It also
explains why every Stage-5 proof missed it: CI's launch smoke
asserted "agent alive after 5 s" (the process was alive; the
sensor was a zombie), and the Windows module was never run on a
real machine before shipping.

### 2. Error swallowing — CONFIRMED

`src-tauri/src/lib.rs` (v1.0.0), `start_sensor`: the sensor thread
did `if let Err(e) = sensor.start(…) { eprintln!(…) }` — on
Windows a GUI-subsystem app's stderr goes nowhere — and
`platform_note` was set to `("windows", "")` *before* start and
never updated, so Settings claimed platform "windows" regardless
of sensor health. A second `eprintln!("drag sensor running")`
fired unconditionally after spawn. Had `run()` failed fast, nobody
would have seen it either.

### 3. Held-modifier parity gap — REAL, but NOT this failure

`GestureTracker::on_record` summoned only on a `Chord` record
received while in `Candidate` state. A modifier held *before* the
press (or pressed while still under the 8 px threshold) produces
its edge in `Idle`/`Pressed`, where it was dropped; the drag then
confirms with the modifier still down and no new edge ever
arrives → no summon. macOS has the held-as-drag-confirms fallback;
Windows lacked it. The owner's gesture was a mid-drag tap, which
takes the edge path — so this gap does **not** explain his test —
but it is a genuine parity defect, fixed in the same pass (§Fix 3).

### 4. Other silent paths found (hardened, not proven to have fired)

- `show_overlay`: silent return on `CreateWindowExW` NULL and on
  `RegisterDragDrop` failure.
- `ensure_classes`: `RegisterClassExW` results discarded, with
  `CLASSES_DONE` latched — a transient failure would be permanent.
- `SetProcessDpiAwarenessContext` result ignored (position-only
  risk on scaled displays; not a silence cause).

### Ruled out on close reading

- **Classification circularity:** none. The overlay is shown by
  the gesture layer *before* `DragEnter`; OLE classification only
  verifies an already-summoned wheel. The design is sound.
- **Hook installation / `create_sensor` dispatch / shell startup:**
  all correct — the sensor *did* start, install hooks and enter
  its loop. It just never ticked.
- **OLE apartment:** `OleInitialize` runs on the sensor thread
  that owns the overlay and the drop target. Correct as designed.

---

## The fix (commit `72bb52a`)

**Fix 1 — a pump that actually pumps (sound architecture).**
The 8 ms timer now lives on a **message-only host window**
(`UrukConvertSensorHost`, parent `HWND_MESSAGE`) created on the
sensor thread. hWnd timers honor their ID; `host_wndproc` runs
`tick()` on `WM_TIMER` and `PostQuitMessage(0)` when the core
requests shutdown. The `GetMessageW` loop is now a pure
translate/dispatch loop. `SetTimer`'s return is checked;
host-window, timer, hook and OLE failures all return real
`SensorError`s after logging.

**Fix 2 — the sensor talks.**
- `imp::run` sends a `Notice { code: "sensor_armed" }` once hooks
  + timer are live. The shell turns it into the Settings platform
  note (`armed — drag a file and tap Shift mid-drag` /
  «مُفعَّل — اسحب أي ملف واضغط Shift أثناء السحب»).
- On `start()` error the shell's spawn closure now writes the
  Settings note itself — `FAILED — <reason>` — **and** sends a
  `sensor_failed` notice that the event handler shows as an
  **error toast** (AR/EN). No more eprintln-only death.
- **Diagnostic log:** `%APPDATA%\org.uruk.convert\sensor.log`
  (the app-data folder that also holds `history.json`). Append
  mode, flushed per line, rotated to `sensor.log.old` past
  256 KB. It records: run start (+pid, log path), DPI result,
  OLE init, class-registration failures, hooks installed,
  arming, **first tick** ("pump alive"), commands, drag
  candidates, summon decisions *including the reason when
  skipped*, overlay creation/RegisterDragDrop failures, overlay
  shown, DragEnter verdicts (no-HDROP / zero paths / resolver
  refusal / accepted with path+segment counts), handbacks, drop
  dispatch and refusals, overlay dismiss, run end (clean/error).

**Fix 3 — held-modifier fallback (macOS parity).**
`GestureTracker` tracks the held set from *every* `Chord` record
(including Idle) and, when a drag confirms, summons immediately
if a chord is already down. One pre-existing test encoded the old
contract and was rewritten to the new one; two tests added
(held-before-threshold, held-before-press); the classic
tap-after-candidate test is unchanged and still passes.

**Versions:** 1.0.0 → 1.0.1 in `package.json`,
`src-tauri/tauri.conf.json`, `src-tauri/Cargo.toml`,
`crates/drag-sensor/Cargo.toml`, `crates/engine-core/Cargo.toml`
(+ `Cargo.lock`).

## Proof ledger

- drag-sensor host unit tests: **43/43** (the Windows module's
  pure-logic tests are target-gated; they compile under the
  Windows target below).
- `cargo check -p uruk-drag-sensor --target x86_64-pc-windows-gnu
  --all-targets`: **clean** (lib + tests, incl. the rewritten
  pump, host window, tracker changes and the 3 new/updated
  gesture tests). Both Apple targets also check clean.
- engine-core unit tests: **18/18**. Engine integration suite
  locally: **98 passed, 0 failed, 1 known gap** — identical to
  the pre-change local baseline (the gap is the documented
  stock-Ubuntu ImageMagick-policy fixture item; CI's
  fixture-relaxed run on the tagged tree is the 99/0/0 one).
- App crate: `cargo check` (host) clean; frontend `npm run
  build` clean. (Local full test-binary *linking* of the Tauri
  app crate is unavailable in this VM shell — no GTK dev
  symlinks / `windres` — an environment limit that predates this
  change; the MSVC CI job below is the real compile gate for the
  app crate on Windows.)
- **CI release run `39342926838` (tag `v1.0.1`): 4/4 jobs
  SUCCESS on the first run** — Linux, Windows (MSVC), macOS
  x86_64 + arm64. Release published (not draft) with the same
  seven assets:

  | Asset | Bytes |
  |---|---|
  | `Uruk.Convert_1.0.1_amd64.deb` | 181,385,936 |
  | `Uruk.Convert-1.0.1-1.x86_64.rpm` | 181,386,025 |
  | `Uruk.Convert_1.0.1_amd64.AppImage` | 240,384,504 |
  | `Uruk.Convert_1.0.1_x64-setup.exe` | 114,338,900 |
  | `Uruk.Convert_1.0.1_x64_en-US.msi` | 164,966,206 |
  | `Uruk.Convert_1.0.1_x64.dmg` | 78,287,089 |
  | `Uruk.Convert_1.0.1_aarch64.dmg` | 84,589,646 |

- Attempted, honestly **not** completed: re-running
  `qa/x11-e2e` against the downloaded v1.0.1 `.deb` in this VM.
  The extracted binary needs `libwebkit2gtk-4.1.so.0`, which this
  VM's apt subset does not carry (the v1.0.0 deb passed the full
  E2E in Stage 5 under the same harness). Substitute evidence:
  X11 sensor code is untouched by this diff, the shell changes
  are additive notice arms, and the CI Linux job ran the engine
  suite on the tagged tree green.

## What the owner should see now

1. Install **v1.0.1** (`Uruk.Convert_1.0.1_x64-setup.exe` or the
   `.msi`) over the old build and launch it once.
2. Open the app window from the tray → **Settings** → the sensor
   status line should read **"windows — armed — drag a file and
   tap Shift mid-drag"** (or the Arabic equivalent). If the
   sensor ever fails to start, that line reads
   **"windows — FAILED — <reason>"** and an error toast appears —
   silence is no longer a possible outcome.
3. `%APPDATA%\org.uruk.convert\sensor.log` exists and, after
   launch, ends with lines like `ARMED: host timer live (8 ms)`
   and `pump alive: first tick`.

## Exact retest steps

1. With the agent running (tray icon visible), open any Explorer
   folder with a PNG or JPG in it.
2. Drag the image a short distance and, **while still holding
   the mouse button**, tap **Shift** once. The gold sun-disc
   wheel should appear at the cursor. (Holding Shift *before*
   you start the drag now also works.)
3. Drop the file on the **JPG** segment (or another format). A
   toast confirms, and `image.converted.jpg` lands next to the
   original; the run is in History with origin `overlay`.
4. Also try **Shift+Alt** (tools wheel → Crop panel) and
   **Control** (actions wheel → Copy path).
5. **If anything still fails:** open
   `%APPDATA%\org.uruk.convert\sensor.log` — its last lines say
   exactly how far the gesture got (candidate? summon? DragEnter?)
   — and send the log. Every stage of the path is now on record.
