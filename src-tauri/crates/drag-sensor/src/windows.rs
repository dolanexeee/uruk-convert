//! Windows sensor — global low-level hooks sense the drag, OLE verifies
//! and captures it. Architecture (Stage-3 DESIGN.md, "Windows"):
//!
//! 1. **Sense.** `WH_MOUSE_LL` + `WH_KEYBOARD_LL` hooks installed on the
//!    `start()` thread. Low-level hook callbacks run in *our* process and
//!    need only a message loop, which is also what keeps them alive. The
//!    callbacks are near-empty by design (Windows silently drops slow
//!    low-level hooks): they translate the event into a [`HookRecord`],
//!    push it onto a queue, and return. The same thread drains the queue
//!    on an 8 ms timer between messages. The summon modifier is read from
//!    the keyboard hook, never from focus-dependent state — the source
//!    app's `DoDragDrop` runs a modal loop in *its* process, and LL hooks
//!    are not starved by it.
//! 2. **Gesture.** Button-down + motion beyond [`DRAG_THRESHOLD_PX`] is a
//!    drag *candidate* ([`GestureTracker`], pure logic, unit-tested). A
//!    modifier edge while a candidate is live summons the wheel.
//! 3. **Participate.** The overlay is a plain Win32 `WS_POPUP` window
//!    (`WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE`, *not*
//!    click-through, *not* a Tauri window — the Tauri webview's own
//!    drag-drop must never see this HWND). It is painted with
//!    `UpdateLayeredWindow` from the shared tiny-skia renderer and
//!    registered with `RegisterDragDrop` + a hand-rolled `IDropTarget`
//!    (COM initialised with `OleInitialize` on this same UI thread), so
//!    OLE routes the live, ongoing drag to us.
//! 4. **Verify / hand back.** `IDropTarget::DragEnter` asks the
//!    `IDataObject` for `CF_HDROP`. Not files → `DROPEFFECT_NONE` and an
//!    *immediate* hand-back: a `WM_APP` message posted to the overlay is
//!    dispatched on this thread as soon as the COM callback returns —
//!    before OLE delivers the next `DragOver` — and its handler hides the
//!    overlay and calls `RevokeDragDrop`, so the window underneath is
//!    re-targeted within one motion event (the design budget is <50 ms;
//!    the post itself is sub-millisecond and the revoke is O(1)). The
//!    same path runs when the host's [`crate::WheelResolver`] declines
//!    the drag. This hand-back is the design's sharpest edge: between
//!    `RegisterDragDrop` and the revoke, the original target sees a
//!    leave, so we only ever summon on an explicit modifier tap
//!    mid-drag — clear user intent — never on mere motion.
//! 5. **Convert.** `DragOver` tracks the hover segment with
//!    [`crate::geom::segment_at`]; `Drop` reads the paths with
//!    `DragQueryFileW` and emits `ConvertRequested`. A drop on the hub or
//!    outside the segments is refused with `DROPEFFECT_NONE`.
//!
//! Coordinates are **physical pixels throughout**: the process declares
//! Per-Monitor-V2 DPI awareness in code (`SetProcessDpiAwarenessContext`)
//! before any window exists, so hook points, OLE points and the pixmap
//! all share one space on mixed-DPI setups.
//!
//! **UIPI (documented, unfixable by design):** a medium-integrity agent
//! cannot receive drops — or even a `DragEnter` — from an elevated
//! process (an admin Explorer window, an installer). The drag silently
//! never arrives; there is no signal to detect, so no `Notice` is
//! emitted for it (a notice we cannot attribute would be spam). The
//! hooks still see a candidate, the OLE stage stays silent, the wheel
//! simply never confirms, and the user's drag completes normally
//! against its real target once the button is released.
//!
//! All FFI lives in `imp` behind `#[cfg(target_os = "windows")]`; the
//! pure logic in this module (HDROP parsing, the gesture state machine)
//! compiles and is unit-tested on any OS. Off Windows, `start` and
//! `set_autostart` return an `unsupported` error.
//!
//! **v1.0.0 post-mortem (fixed in v1.0.1).** The shipped sensor was
//! silently dead on real machines while the process, the tray and the
//! CI launch smoke all looked healthy. Root cause: the queue-drain
//! timer was created with `SetTimer(null_mut(), TIMER_ID, …)`. For a
//! NULL-hWnd timer the system *ignores* the requested ID and assigns
//! its own — returned by `SetTimer`, which the code discarded — so
//! every `WM_TIMER` carried an ID ≠ `TIMER_ID`, the message loop's
//! match never fired, and `SensorCore::tick` (the only drain of the
//! hook queue and the command channel) never ran once. The timer now
//! lives on a message-only host window (hWnd timers honor their ID),
//! start failures and successful arming are reported to the shell
//! (Settings shows the real status; a failure also toasts), and a
//! persistent diagnostic log (`%APPDATA%\org.uruk.convert\sensor.log`)
//! records the sensor's decisions so a silent death can never hide
//! again.

use std::sync::mpsc::{Receiver, Sender};

use crate::{
    chord_for, DragSensor, HeldMods, SensorCommand, SensorConfig, SensorError, SensorEvent,
    SensorLang, SummonModifier, WheelKind, WheelResolver,
};

// ---------------------------------------------------------------------------
// Pure logic: DROPFILES (CF_HDROP) parsing
// ---------------------------------------------------------------------------

/// Size of the `DROPFILES` header: `pFiles` (u32) + `pt` (two i32) +
/// `fNonClient` (BOOL) + `fWide` (BOOL).
pub const DROPFILES_HEADER_LEN: usize = 20;

/// Parse the in-memory `DROPFILES` blob behind a `CF_HDROP` handle into
/// file paths. `p_data` is the whole blob (starting at the `DROPFILES`
/// header); `wide` is the blob's `fWide` flag: `true` = the file list is
/// UTF-16LE, `false` = ANSI bytes.
///
/// The list at the `pFiles` offset is a sequence of NUL-terminated
/// strings ended by an *empty* string (the double-NUL terminator).
/// Malformed input (short header, out-of-range offset, unterminated
/// list) yields whatever complete strings were decoded — an empty vec
/// when nothing usable is present — never a panic.
///
/// ANSI decoding note: a byte-perfect ANSI decode needs the system
/// codepage (`MultiByteToWideChar`, FFI); this pure function maps bytes
/// 1:1 to code points (Latin-1). That is exact for ASCII paths and a
/// documented approximation otherwise. In practice every modern shell
/// source sets `fWide`, and the Drop path prefers `DragQueryFileW`,
/// which decodes with the real codepage.
pub fn parse_hdrop(p_data: &[u8], wide: bool) -> Vec<String> {
    if p_data.len() < 4 {
        return Vec::new();
    }
    let p_files = u32::from_le_bytes([p_data[0], p_data[1], p_data[2], p_data[3]]) as usize;
    if p_files >= p_data.len() {
        return Vec::new();
    }
    let list = &p_data[p_files..];
    let mut out = Vec::new();
    if wide {
        let mut cur: Vec<u16> = Vec::new();
        let mut units = list.chunks_exact(2);
        for pair in &mut units {
            let unit = u16::from_le_bytes([pair[0], pair[1]]);
            if unit == 0 {
                if cur.is_empty() {
                    return out; // double-NUL: end of list
                }
                out.push(String::from_utf16_lossy(&cur));
                cur.clear();
            } else {
                cur.push(unit);
            }
        }
        // Unterminated tail: keep a complete trailing string if the
        // blob was cut exactly after one NUL (handled above); a partial
        // string with no terminator is dropped as untrustworthy.
        out
    } else {
        let mut cur: Vec<u8> = Vec::new();
        for &byte in list {
            if byte == 0 {
                if cur.is_empty() {
                    return out; // double-NUL: end of list
                }
                out.push(cur.iter().map(|&b| b as char).collect());
                cur.clear();
            } else {
                cur.push(byte);
            }
        }
        out
    }
}

/// [`parse_hdrop`] reading `fWide` from the header itself (offset 16).
pub fn parse_hdrop_auto(p_data: &[u8]) -> Vec<String> {
    let wide = p_data.len() >= DROPFILES_HEADER_LEN
        && u32::from_le_bytes([p_data[16], p_data[17], p_data[18], p_data[19]]) != 0;
    parse_hdrop(p_data, wide)
}

// ---------------------------------------------------------------------------
// Pure logic: hook records → gesture state machine
// ---------------------------------------------------------------------------

/// Motion (Chebyshev-independent, Euclidean) a press must exceed before
/// it counts as a drag candidate.
pub const DRAG_THRESHOLD_PX: i32 = 8;

/// One minimal fact from a low-level hook. The hook callbacks build
/// exactly these and nothing more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookRecord {
    LeftButtonDown {
        x: i32,
        y: i32,
    },
    MouseMove {
        x: i32,
        y: i32,
    },
    LeftButtonUp {
        x: i32,
        y: i32,
    },
    /// The held-modifier set changed (an edge of any of Shift /
    /// Control / Alt, press or release). The tracker maps the set to
    /// a wheel kind through the shared chord rule.
    Chord(HeldMods),
    /// Edge of Escape.
    EscapeDown,
}

/// What the tracker wants the sensor core to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GestureOutput {
    /// Press + threshold crossed: a drag candidate exists at `(x, y)`.
    Candidate { x: i32, y: i32 },
    /// A summon chord appeared while a candidate is live: summon the
    /// `kind` wheel at `(x, y)`.
    Summon { x: i32, y: i32, kind: WheelKind },
    /// The chord changed under a live wheel: swap it for `kind`.
    Resummon { x: i32, y: i32, kind: WheelKind },
    /// Escape while summoned: hide + revoke + `DragEnded`, and suppress
    /// any further summon for the rest of this drag.
    EscapeCancel,
    /// Button released with no conversion pending: the drag is over.
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GestureState {
    Idle,
    /// Button down, threshold not yet crossed.
    Pressed,
    /// Threshold crossed: a drag candidate.
    Candidate,
    /// Wheel summoned for this drag.
    Summoned,
    /// Escape pressed during this drag: no summon until button-up.
    Suppressed,
}

/// The press → move → modifier → summon state machine, factored pure so
/// it is testable without hooks. One instance per sensor; records are
/// fed in arrival order.
#[derive(Debug, Clone)]
pub struct GestureTracker {
    state: GestureState,
    origin: (i32, i32),
    last: (i32, i32),
    config: SensorConfig,
    summon_kind: Option<WheelKind>,
    /// The most recent modifier set reported by any Chord record —
    /// tracked in *every* state (even Idle) so the held-modifier
    /// fallback can fire when a drag confirms (v1.0.1).
    held: HeldMods,
}

impl Default for GestureTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// The chord configuration the tracker starts with (the app pushes
/// the live one via `set_config`).
pub fn default_sensor_config() -> SensorConfig {
    SensorConfig {
        modifier: SummonModifier::Shift,
        tools_modifier: SummonModifier::Alt,
        actions_modifier: SummonModifier::Control,
        lang: SensorLang::Ar,
        enabled: true,
    }
}

impl GestureTracker {
    pub fn new() -> Self {
        GestureTracker {
            state: GestureState::Idle,
            origin: (0, 0),
            last: (0, 0),
            config: default_sensor_config(),
            summon_kind: None,
            held: HeldMods {
                shift: false,
                control: false,
                alt: false,
            },
        }
    }

    pub fn set_config(&mut self, config: &SensorConfig) {
        self.config = config.clone();
    }

    pub fn is_summoned(&self) -> bool {
        matches!(self.state, GestureState::Summoned)
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.state, GestureState::Idle)
    }

    pub fn on_record(&mut self, rec: HookRecord) -> Option<GestureOutput> {
        match rec {
            HookRecord::LeftButtonDown { x, y } => {
                self.origin = (x, y);
                self.last = (x, y);
                self.state = GestureState::Pressed;
                None
            }
            HookRecord::MouseMove { x, y } => {
                self.last = (x, y);
                if self.state == GestureState::Pressed {
                    let dx = (x - self.origin.0) as i64;
                    let dy = (y - self.origin.1) as i64;
                    let limit = DRAG_THRESHOLD_PX as i64;
                    if dx * dx + dy * dy > limit * limit {
                        self.state = GestureState::Candidate;
                        // Held-modifier fallback (macOS parity, v1.0.1):
                        // the summon chord may already be down — held
                        // from before the press, or pressed while the
                        // drag was still under the threshold — in which
                        // case no fresh Chord edge will ever arrive for
                        // it. Summon as the drag confirms instead.
                        if let Some(k) = chord_for(self.held, &self.config) {
                            self.state = GestureState::Summoned;
                            self.summon_kind = Some(k);
                            return Some(GestureOutput::Summon {
                                x,
                                y,
                                kind: k,
                            });
                        }
                        return Some(GestureOutput::Candidate { x, y });
                    }
                }
                None
            }
            HookRecord::Chord(held) => {
                // Track the held set in every state: the MouseMove
                // fallback above reads it when a drag confirms.
                self.held = held;
                let kind = chord_for(held, &self.config);
                match self.state {
                    GestureState::Candidate => match kind {
                        Some(k) => {
                            self.state = GestureState::Summoned;
                            self.summon_kind = Some(k);
                            Some(GestureOutput::Summon {
                                x: self.last.0,
                                y: self.last.1,
                                kind: k,
                            })
                        }
                        None => None,
                    },
                    GestureState::Summoned => match kind {
                        Some(k) if Some(k) != self.summon_kind => {
                            self.summon_kind = Some(k);
                            Some(GestureOutput::Resummon {
                                x: self.last.0,
                                y: self.last.1,
                                kind: k,
                            })
                        }
                        _ => None,
                    },
                    _ => None,
                }
            }
            HookRecord::EscapeDown => match self.state {
                GestureState::Summoned => {
                    self.state = GestureState::Suppressed;
                    Some(GestureOutput::EscapeCancel)
                }
                GestureState::Pressed | GestureState::Candidate => {
                    // Not summoned yet, but Escape still poisons this
                    // drag: the source is cancelling, so a later
                    // modifier tap must not summon into a dying drag.
                    self.state = GestureState::Suppressed;
                    None
                }
                _ => None,
            },
            HookRecord::LeftButtonUp { x, y } => {
                self.last = (x, y);
                self.summon_kind = None;
                match self.state {
                    GestureState::Idle => None,
                    GestureState::Suppressed => {
                        // EscapeCancel already terminated this drag.
                        self.state = GestureState::Idle;
                        None
                    }
                    _ => {
                        self.state = GestureState::Idle;
                        Some(GestureOutput::Ended)
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub struct WinSensor;

impl WinSensor {
    pub fn new() -> Self {
        WinSensor
    }
}

impl Default for WinSensor {
    fn default() -> Self {
        Self::new()
    }
}

impl DragSensor for WinSensor {
    fn platform(&self) -> &'static str {
        "windows"
    }

    fn start(
        &mut self,
        config: SensorConfig,
        events: Sender<SensorEvent>,
        commands: Receiver<SensorCommand>,
        resolve: WheelResolver,
    ) -> Result<(), SensorError> {
        #[cfg(target_os = "windows")]
        {
            imp::run(config, events, commands, resolve)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (config, events, commands, resolve);
            Err(SensorError::new(
                "unsupported",
                "the Windows sensor only runs on Windows",
            ))
        }
    }
}

/// Launch-at-login via `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
pub fn set_autostart(enabled: bool) -> Result<(), SensorError> {
    #[cfg(target_os = "windows")]
    {
        imp::set_autostart(enabled)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = enabled;
        Err(SensorError::new(
            "unsupported",
            "Windows autostart is only available on Windows",
        ))
    }
}

// ---------------------------------------------------------------------------
// Windows implementation (FFI)
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod imp {
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::path::PathBuf;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use tiny_skia::Pixmap;
    use windows_sys::core::{GUID, HRESULT};
    use windows_sys::Win32::Foundation::{
        GetLastError, COLORREF, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, E_POINTER, HWND, LPARAM,
        LRESULT, POINT, POINTL, RECT, SIZE, WPARAM,
    };
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, RGBQUAD,
    };
    use windows_sys::Win32::System::Com::{
        DVASPECT_CONTENT, DVTARGETDEVICE, FORMATETC, STGMEDIUM, TYMED_HGLOBAL,
    };
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::System::Ole::{
        OleInitialize, OleUninitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
        CF_HDROP, CF_UNICODETEXT, DROPEFFECT_COPY, DROPEFFECT_NONE,
    };
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    use windows_sys::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        ReleaseCapture, SetCapture, SetFocus, VK_BACK, VK_CONTROL, VK_DELETE, VK_END, VK_ESCAPE,
        VK_HOME, VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_MENU, VK_RCONTROL, VK_RETURN,
        VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_SHIFT, VK_TAB,
    };
    use windows_sys::Win32::UI::Shell::DragQueryFileW;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    use crate::panel::{PanelAction, PanelEvent, PanelKey, PanelState};
    use crate::{
        HeldMods, PanelOutcome, PanelSpec, SensorCommand, SensorConfig, SensorError, SensorEvent,
        Toast, WheelKind, WheelModel, WheelRequest, WheelResolver, ACTION_COPY_FILE,
        ACTION_COPY_PATH,
    };

    use super::{parse_hdrop_auto, GestureOutput, GestureTracker, HookRecord, DROPFILES_HEADER_LEN};

    const TIMER_ID: usize = 0x5543; // "UC" — the queue-drain timer.
    const WM_URUK_HANDBACK: u32 = WM_APP + 0x31;
    const WM_URUK_CLEANUP: u32 = WM_APP + 0x32;
    const WHEEL_PX: i32 = crate::geom::WHEEL_SIZE as i32;

    // -- Hook-side shared state -------------------------------------------
    // The hook callbacks may only touch these statics: a queue, and a few
    // atomics. Everything else happens on the sensor thread.

    static HOOK_QUEUE: Mutex<VecDeque<HookRecord>> = Mutex::new(VecDeque::new());
    static SHIFT_HELD: AtomicBool = AtomicBool::new(false);
    static CTRL_HELD: AtomicBool = AtomicBool::new(false);
    static ALT_HELD: AtomicBool = AtomicBool::new(false);
    static ESC_HELD: AtomicBool = AtomicBool::new(false);
    static LEFT_HELD: AtomicBool = AtomicBool::new(false);
    /// *mut SensorCore as usize (the core is pinned in a Box on the
    /// sensor thread for the whole `run` call).
    static CORE_PTR: AtomicUsize = AtomicUsize::new(0);
    static CLASSES_DONE: AtomicBool = AtomicBool::new(false);

    fn push_record(rec: HookRecord) {
        if let Ok(mut q) = HOOK_QUEUE.lock() {
            q.push_back(rec);
        }
    }

    /// Which modifier family a virtual-key code belongs to:
    /// 0 = Shift, 1 = Control, 2 = Alt (Menu).
    fn vk_modifier_slot(vk: u32) -> Option<usize> {
        if vk == VK_SHIFT as u32 || vk == VK_LSHIFT as u32 || vk == VK_RSHIFT as u32 {
            Some(0)
        } else if vk == VK_CONTROL as u32 || vk == VK_LCONTROL as u32 || vk == VK_RCONTROL as u32
        {
            Some(1)
        } else if vk == VK_MENU as u32 || vk == VK_LMENU as u32 || vk == VK_RMENU as u32 {
            Some(2)
        } else {
            None
        }
    }

    fn held_mods() -> HeldMods {
        HeldMods {
            shift: SHIFT_HELD.load(Ordering::SeqCst),
            control: CTRL_HELD.load(Ordering::SeqCst),
            alt: ALT_HELD.load(Ordering::SeqCst),
        }
    }

    fn modifier_atomic(slot: usize) -> &'static AtomicBool {
        match slot {
            0 => &SHIFT_HELD,
            1 => &CTRL_HELD,
            _ => &ALT_HELD,
        }
    }

    // -- Diagnostic log (v1.0.1: the sensor must never die silently) --
    //
    // `%APPDATA%\org.uruk.convert\sensor.log` — the same app-data
    // folder the shell keeps `history.json` in. Append mode; rotated
    // aside once per process start when past the cap. Every line is
    // flushed: this log is crash evidence first, a debug aid second.
    // The hook callbacks never log (near-empty contract); the sensor
    // thread logs its own decisions.

    static SLOG_FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);
    const SLOG_CAP_BYTES: u64 = 256 * 1024;

    fn slog_path() -> PathBuf {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("org.uruk.convert").join("sensor.log")
    }

    fn slog_init() {
        let path = slog_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() > SLOG_CAP_BYTES {
                let _ = std::fs::rename(&path, path.with_extension("log.old"));
            }
        }
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            if let Ok(mut guard) = SLOG_FILE.lock() {
                *guard = Some(f);
            }
        }
    }

    fn slog(msg: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let line = format!("[{}.{:03}] {}", now.as_secs(), now.subsec_millis(), msg);
        eprintln!("uruk-sensor: {line}");
        if let Ok(mut guard) = SLOG_FILE.lock() {
            if let Some(f) = guard.as_mut() {
                use std::io::Write as _;
                let _ = writeln!(f, "{line}");
                let _ = f.flush();
            }
        }
    }

    // -- Low-level hook callbacks (near-empty by contract) -----------------

    unsafe extern "system" fn mouse_hook_proc(
        code: i32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if code == HC_ACTION as i32 {
            let info = &*(lparam as *const MSLLHOOKSTRUCT);
            match wparam as u32 {
                WM_LBUTTONDOWN => {
                    LEFT_HELD.store(true, Ordering::SeqCst);
                    push_record(HookRecord::LeftButtonDown {
                        x: info.pt.x,
                        y: info.pt.y,
                    });
                }
                WM_LBUTTONUP => {
                    LEFT_HELD.store(false, Ordering::SeqCst);
                    push_record(HookRecord::LeftButtonUp {
                        x: info.pt.x,
                        y: info.pt.y,
                    });
                }
                WM_MOUSEMOVE => {
                    if LEFT_HELD.load(Ordering::SeqCst) {
                        push_record(HookRecord::MouseMove {
                            x: info.pt.x,
                            y: info.pt.y,
                        });
                    }
                }
                _ => {}
            }
        }
        CallNextHookEx(null_mut(), code, wparam, lparam)
    }

    unsafe extern "system" fn keyboard_hook_proc(
        code: i32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if code == HC_ACTION as i32 {
            let info = &*(lparam as *const KBDLLHOOKSTRUCT);
            let vk = info.vkCode;
            match wparam as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN => {
                    if vk == VK_ESCAPE as u32 {
                        // Edge only: auto-repeat must not re-fire.
                        if !ESC_HELD.swap(true, Ordering::SeqCst) {
                            push_record(HookRecord::EscapeDown);
                        }
                    } else if let Some(slot) = vk_modifier_slot(vk) {
                        // Edge per family: auto-repeat must not
                        // re-fire; the record carries the full set.
                        if !modifier_atomic(slot).swap(true, Ordering::SeqCst) {
                            push_record(HookRecord::Chord(held_mods()));
                        }
                    }
                }
                WM_KEYUP | WM_SYSKEYUP => {
                    if vk == VK_ESCAPE as u32 {
                        ESC_HELD.store(false, Ordering::SeqCst);
                    } else if let Some(slot) = vk_modifier_slot(vk) {
                        if modifier_atomic(slot).swap(false, Ordering::SeqCst) {
                            push_record(HookRecord::Chord(held_mods()));
                        }
                    }
                }
                _ => {}
            }
        }
        CallNextHookEx(null_mut(), code, wparam, lparam)
    }

    // -- Layered-window painting -------------------------------------------

    /// Blit a rendered pixmap into a layered window. `pixmap_to_bgra`
    /// produces exactly the premultiplied BGRA `UpdateLayeredWindow`
    /// wants with `AC_SRC_ALPHA`.
    unsafe fn paint_layered(hwnd: HWND, pm: &Pixmap, dst_x: i32, dst_y: i32) {
        let w = pm.width() as i32;
        let h = pm.height() as i32;
        if w <= 0 || h <= 0 || hwnd.is_null() {
            return;
        }
        let bgra = crate::render::pixmap_to_bgra(pm);
        let screen_dc: HDC = GetDC(null_mut());
        if screen_dc.is_null() {
            return;
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        if mem_dc.is_null() {
            ReleaseDC(null_mut(), screen_dc);
            return;
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down, matching pixmap row order
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: bgra.len() as u32,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [RGBQUAD {
                rgbBlue: 0,
                rgbGreen: 0,
                rgbRed: 0,
                rgbReserved: 0,
            }],
        };
        let mut bits: *mut c_void = null_mut();
        let hbmp: HBITMAP =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if !hbmp.is_null() && !bits.is_null() {
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, bgra.len());
            let old: HGDIOBJ = SelectObject(mem_dc, hbmp);
            let dst = POINT { x: dst_x, y: dst_y };
            let size = SIZE { cx: w, cy: h };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            UpdateLayeredWindow(
                hwnd,
                screen_dc,
                &dst,
                &size,
                mem_dc,
                &src,
                0 as COLORREF,
                &blend,
                ULW_ALPHA,
            );
            SelectObject(mem_dc, old);
            DeleteObject(hbmp);
        }
        DeleteDC(mem_dc);
        ReleaseDC(null_mut(), screen_dc);
    }

    // -- Clipboard (Stage 4 file actions) ------------------------------

    /// Put a raw blob on the clipboard under `format`, transferring
    /// ownership of the HGLOBAL to the system on success.
    unsafe fn set_clipboard_data(format: u32, bytes: &[u8]) {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
        if h.is_null() {
            return;
        }
        let ptr = GlobalLock(h);
        if ptr.is_null() {
            GlobalFree(h);
            return;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        GlobalUnlock(h);
        if OpenClipboard(null_mut()) != 0 {
            EmptyClipboard();
            if SetClipboardData(format, h).is_null() {
                GlobalFree(h);
            }
            CloseClipboard();
        } else {
            GlobalFree(h);
        }
    }

    /// Copy file: a `CF_HDROP` DROPFILES blob (wide, double-NUL list).
    unsafe fn set_clipboard_hdrop(paths: &[std::path::PathBuf]) {
        let mut blob: Vec<u8> = Vec::new();
        blob.extend_from_slice(&(DROPFILES_HEADER_LEN as u32).to_le_bytes());
        blob.extend_from_slice(&[0u8; 8]); // pt
        blob.extend_from_slice(&0u32.to_le_bytes()); // fNonClient
        blob.extend_from_slice(&1u32.to_le_bytes()); // fWide
        for p in paths {
            for u in p.display().to_string().encode_utf16() {
                blob.extend_from_slice(&u.to_le_bytes());
            }
            blob.extend_from_slice(&0u16.to_le_bytes());
        }
        blob.extend_from_slice(&0u16.to_le_bytes());
        set_clipboard_data(CF_HDROP as u32, &blob);
    }

    /// Copy path: `CF_UNICODETEXT`.
    unsafe fn set_clipboard_text(text: &str) {
        let mut bytes: Vec<u8> = Vec::new();
        for u in text.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        bytes.extend_from_slice(&0u16.to_le_bytes());
        set_clipboard_data(CF_UNICODETEXT as u32, &bytes);
    }

    /// Register one window class; a real failure (anything but
    /// "already exists") goes to the diagnostic log — in v1.0.0 this
    /// result was discarded, so a failed class silently poisoned every
    /// later `CreateWindowExW`.
    unsafe fn register_class(wc: &WNDCLASSEXW, name: &str) {
        if RegisterClassExW(wc) == 0 {
            let err = GetLastError();
            // ERROR_CLASS_ALREADY_EXISTS (1410) is fine — idempotent.
            if err != 1410 {
                slog(&format!(
                    "RegisterClassExW({name}) failed (GetLastError {err})"
                ));
            }
        }
    }

    unsafe fn ensure_classes(hinst: windows_sys::Win32::Foundation::HINSTANCE) {
        if CLASSES_DONE.swap(true, Ordering::SeqCst) {
            return;
        }
        for (proc, class_name, label) in [
            (
                overlay_wndproc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
                windows_sys::w!("UrukConvertWheelOverlay"),
                "UrukConvertWheelOverlay",
            ),
            (
                overlay_wndproc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
                windows_sys::w!("UrukConvertToast"),
                "UrukConvertToast",
            ),
            (
                host_wndproc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
                windows_sys::w!("UrukConvertSensorHost"),
                "UrukConvertSensorHost",
            ),
        ] {
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: null_mut(),
                hCursor: null_mut(),
                hbrBackground: null_mut(),
                lpszMenuName: null(),
                lpszClassName: class_name,
                hIconSm: null_mut(),
            };
            register_class(&wc, label);
        }
        // Stage 4: the HUD panel has its own proc (it takes real
        // mouse + keyboard input, unlike the click-through overlays).
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(panel_wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: null_mut(),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: windows_sys::w!("UrukConvertPanel"),
            hIconSm: null_mut(),
        };
        register_class(&wc, "UrukConvertPanel");
    }

    unsafe extern "system" fn panel_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        fn lp_xy(lparam: LPARAM) -> (f32, f32) {
            let x = (lparam & 0xFFFF) as i16 as i32;
            let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;
            (x as f32, y as f32)
        }
        match msg {
            WM_LBUTTONDOWN => {
                let (x, y) = lp_xy(lparam);
                SetCapture(hwnd);
                with_core(|core| core.panel_event(PanelEvent::Down { x, y }));
                0
            }
            WM_MOUSEMOVE => {
                if wparam & 0x0001 != 0 {
                    let (x, y) = lp_xy(lparam);
                    with_core(|core| core.panel_event(PanelEvent::Move { x, y }));
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_LBUTTONUP => {
                let (x, y) = lp_xy(lparam);
                with_core(|core| core.panel_event(PanelEvent::Up { x, y }));
                ReleaseCapture();
                0
            }
            WM_CHAR => {
                if let Some(c) = char::from_u32(wparam as u32) {
                    if !c.is_control() {
                        with_core(|core| core.panel_event(PanelEvent::Key(PanelKey::Char(c))));
                    }
                }
                0
            }
            WM_KEYDOWN => {
                let key = match wparam as u32 {
                    v if v == VK_ESCAPE as u32 => Some(PanelKey::Escape),
                    v if v == VK_RETURN as u32 => Some(PanelKey::Enter),
                    v if v == VK_TAB as u32 => Some(PanelKey::Tab),
                    v if v == VK_BACK as u32 => Some(PanelKey::Backspace),
                    v if v == VK_DELETE as u32 => Some(PanelKey::Delete),
                    v if v == VK_LEFT as u32 => Some(PanelKey::Left),
                    v if v == VK_RIGHT as u32 => Some(PanelKey::Right),
                    v if v == VK_HOME as u32 => Some(PanelKey::Home),
                    v if v == VK_END as u32 => Some(PanelKey::End),
                    _ => None,
                };
                match key {
                    Some(k) => {
                        with_core(|core| core.panel_event(PanelEvent::Key(k)));
                        0
                    }
                    None => DefWindowProcW(hwnd, msg, wparam, lparam),
                }
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe extern "system" fn overlay_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            // Posted from inside a COM callback; dispatched on this
            // thread as soon as the callback returns — that ordering is
            // what makes the OLE hand-back fast (see module docs).
            WM_URUK_HANDBACK => {
                with_core(|core| {
                    if core.pending_handback {
                        core.dismiss_overlay();
                    }
                });
                0
            }
            WM_URUK_CLEANUP => {
                with_core(|core| core.dismiss_overlay());
                0
            }
            // Never steal activation from the drag's source app.
            WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    fn with_core(f: impl FnOnce(&mut SensorCore)) {
        let p = CORE_PTR.load(Ordering::SeqCst);
        if p != 0 {
            unsafe { f(&mut *(p as *mut SensorCore)) }
        }
    }

    fn core_shutdown() -> bool {
        let p = CORE_PTR.load(Ordering::SeqCst);
        if p == 0 {
            return false;
        }
        unsafe { (*(p as *const SensorCore)).shutdown }
    }

    /// The sensor host window's proc. The host is a message-only
    /// window created on the sensor thread purely to own the 8 ms
    /// queue-drain timer: an hWnd timer honors its ID, unlike the
    /// v1.0.0 thread timer whose system-assigned ID never matched the
    /// loop's check (see the module post-mortem). WM_TIMER → tick.
    unsafe extern "system" fn host_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_TIMER => {
                with_core(|core| core.tick());
                if core_shutdown() {
                    PostQuitMessage(0);
                }
                0
            }
            WM_CLOSE => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    // -- IDataObject access through its (hand-mirrored) vtbl ----------------
    //
    // windows-sys 0.59 exposes the OLE functions and the FORMATETC /
    // STGMEDIUM types, but treats IDataObject itself as an opaque
    // pointer. This struct mirrors the generated vtbl layout from the
    // Windows SDK (objidl.h) field-for-field, in declaration order, and
    // every call goes through it — never through guessed offsets.

    #[repr(C)]
    struct IDataObjectVtbl {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        get_data:
            unsafe extern "system" fn(*mut c_void, *const FORMATETC, *mut STGMEDIUM) -> HRESULT,
        get_data_here:
            unsafe extern "system" fn(*mut c_void, *const FORMATETC, *mut STGMEDIUM) -> HRESULT,
        query_get_data: unsafe extern "system" fn(*mut c_void, *const FORMATETC) -> HRESULT,
        get_canonical_format_etc:
            unsafe extern "system" fn(*mut c_void, *const FORMATETC, *mut FORMATETC) -> HRESULT,
        set_data: unsafe extern "system" fn(
            *mut c_void,
            *const FORMATETC,
            *const STGMEDIUM,
            i32,
        ) -> HRESULT,
        enum_format_etc: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
        d_advise: unsafe extern "system" fn(
            *mut c_void,
            *const FORMATETC,
            u32,
            *mut c_void,
            *mut u32,
        ) -> HRESULT,
        d_unadvise: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
        enum_d_advise: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    }

    fn hdrop_format() -> FORMATETC {
        FORMATETC {
            cfFormat: CF_HDROP,
            ptd: null_mut::<DVTARGETDEVICE>(),
            dwAspect: DVASPECT_CONTENT,
            lindex: -1,
            tymed: TYMED_HGLOBAL as u32,
        }
    }

    unsafe fn data_vtbl(data: *mut c_void) -> *const IDataObjectVtbl {
        if data.is_null() {
            return null();
        }
        *(data as *const *const IDataObjectVtbl)
    }

    /// QueryGetData(CF_HDROP) — the DragEnter verification.
    unsafe fn data_has_hdrop(data: *mut c_void) -> bool {
        let vtbl = data_vtbl(data);
        if vtbl.is_null() {
            return false;
        }
        ((*vtbl).query_get_data)(data, &hdrop_format()) == 0 // S_OK
    }

    unsafe fn data_get_hdrop_medium(data: *mut c_void) -> Option<STGMEDIUM> {
        let vtbl = data_vtbl(data);
        if vtbl.is_null() {
            return None;
        }
        let mut medium: STGMEDIUM = std::mem::zeroed();
        let hr = ((*vtbl).get_data)(data, &hdrop_format(), &mut medium);
        if hr == 0 && medium.tymed == TYMED_HGLOBAL as u32 && !medium.u.hGlobal.is_null() {
            Some(medium)
        } else {
            None
        }
    }

    /// Paths from a CF_HDROP medium via the pure DROPFILES parser. Used
    /// at DragEnter time, where knowing the paths early is what lets the
    /// resolver confirm (or refuse) the drag before we paint segments.
    unsafe fn paths_via_parse(data: *mut c_void) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(mut medium) = data_get_hdrop_medium(data) {
            let hg = medium.u.hGlobal;
            let locked = GlobalLock(hg);
            if !locked.is_null() {
                let size = GlobalSize(hg);
                if size > 0 {
                    let bytes = std::slice::from_raw_parts(locked as *const u8, size);
                    out = parse_hdrop_auto(bytes)
                        .into_iter()
                        .map(PathBuf::from)
                        .collect();
                }
                GlobalUnlock(hg);
            }
            ReleaseStgMedium(&mut medium);
        }
        out
    }

    /// Paths from a CF_HDROP medium via `DragQueryFileW` — the Drop
    /// path. The shell decodes with the real codepage, so this is the
    /// authoritative list.
    unsafe fn paths_via_drag_query(data: *mut c_void) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(mut medium) = data_get_hdrop_medium(data) {
            let hg = medium.u.hGlobal;
            let locked = GlobalLock(hg);
            if !locked.is_null() {
                let hdrop = locked as windows_sys::Win32::UI::Shell::HDROP;
                let count = DragQueryFileW(hdrop, u32::MAX, null_mut(), 0);
                for i in 0..count {
                    let len = DragQueryFileW(hdrop, i, null_mut(), 0);
                    if len == 0 {
                        continue;
                    }
                    let mut buf = vec![0u16; len as usize + 1];
                    let got = DragQueryFileW(hdrop, i, buf.as_mut_ptr(), buf.len() as u32);
                    buf.truncate(got as usize);
                    out.push(PathBuf::from(String::from_utf16_lossy(&buf)));
                }
                GlobalUnlock(hg);
            }
            ReleaseStgMedium(&mut medium);
        }
        out
    }

    // -- Hand-rolled IDropTarget --------------------------------------------
    //
    // One object per overlay, created at summon, freed after the revoke
    // plus our own release drops the count to zero. Layout follows the
    // COM ABI: vtbl pointer first, then our fields; OLE only ever sees
    // the object through `RegisterDragDrop(hwnd, ptr)`.

    #[repr(C)]
    struct DropTargetVtbl {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        drag_enter:
            unsafe extern "system" fn(*mut c_void, *mut c_void, u32, POINTL, *mut u32) -> HRESULT,
        drag_over: unsafe extern "system" fn(*mut c_void, u32, POINTL, *mut u32) -> HRESULT,
        drag_leave: unsafe extern "system" fn(*mut c_void) -> HRESULT,
        drop: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, POINTL, *mut u32) -> HRESULT,
    }

    #[repr(C)]
    struct DropTargetObject {
        vtbl: *const DropTargetVtbl,
        ref_count: u32,
        core: *mut SensorCore,
    }

    const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
    const IID_IDROP_TARGET: GUID = GUID::from_u128(0x00000122_0000_0000_c000_000000000046);
    const E_NOINTERFACE: HRESULT = 0x8000_4002u32 as i32;

    fn guid_eq(a: &GUID, b: &GUID) -> bool {
        a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
    }

    unsafe extern "system" fn dt_query_interface(
        this: *mut c_void,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> HRESULT {
        if ppv.is_null() || riid.is_null() {
            return E_POINTER;
        }
        let iid = *riid;
        if guid_eq(&iid, &IID_IUNKNOWN) || guid_eq(&iid, &IID_IDROP_TARGET) {
            *ppv = this;
            dt_add_ref(this);
            0 // S_OK
        } else {
            *ppv = null_mut();
            E_NOINTERFACE
        }
    }

    unsafe extern "system" fn dt_add_ref(this: *mut c_void) -> u32 {
        let obj = &mut *(this as *mut DropTargetObject);
        obj.ref_count += 1;
        obj.ref_count
    }

    unsafe extern "system" fn dt_release(this: *mut c_void) -> u32 {
        let obj = &mut *(this as *mut DropTargetObject);
        obj.ref_count = obj.ref_count.saturating_sub(1);
        let count = obj.ref_count;
        if count == 0 {
            drop(Box::from_raw(this as *mut DropTargetObject));
        }
        count
    }

    unsafe fn core_of(this: *mut c_void) -> Option<*mut SensorCore> {
        let obj = &*(this as *const DropTargetObject);
        if obj.core.is_null() {
            None
        } else {
            Some(obj.core)
        }
    }

    unsafe extern "system" fn dt_drag_enter(
        this: *mut c_void,
        data: *mut c_void,
        _key_state: u32,
        pt: POINTL,
        effect: *mut u32,
    ) -> HRESULT {
        if effect.is_null() {
            return 0;
        }
        *effect = DROPEFFECT_NONE;
        if let Some(core) = core_of(this) {
            (*core).on_drag_enter(data, pt, effect);
        }
        0 // S_OK
    }

    unsafe extern "system" fn dt_drag_over(
        this: *mut c_void,
        _key_state: u32,
        pt: POINTL,
        effect: *mut u32,
    ) -> HRESULT {
        if effect.is_null() {
            return 0;
        }
        *effect = DROPEFFECT_NONE;
        if let Some(core) = core_of(this) {
            (*core).on_drag_over(pt, effect);
        }
        0
    }

    unsafe extern "system" fn dt_drag_leave(this: *mut c_void) -> HRESULT {
        if let Some(core) = core_of(this) {
            (*core).on_drag_leave();
        }
        0
    }

    unsafe extern "system" fn dt_drop(
        this: *mut c_void,
        data: *mut c_void,
        _key_state: u32,
        pt: POINTL,
        effect: *mut u32,
    ) -> HRESULT {
        if effect.is_null() {
            return 0;
        }
        *effect = DROPEFFECT_NONE;
        if let Some(core) = core_of(this) {
            (*core).on_drop(data, pt, effect);
        }
        0
    }

    static DROP_TARGET_VTBL: DropTargetVtbl = DropTargetVtbl {
        query_interface: dt_query_interface,
        add_ref: dt_add_ref,
        release: dt_release,
        drag_enter: dt_drag_enter,
        drag_over: dt_drag_over,
        drag_leave: dt_drag_leave,
        drop: dt_drop,
    };

    unsafe fn drop_target_release(obj: *mut DropTargetObject) {
        let vtbl = (*(obj as *const DropTargetObject)).vtbl;
        ((*vtbl).release)(obj as *mut c_void);
    }

    // -- The sensor core (all on the start() thread) -------------------------

    struct SensorCore {
        config: SensorConfig,
        events: std::sync::mpsc::Sender<SensorEvent>,
        commands: std::sync::mpsc::Receiver<SensorCommand>,
        resolve: WheelResolver,
        gesture: GestureTracker,

        overlay: HWND,
        drop_target: *mut DropTargetObject,
        registered: bool,
        summoned: bool,
        /// A wheel was shown for the current drag (terminal-event bookkeeping).
        drag_shown: bool,
        wheel_centre: (i32, i32),
        model: Option<WheelModel>,
        paths: Vec<PathBuf>,
        hover: Option<usize>,
        /// DragEnter confirmed files and the resolver accepted them.
        participating: bool,
        pending_handback: bool,
        converted: bool,
        terminal_sent: bool,

        toast_hwnd: HWND,
        toast_until: Option<Instant>,
        shutdown: bool,
        /// Set on the first `tick` — proof in the log that the pump is
        /// actually draining (the thing v1.0.0 never did).
        pump_logged: bool,

        // Stage 4.
        /// Wheel kind the current chord selected (set at summon).
        pending_kind: WheelKind,
        panel_hwnd: HWND,
        panel_state: Option<PanelState>,
        panel_preview: Option<tiny_skia::Pixmap>,
        panel_pos: (i32, i32),
    }

    fn generic_model() -> WheelModel {
        // Hub-only wheel painted between summon and DragEnter: we know
        // a drag exists, not yet what it carries.
        WheelModel {
            kind: WheelKind::Convert,
            segments: Vec::new(),
            hub_ext: "FILE".to_string(),
            hub_title: String::new(),
            file_count: 0,
        }
    }

    impl SensorCore {
        // -- command / queue pump (timer tick) --

        unsafe fn tick(&mut self) {
            if !self.pump_logged {
                self.pump_logged = true;
                slog("pump alive: first tick — hook queue + command channel draining");
            }
            loop {
                match self.commands.try_recv() {
                    Ok(SensorCommand::Shutdown) => {
                        slog("command: shutdown");
                        self.shutdown = true;
                    }
                    Ok(SensorCommand::UpdateConfig(cfg)) => {
                        slog(&format!(
                            "command: update config (sensing enabled={})",
                            cfg.enabled
                        ));
                        self.gesture.set_config(&cfg);
                        self.config = cfg;
                    }
                    Ok(SensorCommand::ShowToast(toast)) => self.show_toast(toast),
                    Ok(SensorCommand::ShowPanel(spec)) => {
                        // Replacing an open panel closes it as Cancelled.
                        if let Some(old) = self.panel_state.take() {
                            let _ = self.events.send(SensorEvent::PanelClosed {
                                id: old.spec.id.clone(),
                                outcome: PanelOutcome::Cancelled,
                            });
                        }
                        self.panel_preview = None;
                        self.destroy_panel_window();
                        self.show_panel(spec);
                    }
                    Ok(SensorCommand::UpdatePanelCanvas { id, image_png, page, pages }) => {
                        self.update_panel_canvas(&id, &image_png, page, pages);
                    }
                    Ok(SensorCommand::ClosePanel { .. }) => {
                        self.panel_state = None;
                        self.panel_preview = None;
                        self.destroy_panel_window();
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        // Host is gone; an agent with no host should exit.
                        self.shutdown = true;
                        break;
                    }
                }
            }
            if self.shutdown {
                return;
            }

            let records: Vec<HookRecord> = match HOOK_QUEUE.lock() {
                Ok(mut q) => q.drain(..).collect(),
                Err(_) => Vec::new(),
            };
            for rec in records {
                if let Some(out) = self.gesture.on_record(rec) {
                    self.on_gesture_output(out);
                    if self.shutdown {
                        return;
                    }
                }
            }

            // Hand-back failsafe: the posted message is the primary
            // path; if it somehow never dispatched, the next tick after
            // the COM callback still revokes within one timer period of
            // the thread pumping again.
            if self.pending_handback {
                self.dismiss_overlay();
            }

            if let Some(until) = self.toast_until {
                if Instant::now() >= until {
                    self.hide_toast();
                    self.toast_until = None;
                }
            }
        }

        unsafe fn on_gesture_output(&mut self, out: GestureOutput) {
            match out {
                GestureOutput::Candidate { x, y } => {
                    slog(&format!("drag candidate at ({x}, {y}) — watching for the summon chord"));
                }
                GestureOutput::Summon { x, y, kind } => {
                    self.pending_kind = kind;
                    if self.config.enabled && !self.summoned && self.panel_state.is_none() {
                        slog(&format!("summon {kind:?} at ({x}, {y}) — showing the wheel"));
                        self.show_overlay(x, y);
                    } else {
                        slog(&format!(
                            "summon {kind:?} at ({x}, {y}) SKIPPED (enabled={}, summoned={}, panel_open={})",
                            self.config.enabled,
                            self.summoned,
                            self.panel_state.is_some()
                        ));
                    }
                }
                GestureOutput::Resummon { kind, .. } => {
                    self.pending_kind = kind;
                    if self.summoned && self.participating && !self.paths.is_empty() {
                        // Paths are known: re-resolve in place for the
                        // new wheel kind.
                        let request = WheelRequest {
                            kind,
                            paths: self.paths.clone(),
                            mime_types: vec!["CF_HDROP".to_string()],
                        };
                        match (self.resolve)(&request) {
                            Some(model) => {
                                self.model = Some(model);
                                self.hover = None;
                                self.repaint_wheel();
                            }
                            None => self.dismiss_overlay(),
                        }
                    } else if self.summoned {
                        // Nothing classified yet: restart the overlay;
                        // OLE re-enters when the drag next moves.
                        let (cx, cy) = self.wheel_centre;
                        self.dismiss_overlay();
                        if self.config.enabled {
                            self.show_overlay(cx, cy);
                        }
                    }
                }
                GestureOutput::EscapeCancel => {
                    if self.summoned {
                        self.dismiss_overlay();
                    }
                    if self.drag_shown && !self.converted && !self.terminal_sent {
                        let _ = self.events.send(SensorEvent::DragEnded);
                    }
                    self.drag_shown = false;
                    self.converted = false;
                    self.terminal_sent = false;
                }
                GestureOutput::Ended => {
                    if self.summoned {
                        self.dismiss_overlay();
                    }
                    if self.drag_shown && !self.converted && !self.terminal_sent {
                        let _ = self.events.send(SensorEvent::DragEnded);
                    }
                    self.drag_shown = false;
                    self.converted = false;
                    self.terminal_sent = false;
                }
            }
        }

        // -- overlay lifecycle --

        unsafe fn show_overlay(&mut self, cx: i32, cy: i32) {
            if !self.overlay.is_null() || self.summoned {
                return;
            }
            let hinst = GetModuleHandleW(null());
            ensure_classes(hinst);
            let x = cx - WHEEL_PX / 2;
            let y = cy - WHEEL_PX / 2;
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                windows_sys::w!("UrukConvertWheelOverlay"),
                null(),
                WS_POPUP,
                x,
                y,
                WHEEL_PX,
                WHEEL_PX,
                null_mut(),
                null_mut(),
                hinst,
                null(),
            );
            if hwnd.is_null() {
                slog(&format!(
                    "show_overlay: CreateWindowExW failed (GetLastError {})",
                    GetLastError()
                ));
                return;
            }
            self.overlay = hwnd;
            self.wheel_centre = (cx, cy);
            self.summoned = true;
            self.drag_shown = true;
            self.converted = false;
            self.terminal_sent = false;
            self.model = None;
            self.hover = None;
            self.participating = false;
            self.pending_handback = false;

            // Register for OLE drops *before* painting: the drag is
            // already live, and routing it to us is the whole point.
            let obj = Box::into_raw(Box::new(DropTargetObject {
                vtbl: &DROP_TARGET_VTBL,
                ref_count: 1,
                core: self as *mut SensorCore,
            }));
            let hr = RegisterDragDrop(hwnd, obj as *mut c_void);
            if hr != 0 {
                slog(&format!(
                    "show_overlay: RegisterDragDrop failed (HRESULT 0x{:08X}) — wheel torn down",
                    hr as u32
                ));
                drop_target_release(obj);
                DestroyWindow(hwnd);
                self.overlay = null_mut();
                self.summoned = false;
                self.drag_shown = false;
                return;
            }
            self.drop_target = obj;
            self.registered = true;

            self.repaint_wheel();
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            slog("overlay shown + RegisterDragDrop OK — awaiting DragEnter");
            let _ = self.events.send(SensorEvent::Summoned { x: cx, y: cy });
        }

        /// Hide the overlay and revoke the drop registration. Idempotent.
        unsafe fn dismiss_overlay(&mut self) {
            let hwnd = self.overlay;
            if !hwnd.is_null() {
                slog("overlay dismissed (RevokeDragDrop + DestroyWindow)");
                if self.registered {
                    RevokeDragDrop(hwnd);
                    self.registered = false;
                }
                if !self.drop_target.is_null() {
                    let obj = self.drop_target;
                    self.drop_target = null_mut();
                    drop_target_release(obj);
                }
                DestroyWindow(hwnd);
                self.overlay = null_mut();
            }
            self.summoned = false;
            self.model = None;
            self.paths.clear();
            self.hover = None;
            self.participating = false;
            self.pending_handback = false;
        }

        unsafe fn repaint_wheel(&mut self) {
            if self.overlay.is_null() {
                return;
            }
            let (model, hover) = match &self.model {
                Some(m) => (m.clone(), self.hover),
                None => (generic_model(), None),
            };
            let pm = crate::render::render_wheel(&model, hover);
            paint_layered(
                self.overlay,
                &pm,
                self.wheel_centre.0 - WHEEL_PX / 2,
                self.wheel_centre.1 - WHEEL_PX / 2,
            );
        }

        unsafe fn post_overlay(&self, msg: u32) {
            if !self.overlay.is_null() {
                PostMessageW(self.overlay, msg, 0, 0);
            }
        }

        /// The hand-back: we are not this drag's target after all.
        /// `DROPEFFECT_NONE` has already been written by the caller; here
        /// we arrange the revoke + hide to run *immediately after* the
        /// current COM callback returns (posted messages dispatch before
        /// OLE delivers the next DragOver), with the tick as a failsafe.
        unsafe fn request_handback(&mut self) {
            slog("handback: this drag is not ours — revoking so the real target re-acquires it");
            self.participating = false;
            self.pending_handback = true;
            self.post_overlay(WM_URUK_HANDBACK);
        }

        // -- IDropTarget callbacks (invoked by OLE on this thread) --

        unsafe fn on_drag_enter(&mut self, data: *mut c_void, _pt: POINTL, effect: *mut u32) {
            // UIPI note: drags from elevated processes never reach this
            // callback at all — there is nothing here to detect or log.
            if !data_has_hdrop(data) {
                slog("DragEnter: no CF_HDROP in the drag — handing back");
                self.request_handback();
                return;
            }
            let paths = paths_via_parse(data);
            if paths.is_empty() {
                slog("DragEnter: CF_HDROP present but zero paths parsed — handing back");
                self.request_handback();
                return;
            }
            let request = WheelRequest {
                kind: self.pending_kind,
                paths: paths.clone(),
                mime_types: vec!["CF_HDROP".to_string()],
            };
            match (self.resolve)(&request) {
                Some(model) => {
                    slog(&format!(
                        "DragEnter accepted: {} path(s), wheel kind {:?} with {} segment(s)",
                        paths.len(),
                        model.kind,
                        model.segments.len()
                    ));
                    self.model = Some(model);
                    self.paths = paths;
                    self.participating = true;
                    self.hover = None;
                    self.repaint_wheel();
                    *effect = DROPEFFECT_COPY;
                    let _ = self.events.send(SensorEvent::DragClassified {
                        mime_types: vec!["CF_HDROP".to_string()],
                    });
                }
                None => {
                    slog("DragEnter: the resolver declined this drag — handing back");
                    self.request_handback();
                }
            }
        }

        unsafe fn on_drag_over(&mut self, pt: POINTL, effect: *mut u32) {
            if self.pending_handback || !self.participating {
                return; // effect already DROPEFFECT_NONE
            }
            let seg = self.segment_under(pt);
            if seg != self.hover {
                self.hover = seg;
                self.repaint_wheel();
            }
            if seg.is_some() {
                *effect = DROPEFFECT_COPY;
            }
        }

        unsafe fn on_drag_leave(&mut self) {
            // The pointer left the wheel, but the drag is still ours to
            // lose: stay registered (a re-entry produces a fresh
            // DragEnter) and just clear the hover highlight. Final
            // teardown happens on Drop, Escape, or button-up.
            if self.hover.is_some() {
                self.hover = None;
                self.repaint_wheel();
            }
        }

        unsafe fn on_drop(&mut self, data: *mut c_void, pt: POINTL, effect: *mut u32) {
            let seg = self.segment_under(pt).or(self.hover);
            let mut converted_now = false;
            if self.participating {
                if let (Some(i), Some(model)) = (seg, self.model.clone()) {
                    if i < model.segments.len() {
                        let mut paths = paths_via_drag_query(data);
                        if paths.is_empty() {
                            paths = self.paths.clone();
                        }
                        if !paths.is_empty() {
                            let segment = model.segments[i].clone();
                            let dispatched = match model.kind {
                                WheelKind::Convert => {
                                    let _ = self.events.send(SensorEvent::ConvertRequested {
                                        paths: paths.clone(),
                                        target: segment.id.clone(),
                                    });
                                    true
                                }
                                WheelKind::Tools => {
                                    let _ = self.events.send(SensorEvent::ToolRequested {
                                        paths: paths.clone(),
                                        tool: segment.id.clone(),
                                        x: pt.x,
                                        y: pt.y,
                                    });
                                    true
                                }
                                WheelKind::Actions => {
                                    if segment.id == ACTION_COPY_FILE {
                                        set_clipboard_hdrop(&paths);
                                        let _ = self.events.send(SensorEvent::ActionDone {
                                            paths: paths.clone(),
                                            action: segment.id.clone(),
                                        });
                                        true
                                    } else if segment.id == ACTION_COPY_PATH {
                                        let text = paths
                                            .iter()
                                            .map(|p| p.display().to_string())
                                            .collect::<Vec<_>>()
                                            .join("\r\n");
                                        set_clipboard_text(&text);
                                        let _ = self.events.send(SensorEvent::ActionDone {
                                            paths: paths.clone(),
                                            action: segment.id.clone(),
                                        });
                                        true
                                    } else {
                                        let _ = self.events.send(
                                            SensorEvent::ActionRequested {
                                                paths: paths.clone(),
                                                action: segment.id.clone(),
                                                x: pt.x,
                                                y: pt.y,
                                            },
                                        );
                                        true
                                    }
                                }
                            };
                            if dispatched {
                                slog(&format!(
                                    "drop dispatched: kind {:?}, segment '{}', {} path(s)",
                                    model.kind,
                                    segment.id,
                                    paths.len()
                                ));
                                self.converted = true;
                                converted_now = true;
                                *effect = DROPEFFECT_COPY;
                            }
                        }
                    }
                }
            }
            if !converted_now {
                slog("drop refused (hub / gap / outside the segments, or never confirmed)");
                // Hub / gap / outside drop, or a drag we never
                // confirmed: refuse, and this is the drag's end for us.
                if self.drag_shown && !self.terminal_sent {
                    let _ = self.events.send(SensorEvent::DragEnded);
                    self.terminal_sent = true;
                }
            }
            // Teardown is posted, not done inline: revoking from inside
            // the Drop callback itself is the one re-entrancy OLE does
            // not want; the posted message runs as soon as we return.
            self.post_overlay(WM_URUK_CLEANUP);
        }

        fn segment_under(&self, pt: POINTL) -> Option<usize> {
            let model = self.model.as_ref()?;
            let dx = (pt.x - self.wheel_centre.0) as f32;
            let dy = (pt.y - self.wheel_centre.1) as f32;
            crate::geom::segment_at(model.segments.len(), dx, dy)
        }

        // -- toasts --

        unsafe fn show_toast(&mut self, toast: Toast) {
            let rtl = self.config.lang.is_rtl();
            let pm = crate::render::render_toast(toast.kind, &toast.title, &toast.body, rtl);
            let w = pm.width() as i32;
            let h = pm.height() as i32;
            let mut area: RECT = std::mem::zeroed();
            SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut area as *mut RECT as *mut c_void, 0);
            // Bottom-centre of the primary work area.
            let x = area.left + ((area.right - area.left) - w) / 2;
            let y = area.bottom - h - 24;
            if self.toast_hwnd.is_null() {
                let hinst = GetModuleHandleW(null());
                ensure_classes(hinst);
                let hwnd = CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                    windows_sys::w!("UrukConvertToast"),
                    null(),
                    WS_POPUP,
                    x,
                    y,
                    w,
                    h,
                    null_mut(),
                    null_mut(),
                    hinst,
                    null(),
                );
                if hwnd.is_null() {
                    return;
                }
                self.toast_hwnd = hwnd;
            } else {
                SetWindowPos(self.toast_hwnd, HWND_TOPMOST, x, y, w, h, SWP_SHOWWINDOW);
            }
            paint_layered(self.toast_hwnd, &pm, x, y);
            ShowWindow(self.toast_hwnd, SW_SHOWNOACTIVATE);
            self.toast_until = Some(Instant::now() + Duration::from_millis(3600));
        }

        unsafe fn hide_toast(&mut self) {
            if !self.toast_hwnd.is_null() {
                ShowWindow(self.toast_hwnd, SW_HIDE);
            }
        }

        unsafe fn destroy_toast(&mut self) {
            if !self.toast_hwnd.is_null() {
                DestroyWindow(self.toast_hwnd);
                self.toast_hwnd = null_mut();
            }
            self.toast_until = None;
        }

        // -- Stage 4: HUD panel host --

        unsafe fn show_panel(&mut self, spec: PanelSpec) {
            let state = PanelState::new(spec);
            let (w, h) = (state.layout.width as i32, state.layout.height as i32);
            let (ax, ay) = state.spec.anchor;
            let mut area: RECT = std::mem::zeroed();
            SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut area as *mut RECT as *mut c_void, 0);
            let mut x = (ax - 40).max(area.left + 4);
            if x + w > area.right - 4 {
                x = (area.right - 4 - w).max(area.left + 4);
            }
            let mut y = (ay + 18).max(area.top + 4);
            if y + h > area.bottom - 4 {
                y = (area.bottom - 4 - h).max(area.top + 4);
            }
            self.panel_preview = state
                .spec
                .canvas
                .as_ref()
                .and_then(|cv| std::fs::read(&cv.image_png).ok())
                .and_then(|b| crate::panel::decode_png(&b));
            self.panel_state = Some(state);
            self.panel_pos = (x, y);
            if self.panel_hwnd.is_null() {
                let hinst = GetModuleHandleW(null());
                ensure_classes(hinst);
                let hwnd = CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_TOPMOST,
                    windows_sys::w!("UrukConvertPanel"),
                    null(),
                    WS_POPUP,
                    x,
                    y,
                    w,
                    h,
                    null_mut(),
                    null_mut(),
                    hinst,
                    null(),
                );
                if hwnd.is_null() {
                    self.panel_state = None;
                    return;
                }
                self.panel_hwnd = hwnd;
            }
            self.repaint_panel();
            ShowWindow(self.panel_hwnd, SW_SHOW);
            SetForegroundWindow(self.panel_hwnd);
            SetFocus(self.panel_hwnd);
        }

        unsafe fn repaint_panel(&mut self) {
            if self.panel_hwnd.is_null() {
                return;
            }
            let pm = match &self.panel_state {
                Some(st) => crate::panel::render_panel(st, self.panel_preview.as_ref()),
                None => return,
            };
            paint_layered(self.panel_hwnd, &pm, self.panel_pos.0, self.panel_pos.1);
        }

        unsafe fn destroy_panel_window(&mut self) {
            if !self.panel_hwnd.is_null() {
                DestroyWindow(self.panel_hwnd);
                self.panel_hwnd = null_mut();
            }
        }

        unsafe fn update_panel_canvas(
            &mut self,
            id: &str,
            image_png: &std::path::PathBuf,
            page: u32,
            pages: u32,
        ) {
            let matches = matches!(&self.panel_state, Some(st) if st.spec.id == id);
            if !matches {
                return;
            }
            if let Some(pm) = std::fs::read(image_png)
                .ok()
                .and_then(|b| crate::panel::decode_png(&b))
            {
                self.panel_preview = Some(pm);
            }
            if let Some(st) = &mut self.panel_state {
                st.set_canvas_page(page, pages);
            }
            self.repaint_panel();
        }

        /// Feed one input event into the open panel (from panel_wndproc).
        unsafe fn panel_event(&mut self, ev: PanelEvent) {
            let action = match &mut self.panel_state {
                Some(st) => st.handle(ev),
                None => return,
            };
            match action {
                PanelAction::None => {}
                PanelAction::Redraw => self.repaint_panel(),
                PanelAction::NeedPage(page) => {
                    if let Some(st) = &self.panel_state {
                        let _ = self.events.send(SensorEvent::PanelPageRequest {
                            id: st.spec.id.clone(),
                            page,
                        });
                    }
                }
                PanelAction::Apply => {
                    if let Some(st) = self.panel_state.take() {
                        let outcome = st.outcome();
                        let id = st.spec.id.clone();
                        self.panel_preview = None;
                        self.destroy_panel_window();
                        let _ = self.events.send(SensorEvent::PanelClosed { id, outcome });
                    }
                }
                PanelAction::Cancel => {
                    if let Some(st) = self.panel_state.take() {
                        let id = st.spec.id.clone();
                        self.panel_preview = None;
                        self.destroy_panel_window();
                        let _ = self.events.send(SensorEvent::PanelClosed {
                            id,
                            outcome: PanelOutcome::Cancelled,
                        });
                    }
                }
            }
        }
    }

    // -- Entry points --------------------------------------------------------

    pub fn run(
        config: SensorConfig,
        events: std::sync::mpsc::Sender<SensorEvent>,
        commands: std::sync::mpsc::Receiver<SensorCommand>,
        resolve: WheelResolver,
    ) -> Result<(), SensorError> {
        unsafe {
            slog_init();
            slog(&format!(
                "sensor run start (pid {}) — log: {}",
                std::process::id(),
                slog_path().display()
            ));
            // Per-Monitor-V2, declared in code (no manifest dependency):
            // hook, OLE and window coordinates are physical pixels.
            let dpi_ok =
                SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) != 0;
            slog(&format!("SetProcessDpiAwarenessContext(PMv2) -> {dpi_ok}"));

            let hr = OleInitialize(null());
            if hr < 0 {
                slog(&format!(
                    "FAILED: OleInitialize (HRESULT 0x{:08X})",
                    hr as u32
                ));
                return Err(SensorError::new(
                    "ole_init",
                    format!("OleInitialize failed: HRESULT 0x{:08X}", hr as u32),
                ));
            }
            slog("OleInitialize OK");

            let mut core = Box::new(SensorCore {
                config,
                events,
                commands,
                resolve,
                gesture: GestureTracker::new(),
                overlay: null_mut(),
                drop_target: null_mut(),
                registered: false,
                summoned: false,
                drag_shown: false,
                wheel_centre: (0, 0),
                model: None,
                paths: Vec::new(),
                hover: None,
                participating: false,
                pending_handback: false,
                converted: false,
                terminal_sent: false,
                toast_hwnd: null_mut(),
                toast_until: None,
                shutdown: false,
                pump_logged: false,
                pending_kind: WheelKind::Convert,
                panel_hwnd: null_mut(),
                panel_state: None,
                panel_preview: None,
                panel_pos: (0, 0),
            });
            core.gesture.set_config(&core.config.clone());
            CORE_PTR.store(&mut *core as *mut SensorCore as usize, Ordering::SeqCst);

            let hinst = GetModuleHandleW(null());
            ensure_classes(hinst);

            let mouse_hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), hinst, 0);
            let keyboard_hook = if mouse_hook.is_null() {
                null_mut()
            } else {
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), hinst, 0)
            };
            if mouse_hook.is_null() || keyboard_hook.is_null() {
                let err = GetLastError();
                slog(&format!(
                    "FAILED: SetWindowsHookExW (mouse null={}, keyboard null={}, GetLastError {err})",
                    mouse_hook.is_null(),
                    keyboard_hook.is_null()
                ));
                if !mouse_hook.is_null() {
                    UnhookWindowsHookEx(mouse_hook);
                }
                CORE_PTR.store(0, Ordering::SeqCst);
                OleUninitialize();
                return Err(SensorError::new(
                    "hook_failed",
                    format!("SetWindowsHookExW failed (GetLastError {err})"),
                ));
            }
            slog("hooks installed (WH_MOUSE_LL + WH_KEYBOARD_LL)");

            // The queue-drain timer lives on a message-only HOST
            // window, whose proc runs `tick` on WM_TIMER. v1.0.0 used
            // a thread timer — `SetTimer(null_mut(), TIMER_ID, …)` —
            // whose requested ID the system ignores (it assigns its
            // own, returned by SetTimer and discarded by the code),
            // so the loop's `wParam == TIMER_ID` match never fired and
            // `tick` never ran: the sensor was silently dead. An hWnd
            // timer honors its ID. See the module post-mortem.
            let host = CreateWindowExW(
                0,
                windows_sys::w!("UrukConvertSensorHost"),
                null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                hinst,
                null(),
            );
            if host.is_null() {
                let err = GetLastError();
                slog(&format!(
                    "FAILED: sensor host window creation (GetLastError {err})"
                ));
                UnhookWindowsHookEx(mouse_hook);
                UnhookWindowsHookEx(keyboard_hook);
                CORE_PTR.store(0, Ordering::SeqCst);
                OleUninitialize();
                return Err(SensorError::new(
                    "host_window",
                    format!("CreateWindowExW(sensor host) failed (GetLastError {err})"),
                ));
            }
            if SetTimer(host, TIMER_ID, 8, None) == 0 {
                let err = GetLastError();
                slog(&format!("FAILED: SetTimer(host) (GetLastError {err})"));
                DestroyWindow(host);
                UnhookWindowsHookEx(mouse_hook);
                UnhookWindowsHookEx(keyboard_hook);
                CORE_PTR.store(0, Ordering::SeqCst);
                OleUninitialize();
                return Err(SensorError::new(
                    "timer_failed",
                    format!("SetTimer(sensor host) failed (GetLastError {err})"),
                ));
            }
            slog("ARMED: host timer live (8 ms) — entering the message loop");
            // Tell the shell the sensor is genuinely alive (Settings
            // shows it; v1.0.0 claimed "running" unconditionally).
            let _ = core.events.send(SensorEvent::Notice {
                code: "sensor_armed".to_string(),
                message: "windows sensor armed (hooks live)".to_string(),
            });

            let mut result: Result<(), SensorError> = Ok(());
            let mut msg: MSG = std::mem::zeroed();
            loop {
                let r = GetMessageW(&mut msg, null_mut(), 0, 0);
                if r == 0 {
                    break; // WM_QUIT
                }
                if r == -1 {
                    result = Err(SensorError::new(
                        "msg_loop",
                        format!("GetMessageW failed (GetLastError {})", GetLastError()),
                    ));
                    break;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            KillTimer(host, TIMER_ID);
            DestroyWindow(host);
            UnhookWindowsHookEx(mouse_hook);
            UnhookWindowsHookEx(keyboard_hook);
            core.dismiss_overlay();
            core.destroy_toast();
            CORE_PTR.store(0, Ordering::SeqCst);
            drop(core);
            OleUninitialize();
            match &result {
                Ok(()) => slog("sensor run end (clean)"),
                Err(e) => slog(&format!("sensor run end (error [{}] {})", e.code, e.message)),
            }
            result
        }
    }

    fn wide_z(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn set_autostart(enabled: bool) -> Result<(), SensorError> {
        let subkey = wide_z("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
        let value_name = wide_z("Uruk Convert");
        unsafe {
            let mut hkey: HKEY = null_mut();
            if enabled {
                let rc = RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    subkey.as_ptr(),
                    0,
                    null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_SET_VALUE,
                    null(),
                    &mut hkey,
                    null_mut(),
                );
                if rc != ERROR_SUCCESS {
                    return Err(SensorError::new(
                        "registry",
                        format!("RegCreateKeyExW failed (error {rc})"),
                    ));
                }
                let exe = std::env::current_exe().map_err(|e| {
                    SensorError::new("io", format!("cannot locate own executable: {e}"))
                })?;
                let value_str = format!("\"{}\" --agent", exe.to_string_lossy());
                let value: Vec<u16> = value_str.encode_utf16().chain(std::iter::once(0)).collect();
                let rc = RegSetValueExW(
                    hkey,
                    value_name.as_ptr(),
                    0,
                    REG_SZ,
                    value.as_ptr() as *const u8,
                    (value.len() * 2) as u32,
                );
                RegCloseKey(hkey);
                if rc != ERROR_SUCCESS {
                    return Err(SensorError::new(
                        "registry",
                        format!("RegSetValueExW failed (error {rc})"),
                    ));
                }
                Ok(())
            } else {
                let rc = RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    subkey.as_ptr(),
                    0,
                    KEY_SET_VALUE,
                    &mut hkey,
                );
                if rc == ERROR_FILE_NOT_FOUND {
                    return Ok(()); // nothing to remove
                }
                if rc != ERROR_SUCCESS {
                    return Err(SensorError::new(
                        "registry",
                        format!("RegOpenKeyExW failed (error {rc})"),
                    ));
                }
                let rc = RegDeleteValueW(hkey, value_name.as_ptr());
                RegCloseKey(hkey);
                if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
                    return Err(SensorError::new(
                        "registry",
                        format!("RegDeleteValueW failed (error {rc})"),
                    ));
                }
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (pure logic — these run on any OS)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- DROPFILES fixtures --

    fn dropfiles(payload: &[u8], wide: bool) -> Vec<u8> {
        let mut blob = Vec::new();
        blob.extend_from_slice(&20u32.to_le_bytes()); // pFiles
        blob.extend_from_slice(&0i32.to_le_bytes()); // pt.x
        blob.extend_from_slice(&0i32.to_le_bytes()); // pt.y
        blob.extend_from_slice(&0u32.to_le_bytes()); // fNonClient
        blob.extend_from_slice(&(wide as u32).to_le_bytes()); // fWide
        blob.extend_from_slice(payload);
        blob
    }

    fn utf16_list(paths: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in paths {
            for u in p.encode_utf16() {
                out.extend_from_slice(&u.to_le_bytes());
            }
            out.extend_from_slice(&0u16.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn hdrop_wide_two_files() {
        let blob = dropfiles(&utf16_list(&["C:\\a.png", "D:\\b c.jpg"]), true);
        assert_eq!(
            parse_hdrop(&blob, true),
            vec!["C:\\a.png".to_string(), "D:\\b c.jpg".to_string()]
        );
        assert_eq!(parse_hdrop_auto(&blob), parse_hdrop(&blob, true));
    }

    #[test]
    fn hdrop_ansi_two_files() {
        let blob = dropfiles(b"C:\\x.gif\0E:\\y.bmp\0\0", false);
        assert_eq!(
            parse_hdrop(&blob, false),
            vec!["C:\\x.gif".to_string(), "E:\\y.bmp".to_string()]
        );
        assert_eq!(parse_hdrop_auto(&blob), parse_hdrop(&blob, false));
    }

    #[test]
    fn hdrop_stops_at_double_null() {
        let mut payload = utf16_list(&["C:\\only.png"]);
        payload.extend_from_slice(&utf16_list(&["C:\\after.png"]));
        let blob = dropfiles(&payload, true);
        assert_eq!(parse_hdrop(&blob, true), vec!["C:\\only.png".to_string()]);
    }

    #[test]
    fn hdrop_malformed_is_empty_not_panic() {
        assert!(parse_hdrop(&[], true).is_empty());
        assert!(parse_hdrop(&[1, 2], false).is_empty());
        // pFiles points past the end.
        let mut blob = dropfiles(b"\0", false);
        blob[0..4].copy_from_slice(&9999u32.to_le_bytes());
        assert!(parse_hdrop(&blob, false).is_empty());
    }

    // -- gesture state machine --
    /// A Shift-press chord record (the default summon chord).
    const SHIFT_DOWN: HookRecord = HookRecord::Chord(HeldMods {
        shift: true,
        control: false,
        alt: false,
    });


    fn press(t: &mut GestureTracker) {
        assert_eq!(
            t.on_record(HookRecord::LeftButtonDown { x: 100, y: 100 }),
            None
        );
    }

    #[test]
    fn press_move_modifier_summons() {
        let mut t = GestureTracker::new();
        press(&mut t);
        assert_eq!(
            t.on_record(HookRecord::MouseMove { x: 130, y: 100 }),
            Some(GestureOutput::Candidate { x: 130, y: 100 })
        );
        assert_eq!(
            t.on_record(SHIFT_DOWN),
            Some(GestureOutput::Summon { x: 130, y: 100, kind: WheelKind::Convert })
        );
        assert!(t.is_summoned());
    }

    #[test]
    fn small_move_is_not_a_candidate() {
        let mut t = GestureTracker::new();
        press(&mut t);
        assert_eq!(t.on_record(HookRecord::MouseMove { x: 105, y: 103 }), None);
        assert_eq!(t.on_record(SHIFT_DOWN), None);
        assert!(!t.is_summoned());
    }

    #[test]
    fn threshold_boundary_is_strictly_greater() {
        let mut t = GestureTracker::new();
        press(&mut t);
        // Exactly 8 px away: not yet a candidate.
        assert_eq!(t.on_record(HookRecord::MouseMove { x: 108, y: 100 }), None);
        // 9 px: candidate.
        assert_eq!(
            t.on_record(HookRecord::MouseMove { x: 109, y: 100 }),
            Some(GestureOutput::Candidate { x: 109, y: 100 })
        );
    }

    #[test]
    fn modifier_held_before_threshold_summons_at_candidate() {
        // v1.0.1 held-modifier fallback (macOS parity): a chord pressed
        // while the drag is still under the threshold summons the
        // moment the drag confirms — no fresh edge will arrive for it.
        let mut t = GestureTracker::new();
        press(&mut t);
        assert_eq!(t.on_record(SHIFT_DOWN), None);
        assert_eq!(
            t.on_record(HookRecord::MouseMove { x: 150, y: 100 }),
            Some(GestureOutput::Summon {
                x: 150,
                y: 100,
                kind: WheelKind::Convert
            })
        );
        assert!(t.is_summoned());
    }

    #[test]
    fn modifier_held_before_press_summons_at_candidate() {
        // Same fallback, harder case: the chord went down before the
        // mouse press (the record arrives in Idle and must be kept).
        let mut t = GestureTracker::new();
        assert_eq!(t.on_record(SHIFT_DOWN), None);
        press(&mut t);
        assert_eq!(
            t.on_record(HookRecord::MouseMove { x: 150, y: 100 }),
            Some(GestureOutput::Summon {
                x: 150,
                y: 100,
                kind: WheelKind::Convert
            })
        );
        assert!(t.is_summoned());
    }

    #[test]
    fn fresh_tap_after_candidate_still_summons() {
        // The classic gesture is unchanged: candidate first, then the
        // mid-drag modifier tap summons on its edge.
        let mut t = GestureTracker::new();
        press(&mut t);
        assert_eq!(
            t.on_record(HookRecord::MouseMove { x: 150, y: 100 }),
            Some(GestureOutput::Candidate { x: 150, y: 100 })
        );
        assert_eq!(
            t.on_record(SHIFT_DOWN),
            Some(GestureOutput::Summon {
                x: 150,
                y: 100,
                kind: WheelKind::Convert
            })
        );
    }

    #[test]
    fn escape_suppresses_summon_for_rest_of_drag() {
        let mut t = GestureTracker::new();
        press(&mut t);
        t.on_record(HookRecord::MouseMove { x: 140, y: 100 });
        assert_eq!(
            t.on_record(SHIFT_DOWN),
            Some(GestureOutput::Summon { x: 140, y: 100, kind: WheelKind::Convert })
        );
        assert_eq!(
            t.on_record(HookRecord::EscapeDown),
            Some(GestureOutput::EscapeCancel)
        );
        // Modifier again: suppressed. Button-up: no second terminal event.
        assert_eq!(t.on_record(SHIFT_DOWN), None);
        assert_eq!(
            t.on_record(HookRecord::LeftButtonUp { x: 140, y: 100 }),
            None
        );
        assert!(!t.is_active());
        // The next drag starts clean and can summon again.
        press(&mut t);
        t.on_record(HookRecord::MouseMove { x: 140, y: 100 });
        assert_eq!(
            t.on_record(SHIFT_DOWN),
            Some(GestureOutput::Summon { x: 140, y: 100, kind: WheelKind::Convert })
        );
    }

    #[test]
    fn release_without_summon_ends_drag() {
        let mut t = GestureTracker::new();
        press(&mut t);
        t.on_record(HookRecord::MouseMove { x: 140, y: 100 });
        assert_eq!(
            t.on_record(HookRecord::LeftButtonUp { x: 140, y: 100 }),
            Some(GestureOutput::Ended)
        );
        assert!(!t.is_active());
        // A stray button-up in Idle is ignored.
        assert_eq!(t.on_record(HookRecord::LeftButtonUp { x: 0, y: 0 }), None);
    }
    #[test]
    fn chord_change_under_live_wheel_resummons() {
        let mut t = GestureTracker::new();
        press(&mut t);
        t.on_record(HookRecord::MouseMove { x: 140, y: 100 });
        assert_eq!(
            t.on_record(SHIFT_DOWN),
            Some(GestureOutput::Summon {
                x: 140,
                y: 100,
                kind: WheelKind::Convert
            })
        );
        // Adding Alt upgrades to the tools wheel.
        assert_eq!(
            t.on_record(HookRecord::Chord(HeldMods {
                shift: true,
                control: false,
                alt: true
            })),
            Some(GestureOutput::Resummon {
                x: 140,
                y: 100,
                kind: WheelKind::Tools
            })
        );
        // Control alone (Shift released first in the same set change
        // would pass through no-chord; here it arrives as a set with
        // only Control) selects the actions wheel.
        assert_eq!(
            t.on_record(HookRecord::Chord(HeldMods {
                shift: false,
                control: true,
                alt: false
            })),
            Some(GestureOutput::Resummon {
                x: 140,
                y: 100,
                kind: WheelKind::Actions
            })
        );
    }
}
