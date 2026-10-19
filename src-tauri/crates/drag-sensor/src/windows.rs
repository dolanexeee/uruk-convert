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

use std::sync::mpsc::{Receiver, Sender};

use crate::{DragSensor, SensorCommand, SensorConfig, SensorError, SensorEvent, WheelResolver};

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
    /// Edge (not auto-repeat) of the configured summon modifier.
    ModifierDown,
    /// Edge of Escape.
    EscapeDown,
}

/// What the tracker wants the sensor core to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GestureOutput {
    /// Press + threshold crossed: a drag candidate exists at `(x, y)`.
    Candidate { x: i32, y: i32 },
    /// Modifier tapped while a candidate is live: summon at `(x, y)`.
    Summon { x: i32, y: i32 },
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
}

impl Default for GestureTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl GestureTracker {
    pub fn new() -> Self {
        GestureTracker {
            state: GestureState::Idle,
            origin: (0, 0),
            last: (0, 0),
        }
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
                        return Some(GestureOutput::Candidate { x, y });
                    }
                }
                None
            }
            HookRecord::ModifierDown => match self.state {
                GestureState::Candidate => {
                    self.state = GestureState::Summoned;
                    Some(GestureOutput::Summon {
                        x: self.last.0,
                        y: self.last.1,
                    })
                }
                _ => None,
            },
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
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows_sys::Win32::System::Ole::{
        OleInitialize, OleUninitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
        CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_NONE,
    };
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    use windows_sys::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        VK_CONTROL, VK_ESCAPE, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_MENU, VK_RCONTROL, VK_RMENU,
        VK_RSHIFT, VK_SHIFT,
    };
    use windows_sys::Win32::UI::Shell::DragQueryFileW;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    use crate::{
        SensorCommand, SensorConfig, SensorError, SensorEvent, SummonModifier, Toast, WheelModel,
        WheelRequest, WheelResolver,
    };

    use super::{parse_hdrop_auto, GestureOutput, GestureTracker, HookRecord};

    const TIMER_ID: usize = 0x5543; // "UC" — the queue-drain timer.
    const WM_URUK_HANDBACK: u32 = WM_APP + 0x31;
    const WM_URUK_CLEANUP: u32 = WM_APP + 0x32;
    const WHEEL_PX: i32 = crate::geom::WHEEL_SIZE as i32;

    // -- Hook-side shared state -------------------------------------------
    // The hook callbacks may only touch these statics: a queue, and a few
    // atomics. Everything else happens on the sensor thread.

    static HOOK_QUEUE: Mutex<VecDeque<HookRecord>> = Mutex::new(VecDeque::new());
    /// Configured summon modifier: 0 = Shift, 1 = Control, 2 = Alt.
    static MODIFIER_KIND: AtomicU8 = AtomicU8::new(0);
    static MODIFIER_HELD: AtomicBool = AtomicBool::new(false);
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

    fn modifier_kind(m: SummonModifier) -> u8 {
        match m {
            SummonModifier::Shift => 0,
            SummonModifier::Control => 1,
            SummonModifier::Alt => 2,
        }
    }

    fn vk_matches_modifier(kind: u8, vk: u32) -> bool {
        match kind {
            0 => vk == VK_SHIFT as u32 || vk == VK_LSHIFT as u32 || vk == VK_RSHIFT as u32,
            1 => vk == VK_CONTROL as u32 || vk == VK_LCONTROL as u32 || vk == VK_RCONTROL as u32,
            _ => vk == VK_MENU as u32 || vk == VK_LMENU as u32 || vk == VK_RMENU as u32,
        }
    }

    fn vk_is_any_modifier(vk: u32) -> bool {
        vk_matches_modifier(0, vk) || vk_matches_modifier(1, vk) || vk_matches_modifier(2, vk)
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
                    } else if vk_matches_modifier(MODIFIER_KIND.load(Ordering::SeqCst), vk)
                        && !MODIFIER_HELD.swap(true, Ordering::SeqCst)
                    {
                        push_record(HookRecord::ModifierDown);
                    }
                }
                WM_KEYUP | WM_SYSKEYUP => {
                    if vk == VK_ESCAPE as u32 {
                        ESC_HELD.store(false, Ordering::SeqCst);
                    } else if vk_is_any_modifier(vk) {
                        MODIFIER_HELD.store(false, Ordering::SeqCst);
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

    unsafe fn ensure_classes(hinst: windows_sys::Win32::Foundation::HINSTANCE) {
        if CLASSES_DONE.swap(true, Ordering::SeqCst) {
            return;
        }
        for class_name in [
            windows_sys::w!("UrukConvertWheelOverlay"),
            windows_sys::w!("UrukConvertToast"),
        ] {
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(overlay_wndproc),
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
            // 0 with ERROR_CLASS_ALREADY_EXISTS is fine (idempotent).
            let _ = RegisterClassExW(&wc);
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
    }

    fn generic_model() -> WheelModel {
        // Hub-only wheel painted between summon and DragEnter: we know
        // a drag exists, not yet what it carries.
        WheelModel {
            segments: Vec::new(),
            hub_ext: "FILE".to_string(),
            hub_title: String::new(),
            file_count: 0,
        }
    }

    impl SensorCore {
        // -- command / queue pump (timer tick) --

        unsafe fn tick(&mut self) {
            loop {
                match self.commands.try_recv() {
                    Ok(SensorCommand::Shutdown) => {
                        self.shutdown = true;
                    }
                    Ok(SensorCommand::UpdateConfig(cfg)) => {
                        MODIFIER_KIND.store(modifier_kind(cfg.modifier), Ordering::SeqCst);
                        self.config = cfg;
                    }
                    Ok(SensorCommand::ShowToast(toast)) => self.show_toast(toast),
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
                GestureOutput::Candidate { .. } => {}
                GestureOutput::Summon { x, y } => {
                    if self.config.enabled && !self.summoned {
                        self.show_overlay(x, y);
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
            let _ = self.events.send(SensorEvent::Summoned { x: cx, y: cy });
        }

        /// Hide the overlay and revoke the drop registration. Idempotent.
        unsafe fn dismiss_overlay(&mut self) {
            let hwnd = self.overlay;
            if !hwnd.is_null() {
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
            self.participating = false;
            self.pending_handback = true;
            self.post_overlay(WM_URUK_HANDBACK);
        }

        // -- IDropTarget callbacks (invoked by OLE on this thread) --

        unsafe fn on_drag_enter(&mut self, data: *mut c_void, _pt: POINTL, effect: *mut u32) {
            // UIPI note: drags from elevated processes never reach this
            // callback at all — there is nothing here to detect or log.
            if !data_has_hdrop(data) {
                self.request_handback();
                return;
            }
            let paths = paths_via_parse(data);
            if paths.is_empty() {
                self.request_handback();
                return;
            }
            let request = WheelRequest {
                paths: paths.clone(),
                mime_types: vec!["CF_HDROP".to_string()],
            };
            match (self.resolve)(&request) {
                Some(model) => {
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
                None => self.request_handback(),
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
                            let target = model.segments[i].clone();
                            let _ = self
                                .events
                                .send(SensorEvent::ConvertRequested { paths, target });
                            self.converted = true;
                            converted_now = true;
                            *effect = DROPEFFECT_COPY;
                        }
                    }
                }
            }
            if !converted_now {
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
    }

    // -- Entry points --------------------------------------------------------

    pub fn run(
        config: SensorConfig,
        events: std::sync::mpsc::Sender<SensorEvent>,
        commands: std::sync::mpsc::Receiver<SensorCommand>,
        resolve: WheelResolver,
    ) -> Result<(), SensorError> {
        unsafe {
            // Per-Monitor-V2, declared in code (no manifest dependency):
            // hook, OLE and window coordinates are physical pixels.
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

            let hr = OleInitialize(null());
            if hr < 0 {
                return Err(SensorError::new(
                    "ole_init",
                    format!("OleInitialize failed: HRESULT 0x{:08X}", hr as u32),
                ));
            }

            MODIFIER_KIND.store(modifier_kind(config.modifier), Ordering::SeqCst);

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
            });
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

            // 8 ms thread timer: drains the hook queue + the command
            // channel between messages, keeping hook-to-gesture latency
            // in single-digit milliseconds without a busy poll.
            SetTimer(null_mut(), TIMER_ID, 8, None);

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
                if msg.message == WM_TIMER && msg.wParam == TIMER_ID {
                    core.tick();
                    if core.shutdown {
                        break;
                    }
                } else {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            KillTimer(null_mut(), TIMER_ID);
            UnhookWindowsHookEx(mouse_hook);
            UnhookWindowsHookEx(keyboard_hook);
            core.dismiss_overlay();
            core.destroy_toast();
            CORE_PTR.store(0, Ordering::SeqCst);
            drop(core);
            OleUninitialize();
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
            t.on_record(HookRecord::ModifierDown),
            Some(GestureOutput::Summon { x: 130, y: 100 })
        );
        assert!(t.is_summoned());
    }

    #[test]
    fn small_move_is_not_a_candidate() {
        let mut t = GestureTracker::new();
        press(&mut t);
        assert_eq!(t.on_record(HookRecord::MouseMove { x: 105, y: 103 }), None);
        assert_eq!(t.on_record(HookRecord::ModifierDown), None);
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
    fn modifier_before_threshold_does_not_summon() {
        let mut t = GestureTracker::new();
        press(&mut t);
        assert_eq!(t.on_record(HookRecord::ModifierDown), None);
        assert_eq!(
            t.on_record(HookRecord::MouseMove { x: 150, y: 100 }),
            Some(GestureOutput::Candidate { x: 150, y: 100 })
        );
        // A fresh tap after the candidate exists does summon.
        assert_eq!(
            t.on_record(HookRecord::ModifierDown),
            Some(GestureOutput::Summon { x: 150, y: 100 })
        );
    }

    #[test]
    fn escape_suppresses_summon_for_rest_of_drag() {
        let mut t = GestureTracker::new();
        press(&mut t);
        t.on_record(HookRecord::MouseMove { x: 140, y: 100 });
        assert_eq!(
            t.on_record(HookRecord::ModifierDown),
            Some(GestureOutput::Summon { x: 140, y: 100 })
        );
        assert_eq!(
            t.on_record(HookRecord::EscapeDown),
            Some(GestureOutput::EscapeCancel)
        );
        // Modifier again: suppressed. Button-up: no second terminal event.
        assert_eq!(t.on_record(HookRecord::ModifierDown), None);
        assert_eq!(
            t.on_record(HookRecord::LeftButtonUp { x: 140, y: 100 }),
            None
        );
        assert!(!t.is_active());
        // The next drag starts clean and can summon again.
        press(&mut t);
        t.on_record(HookRecord::MouseMove { x: 140, y: 100 });
        assert_eq!(
            t.on_record(HookRecord::ModifierDown),
            Some(GestureOutput::Summon { x: 140, y: 100 })
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
}
