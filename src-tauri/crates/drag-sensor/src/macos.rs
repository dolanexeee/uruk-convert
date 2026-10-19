//! macOS sensor — full Stage-3 implementation.
//!
//! Follows the macOS column of the Stage-3 DESIGN.md (spikes project):
//!
//! ```text
//! detect → classify → summon → participate → convert
//! ```
//!
//! * **detect** — a *listen-only* `CGEventTap` (session tap, head insert)
//!   for leftMouseDown / leftMouseDragged / leftMouseUp / flagsChanged /
//!   keyDown, created on the thread `start()` is called on, driven by
//!   that thread's `CFRunLoop`. Callbacks are near-empty: they record raw
//!   state under a mutex and return. `kCGEventTapDisabledByTimeout`
//!   (and …ByUserInput) re-enable the tap, per Apple's contract.
//! * **classify** — during a button-down drag past a small movement
//!   threshold, the system drag pasteboard (`NSPasteboard` named
//!   `.drag`) has its *type list* inspected — no payload is pulled to
//!   classify. File types present → emit `DragClassified`, once per drag.
//! * **summon** — on the configured modifier's rising edge mid-drag
//!   (or while it is held when the drag confirms), the file paths are
//!   read from the drag pasteboard (`readObjects(forClasses: [NSURL])`,
//!   with the legacy `NSFilenamesPboardType` as fallback), the host's
//!   [`WheelResolver`] decides the wheel, and a pre-created
//!   non-activating `NSPanel` (level `.popUpMenu`, joins all Spaces,
//!   stationary, transparent, no shadow) is ordered front **at the
//!   cursor** showing the shared tiny-skia wheel pixmap (blitted as an
//!   `NSImage` built from a `CGImage` over the shared BGRA bytes, so
//!   the wheel is pixel-identical across platforms).
//! * **participate** — the panel's content view is a small Objective-C
//!   class defined at runtime (`objc_allocateClassPair` on `NSView`)
//!   implementing `NSDraggingDestination`; the panel is registered for
//!   `.fileURL` drags. `draggingUpdated` tracks the hover segment via
//!   [`crate::geom::segment_at`] and re-renders on change; Esc
//!   (keyDown, keycode 53) dismisses and suppresses re-summon for the
//!   rest of that drag.
//! * **convert** — `performDragOperation` reads the paths from the
//!   *dragging* pasteboard (authoritative) and, with the segment under
//!   the pointer, the sensor emits `ConvertRequested`. Mouse-up with no
//!   drop on the wheel emits `DragEnded` (after a short grace period,
//!   because the tap sees the physical release before AppKit delivers
//!   `performDragOperation` to us — the same ordering the X11 design
//!   documents).
//!
//! Permissions: event taps need Accessibility trust. At `start()`, if
//! `AXIsProcessTrusted()` is false the sensor emits one
//! `Notice { code: "permission_missing" }`, calls
//! `AXIsProcessTrustedWithOptions` with the prompt option once, and a
//! run-loop poll (every ~2 s) creates the tap as soon as trust appears.
//! It never blocks waiting for permission: commands, toasts and
//! shutdown all keep working in the degraded mode (sensing simply
//! yields nothing).
//!
//! Everything AppKit lives on the `start()` caller's thread and its
//! `CFRunLoop`; AppKit therefore wants `start()` called on the process
//! main thread — if it is not, a `Notice { code:
//! "main_thread_required" }` is emitted and the sensor carries on
//! (the tap works from any run-loop thread; panels may not). Only raw
//! FFI + `libc` are used — no Objective-C crates.
//!
//! The pure helpers in this file (pasteboard-type classification,
//! modifier-flag mapping, LaunchAgent plist generation, file-URL and
//! filename-plist parsing) compile and are unit-tested on every OS;
//! `start()` outside macOS returns an `unsupported` error.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use crate::{
    DragSensor, SensorCommand, SensorConfig, SensorError, SensorEvent, SummonModifier,
    WheelResolver,
};

// ---------------------------------------------------------------------------
// Public sensor shell
// ---------------------------------------------------------------------------

pub struct MacSensor;

impl MacSensor {
    pub fn new() -> Self {
        MacSensor
    }
}

impl Default for MacSensor {
    fn default() -> Self {
        Self::new()
    }
}

impl DragSensor for MacSensor {
    fn platform(&self) -> &'static str {
        "macos"
    }

    fn start(
        &mut self,
        config: SensorConfig,
        events: Sender<SensorEvent>,
        commands: Receiver<SensorCommand>,
        resolve: WheelResolver,
    ) -> Result<(), SensorError> {
        #[cfg(target_os = "macos")]
        {
            imp::run(config, events, commands, resolve)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (config, events, commands, resolve);
            Err(SensorError::new(
                "unsupported",
                "the macOS drag sensor only runs on macOS",
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Autostart (LaunchAgent) — pure path/plist logic, real file IO.
// Only invoked on macOS (see `crate::set_autostart` dispatch), but the
// implementation is platform-neutral std::fs and is exercised by the
// unit tests on Linux.
// ---------------------------------------------------------------------------

pub const LAUNCH_AGENT_LABEL: &str = "org.uruk.convert.agent";

/// `~/Library/LaunchAgents/org.uruk.convert.agent.plist`
pub fn launch_agent_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCH_AGENT_LABEL}.plist"))
}

/// Escape a string for inclusion in an XML plist `<string>` element.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// The LaunchAgent plist body: run `exe --agent` at login.
pub fn launch_agent_plist(exe: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{exe}</string>\n\
         \t\t<string>--agent</string>\n\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         </dict>\n\
         </plist>\n",
        label = LAUNCH_AGENT_LABEL,
        exe = xml_escape(&exe.to_string_lossy()),
    )
}

pub fn set_autostart(enabled: bool) -> Result<(), SensorError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| SensorError::new("io", "HOME is not set"))?;
    let file = launch_agent_path(&home);
    if !enabled {
        if file.exists() {
            std::fs::remove_file(&file).map_err(|e| {
                SensorError::new("io", format!("cannot remove LaunchAgent plist: {e}"))
            })?;
        }
        return Ok(());
    }
    let exe = std::env::current_exe()
        .map_err(|e| SensorError::new("io", format!("cannot locate own executable: {e}")))?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| SensorError::new("io", format!("cannot create LaunchAgents dir: {e}")))?;
    }
    std::fs::write(&file, launch_agent_plist(&exe))
        .map_err(|e| SensorError::new("io", format!("cannot write LaunchAgent plist: {e}")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Pure classification / input helpers (unit-tested on every OS)
// ---------------------------------------------------------------------------

/// Pasteboard type string for the legacy filenames type. (The constant's
/// value is its own name.)
pub const PB_TYPE_FILENAMES: &str = "NSFilenamesPboardType";
/// Pasteboard type string for a single file URL (`NSPasteboardTypeFileURL`).
pub const PB_TYPE_FILE_URL: &str = "public.file-url";
/// Pasteboard type string for a property-list array of file URLs.
pub const PB_TYPE_FILE_URL_ALT: &str = "public.file-urls";

/// Is a drag carrying files, judged from its pasteboard *type list*
/// alone? Type inspection is the classification — no payload is read.
pub fn is_file_drag_types(types: &[String]) -> bool {
    types
        .iter()
        .any(|t| t == PB_TYPE_FILENAMES || t == PB_TYPE_FILE_URL || t == PB_TYPE_FILE_URL_ALT)
}

/// `CGEventFlags` bits for the configurable summon modifiers
/// (`kCGEventFlagMaskShift` / `…Control` / `…Alternate`).
pub fn modifier_mask(m: SummonModifier) -> u64 {
    match m {
        SummonModifier::Shift => 1 << 17,
        SummonModifier::Control => 1 << 18,
        SummonModifier::Alt => 1 << 19,
    }
}

/// Is `m` currently down in a `CGEventFlags` word? The generic mask
/// bits are set by either the left or the right physical key.
pub fn modifier_is_down(flags: u64, m: SummonModifier) -> bool {
    flags & modifier_mask(m) != 0
}

/// Virtual keycode of Escape (`kVK_Escape`).
pub const KEYCODE_ESCAPE: i64 = 53;

pub fn is_escape_keycode(code: i64) -> bool {
    code == KEYCODE_ESCAPE
}

/// Movement (in points) a button-down pointer must travel before the
/// gesture counts as a drag candidate rather than a click.
pub const DRAG_THRESHOLD_PX: f64 = 4.0;

pub fn beyond_threshold(down: (f64, f64), cur: (f64, f64)) -> bool {
    let dx = cur.0 - down.0;
    let dy = cur.1 - down.1;
    (dx * dx + dy * dy).sqrt() > DRAG_THRESHOLD_PX
}

/// Percent-decode into raw bytes (`%XX`); malformed sequences pass
/// through literally.
pub fn percent_decode(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Turn a `file://` URL (as read from an `NSURL`'s absolute string or a
/// pasteboard URL list) into a local path. Non-local hosts and non-file
/// schemes yield `None`.
pub fn path_from_file_url(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    // `rest` is "host/path" or "/path".
    let path_part = if let Some(stripped) = rest.strip_prefix('/') {
        // "file:///path" — empty host.
        let _ = stripped;
        rest
    } else {
        let slash = rest.find('/')?;
        let host = &rest[..slash];
        if host != "localhost" {
            return None;
        }
        &rest[slash..]
    };
    let decoded = percent_decode(path_part);
    Some(PathBuf::from(
        String::from_utf8_lossy(&decoded).into_owned(),
    ))
}

fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let mut consumed = 0;
        for (entity, ch) in [
            ("&amp;", '&'),
            ("&lt;", '<'),
            ("&gt;", '>'),
            ("&quot;", '"'),
            ("&apos;", '\''),
        ] {
            if tail.starts_with(entity) {
                out.push(ch);
                consumed = entity.len();
                break;
            }
        }
        if consumed == 0 {
            out.push('&');
            consumed = 1;
        }
        rest = &tail[consumed..];
    }
    out.push_str(rest);
    out
}

/// Parse the payload of an `NSFilenamesPboardType` pasteboard entry:
/// an array of path strings, serialized either as an XML plist
/// (`<string>…</string>` elements) or in the legacy OpenStep format
/// (`("a", "b")`). Used as a fallback when `readObjects(NSURL)` and
/// the property-list route both come up empty.
pub fn parse_filenames_plist(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.starts_with("<?xml") || trimmed.starts_with("<plist") {
        let mut out = Vec::new();
        let mut rest = trimmed;
        while let Some(start) = rest.find("<string>") {
            let after = &rest[start + "<string>".len()..];
            match after.find("</string>") {
                Some(end) => {
                    out.push(xml_unescape(&after[..end]));
                    rest = &after[end + "</string>".len()..];
                }
                None => break,
            }
        }
        return out;
    }
    // OpenStep array: '(' elem (',' elem)* ')' where elem is a quoted
    // string (backslash escapes) or a bare token.
    let mut out = Vec::new();
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    if i < bytes.len() && bytes[i] == b'(' {
        i += 1;
    }
    while i < bytes.len() {
        // Skip separators and whitespace.
        while i < bytes.len() && (bytes[i] == b',' || (bytes[i] as char).is_whitespace()) {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] == b')' {
            break;
        }
        if bytes[i] == b'"' {
            i += 1;
            let mut s = String::new();
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 1;
                    match bytes[i] {
                        b'n' => s.push('\n'),
                        b't' => s.push('\t'),
                        b'r' => s.push('\r'),
                        other => s.push(other as char),
                    }
                    i += 1;
                } else {
                    // Consume one UTF-8 char.
                    let ch_len = utf8_len(bytes[i]);
                    let end = (i + ch_len).min(bytes.len());
                    if let Ok(part) = std::str::from_utf8(&bytes[i..end]) {
                        s.push_str(part);
                    }
                    i = end;
                }
            }
            i += 1; // closing quote
            out.push(s);
        } else {
            let start = i;
            while i < bytes.len() && bytes[i] != b',' && bytes[i] != b')' {
                i += 1;
            }
            let token = trimmed[start..i].trim();
            if !token.is_empty() {
                out.push(token.to_string());
            }
        }
    }
    out
}

fn utf8_len(lead: u8) -> usize {
    if lead < 0x80 {
        1
    } else if lead >> 5 == 0b110 {
        2
    } else if lead >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

// ---------------------------------------------------------------------------
// macOS implementation (raw FFI: CoreGraphics / CoreFoundation / AppKit
// via libobjc — no Objective-C crates).
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use super::{
        beyond_threshold, is_escape_keycode, is_file_drag_types, modifier_is_down,
        parse_filenames_plist, PB_TYPE_FILENAMES, PB_TYPE_FILE_URL,
    };
    use crate::geom;
    use crate::render::{pixmap_to_bgra, render_toast, render_wheel, TOAST_H};
    use crate::{
        SensorCommand, SensorConfig, SensorError, SensorEvent, SensorLang, Toast, ToastKind,
        WheelModel, WheelRequest, WheelResolver,
    };

    // -----------------------------------------------------------------
    // CoreFoundation / CoreGraphics / ApplicationServices FFI
    // -----------------------------------------------------------------

    type Id = *mut c_void;
    type Sel = *mut c_void;
    type Class = *mut c_void;
    type CFTypeRef = *const c_void;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    type NSPoint = CGPoint;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CGSize {
        width: f64,
        height: f64,
    }
    type NSSize = CGSize;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }
    type NSRect = CGRect;

    type TapCallback =
        unsafe extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void;
    type TimerCallback = unsafe extern "C" fn(*mut c_void, *mut c_void);

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRunLoopGetCurrent() -> *mut c_void;
        fn CFRunLoopRun();
        fn CFRunLoopStop(rl: *mut c_void);
        fn CFRunLoopAddSource(rl: *mut c_void, source: *mut c_void, mode: CFTypeRef);
        fn CFRunLoopRemoveSource(rl: *mut c_void, source: *mut c_void, mode: CFTypeRef);
        fn CFRunLoopAddTimer(rl: *mut c_void, timer: *mut c_void, mode: CFTypeRef);
        fn CFRunLoopTimerCreate(
            alloc: CFTypeRef,
            fire_date: f64,
            interval: f64,
            flags: usize,
            order: isize,
            callback: Option<TimerCallback>,
            info: *mut c_void,
        ) -> *mut c_void;
        fn CFRunLoopTimerInvalidate(timer: *mut c_void);
        fn CFMachPortCreateRunLoopSource(
            alloc: CFTypeRef,
            port: *mut c_void,
            order: isize,
        ) -> *mut c_void;
        fn CFMachPortInvalidate(port: *mut c_void);
        fn CFAbsoluteTimeGetCurrent() -> f64;
        fn CFRelease(cf: CFTypeRef);
        static kCFRunLoopCommonModes: CFTypeRef;
        static kCFBooleanTrue: CFTypeRef;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventTapCreate(
            tap: u32,
            place: u32,
            options: u32,
            mask: u64,
            callback: Option<TapCallback>,
            user_info: *mut c_void,
        ) -> *mut c_void;
        fn CGEventTapEnable(tap: *mut c_void, enable: bool);
        fn CGEventGetFlags(event: *mut c_void) -> u64;
        fn CGEventGetLocation(event: *mut c_void) -> CGPoint;
        fn CGEventGetIntegerValueField(event: *mut c_void, field: u32) -> i64;
        fn CGMainDisplayID() -> u32;
        fn CGDisplayBounds(display: u32) -> CGRect;
        fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
        fn CGColorSpaceRelease(cs: *mut c_void);
        fn CGDataProviderCreateWithData(
            info: *mut c_void,
            data: *const u8,
            size: usize,
            release: Option<unsafe extern "C" fn(*mut c_void, *const u8, usize)>,
        ) -> *mut c_void;
        fn CGDataProviderRelease(p: *mut c_void);
        fn CGImageCreate(
            width: usize,
            height: usize,
            bits_per_component: usize,
            bits_per_pixel: usize,
            bytes_per_row: usize,
            space: *mut c_void,
            bitmap_info: u32,
            provider: *mut c_void,
            decode: *const f64,
            should_interpolate: bool,
            intent: i32,
        ) -> *mut c_void;
        fn CGImageRelease(img: *mut c_void);
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> bool;
        static kAXTrustedCheckOptionPrompt: CFTypeRef;
    }

    extern "C" {
        fn pthread_main_np() -> i32;
    }

    // CGEventType values.
    const EV_LEFT_MOUSE_DOWN: u32 = 1;
    const EV_LEFT_MOUSE_UP: u32 = 2;
    const EV_LEFT_MOUSE_DRAGGED: u32 = 6;
    const EV_KEY_DOWN: u32 = 10;
    const EV_FLAGS_CHANGED: u32 = 12;
    const EV_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
    const EV_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
    /// kCGKeyboardEventKeycode.
    const FIELD_KEYCODE: u32 = 9;

    /// kCGImageAlphaPremultipliedFirst (2) | kCGBitmapByteOrder32Little (2<<12).
    const BITMAP_INFO_BGRA_PREMUL: u32 = 2 | (2 << 12);

    // NSDragOperation values.
    const DRAG_OP_NONE: usize = 0;
    const DRAG_OP_COPY: usize = 1;

    // -----------------------------------------------------------------
    // libobjc: one msgSend declaration, transmuted per signature.
    // -----------------------------------------------------------------

    #[link(name = "objc", kind = "dylib")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> Class;
        fn objc_allocateClassPair(superclass: Class, name: *const c_char, extra: usize) -> Class;
        fn objc_registerClassPair(cls: Class);
        fn sel_registerName(name: *const c_char) -> Sel;
        fn class_addMethod(cls: Class, sel: Sel, imp: *const c_void, types: *const c_char) -> bool;
        fn objc_msgSend();
    }

    fn msgsend_addr() -> usize {
        let f: unsafe extern "C" fn() = objc_msgSend;
        f as usize
    }

    unsafe fn get_class(name: &str) -> Id {
        let c = CString::new(name).expect("class name");
        objc_getClass(c.as_ptr()) as Id
    }

    unsafe fn sel(name: &str) -> Sel {
        let c = CString::new(name).expect("selector name");
        sel_registerName(c.as_ptr())
    }

    unsafe fn send0(recv: Id, sel: Sel) -> Id {
        let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(msgsend_addr());
        f(recv, sel)
    }
    unsafe fn send0_void(recv: Id, sel: Sel) {
        let f: unsafe extern "C" fn(Id, Sel) = std::mem::transmute(msgsend_addr());
        f(recv, sel)
    }
    unsafe fn send0_usize(recv: Id, sel: Sel) -> usize {
        let f: unsafe extern "C" fn(Id, Sel) -> usize = std::mem::transmute(msgsend_addr());
        f(recv, sel)
    }
    unsafe fn send0_bool(recv: Id, sel: Sel) -> bool {
        let f: unsafe extern "C" fn(Id, Sel) -> bool = std::mem::transmute(msgsend_addr());
        f(recv, sel)
    }
    unsafe fn send0_cstr(recv: Id, sel: Sel) -> *const c_char {
        let f: unsafe extern "C" fn(Id, Sel) -> *const c_char = std::mem::transmute(msgsend_addr());
        f(recv, sel)
    }
    unsafe fn send0_ptr(recv: Id, sel: Sel) -> *const u8 {
        let f: unsafe extern "C" fn(Id, Sel) -> *const u8 = std::mem::transmute(msgsend_addr());
        f(recv, sel)
    }
    unsafe fn send0_point(recv: Id, sel: Sel) -> NSPoint {
        let f: unsafe extern "C" fn(Id, Sel) -> NSPoint = std::mem::transmute(msgsend_addr());
        f(recv, sel)
    }
    unsafe fn send_id(recv: Id, sel: Sel, a: Id) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, Id) -> Id = std::mem::transmute(msgsend_addr());
        f(recv, sel, a)
    }
    unsafe fn send_id_void(recv: Id, sel: Sel, a: Id) {
        let f: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute(msgsend_addr());
        f(recv, sel, a)
    }
    unsafe fn send_id_id(recv: Id, sel: Sel, a: Id, b: Id) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, Id, Id) -> Id = std::mem::transmute(msgsend_addr());
        f(recv, sel, a, b)
    }
    unsafe fn send_usize(recv: Id, sel: Sel, a: usize) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, usize) -> Id = std::mem::transmute(msgsend_addr());
        f(recv, sel, a)
    }
    unsafe fn send_bool_void(recv: Id, sel: Sel, a: bool) {
        let f: unsafe extern "C" fn(Id, Sel, bool) = std::mem::transmute(msgsend_addr());
        f(recv, sel, a)
    }
    unsafe fn send_isize_void(recv: Id, sel: Sel, a: isize) {
        let f: unsafe extern "C" fn(Id, Sel, isize) = std::mem::transmute(msgsend_addr());
        f(recv, sel, a)
    }
    unsafe fn send_usize_void(recv: Id, sel: Sel, a: usize) {
        let f: unsafe extern "C" fn(Id, Sel, usize) = std::mem::transmute(msgsend_addr());
        f(recv, sel, a)
    }
    unsafe fn send_cstr(recv: Id, sel: Sel, a: *const c_char) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, *const c_char) -> Id =
            std::mem::transmute(msgsend_addr());
        f(recv, sel, a)
    }
    unsafe fn send_rect(recv: Id, sel: Sel, a: NSRect) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, NSRect) -> Id = std::mem::transmute(msgsend_addr());
        f(recv, sel, a)
    }
    unsafe fn send_rect_void(recv: Id, sel: Sel, a: NSRect) {
        let f: unsafe extern "C" fn(Id, Sel, NSRect) = std::mem::transmute(msgsend_addr());
        f(recv, sel, a)
    }
    unsafe fn send_rect_bool_void(recv: Id, sel: Sel, a: NSRect, b: bool) {
        let f: unsafe extern "C" fn(Id, Sel, NSRect, bool) = std::mem::transmute(msgsend_addr());
        f(recv, sel, a, b)
    }
    unsafe fn send_window_init(
        recv: Id,
        sel: Sel,
        rect: NSRect,
        style_mask: usize,
        backing: usize,
        defer_flag: bool,
    ) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, NSRect, usize, usize, bool) -> Id =
            std::mem::transmute(msgsend_addr());
        f(recv, sel, rect, style_mask, backing, defer_flag)
    }
    unsafe fn send_image_init(recv: Id, sel: Sel, cg_image: *mut c_void, size: NSSize) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, *mut c_void, NSSize) -> Id =
            std::mem::transmute(msgsend_addr());
        f(recv, sel, cg_image, size)
    }

    unsafe fn ns_string(s: &str) -> Id {
        let cls = get_class("NSString");
        let c = CString::new(s).unwrap_or_else(|_| CString::new("").expect("empty cstring"));
        send_cstr(cls, sel("stringWithUTF8String:"), c.as_ptr())
    }

    unsafe fn ns_string_to_string(obj: Id) -> Option<String> {
        if obj.is_null() {
            return None;
        }
        let ptr = send0_cstr(obj, sel("UTF8String"));
        if ptr.is_null() {
            return None;
        }
        Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
    }

    unsafe fn ns_array_strings(arr: Id) -> Vec<String> {
        let mut out = Vec::new();
        if arr.is_null() {
            return out;
        }
        let count = send0_usize(arr, sel("count"));
        for i in 0..count {
            let item = send_usize(arr, sel("objectAtIndex:"), i);
            if let Some(s) = ns_string_to_string(item) {
                out.push(s);
            }
        }
        out
    }

    // -----------------------------------------------------------------
    // Shared state. Everything runs on the run-loop thread (tap
    // callback + timer callback + AppKit drag callbacks all dispatch
    // through the same CFRunLoop), so the mutex is never contended for
    // long — but callbacks must never hold it across an AppKit call
    // that could re-enter one of our IMPs; `tick` collects *actions*
    // under the lock and executes them after releasing it.
    // -----------------------------------------------------------------

    const TICK_INTERVAL: f64 = 0.03;
    /// Grace after physical mouse-up before a no-drop drag is declared
    /// ended: AppKit delivers performDragOperation *after* the tap
    /// has already seen the release event.
    const DROP_GRACE: Duration = Duration::from_millis(150);
    const TOAST_TTL: Duration = Duration::from_millis(3600);
    /// Permission re-check cadence: every ~2 s of ticks.
    const PERMISSION_POLL_TICKS: u64 = 66;

    struct State {
        running: bool,
        config: SensorConfig,
        events: Option<Sender<SensorEvent>>,
        commands: Option<Receiver<SensorCommand>>,
        resolve: Option<WheelResolver>,
        run_loop: usize,
        tap_port: usize,
        tap_source: usize,
        timer: usize,
        trusted: bool,
        notice_sent: bool,
        prompt_sent: bool,
        tick_count: u64,
        // AppKit objects (Id stored as usize; 0 = none).
        wheel_panel: usize,
        wheel_image_view: usize,
        toast_panel: usize,
        toast_image_view: usize,
        toast_shown_at: Option<Instant>,
        toast_size: (u32, u32),
        // Drag session.
        button_down: bool,
        ignore_until_up: bool,
        down_pos: (f64, f64),
        last_pos: (f64, f64),
        candidate: bool,
        classify_pending: bool,
        classify_done: bool,
        classified: bool,
        drag_types: Vec<String>,
        modifier_down: bool,
        summon_pending: bool,
        summon_tried: bool,
        summoned: bool,
        suppress: bool,
        esc_pending: bool,
        model: Option<WheelModel>,
        paths: Vec<PathBuf>,
        hover: Option<usize>,
        drop_result: Option<(Vec<PathBuf>, String)>,
        conclude_pending: bool,
        mouse_up_at: Option<Instant>,
        session_active: bool,
        shutdown: bool,
    }

    impl State {
        const fn new() -> State {
            State {
                running: false,
                config: SensorConfig {
                    modifier: crate::SummonModifier::Shift,
                    lang: SensorLang::Ar,
                    enabled: true,
                },
                events: None,
                commands: None,
                resolve: None,
                run_loop: 0,
                tap_port: 0,
                tap_source: 0,
                timer: 0,
                trusted: false,
                notice_sent: false,
                prompt_sent: false,
                tick_count: 0,
                wheel_panel: 0,
                wheel_image_view: 0,
                toast_panel: 0,
                toast_image_view: 0,
                toast_shown_at: None,
                toast_size: (0, 0),
                button_down: false,
                ignore_until_up: false,
                down_pos: (0.0, 0.0),
                last_pos: (0.0, 0.0),
                candidate: false,
                classify_pending: false,
                classify_done: false,
                classified: false,
                drag_types: Vec::new(),
                modifier_down: false,
                summon_pending: false,
                summon_tried: false,
                summoned: false,
                suppress: false,
                esc_pending: false,
                model: None,
                paths: Vec::new(),
                hover: None,
                drop_result: None,
                conclude_pending: false,
                mouse_up_at: None,
                session_active: false,
                shutdown: false,
            }
        }

        fn reset_drag(&mut self) {
            self.button_down = false;
            self.ignore_until_up = false;
            self.candidate = false;
            self.classify_pending = false;
            self.classify_done = false;
            self.classified = false;
            self.drag_types = Vec::new();
            self.summon_pending = false;
            self.summon_tried = false;
            self.summoned = false;
            self.suppress = false;
            self.esc_pending = false;
            self.model = None;
            self.paths = Vec::new();
            self.hover = None;
            self.drop_result = None;
            self.conclude_pending = false;
            self.mouse_up_at = None;
            self.session_active = false;
        }
    }

    static STATE: Mutex<State> = Mutex::new(State::new());

    fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
        let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }

    fn emit(state: &State, ev: SensorEvent) {
        if let Some(tx) = &state.events {
            let _ = tx.send(ev);
        }
    }

    /// Work decided under the lock, executed after it is released so no
    /// AppKit call ever happens while STATE is held.
    enum Action {
        CreateTap,
        ClassifyPlaceholder,
        Resolve {
            paths: Vec<PathBuf>,
            mime_types: Vec<String>,
            x: f64,
            y: f64,
        },
        HideWheel,
        ShowToast(Toast, bool),
        HideToast,
        StopLoop,
    }

    // -----------------------------------------------------------------
    // Drag pasteboard access
    // -----------------------------------------------------------------

    unsafe fn drag_pasteboard() -> Id {
        let cls = get_class("NSPasteboard");
        if cls.is_null() {
            return std::ptr::null_mut();
        }
        // NSPasteboardNameDrag is the exported constant @"…drag"; its
        // value is the well-known name string below.
        let name = ns_string("Apple CFPasteboard drag");
        send_id(cls, sel("pasteboardWithName:"), name)
    }

    /// Type list only — classification never pulls the payload.
    unsafe fn pasteboard_types(pb: Id) -> Vec<String> {
        if pb.is_null() {
            return Vec::new();
        }
        let types = send0(pb, sel("types"));
        ns_array_strings(types)
    }

    /// Paths from an arbitrary pasteboard (drag or dragging-info):
    /// 1. `readObjects(forClasses: [NSURL])` filtered to file URLs
    ///    (the documented route, also correct for the drop itself);
    /// 2. the legacy filenames property list;
    /// 3. the filenames payload parsed as a plist string array.
    unsafe fn pasteboard_paths(pb: Id) -> Vec<PathBuf> {
        if pb.is_null() {
            return Vec::new();
        }
        // 1) NSURL objects.
        let classes = {
            let arr_cls = get_class("NSMutableArray");
            let arr = send0(arr_cls, sel("array"));
            let url_cls = get_class("NSURL");
            if !arr.is_null() && !url_cls.is_null() {
                send_id_void(arr, sel("addObject:"), url_cls);
            }
            arr
        };
        let objects = send_id_id(
            pb,
            sel("readObjectsForClasses:options:"),
            classes,
            std::ptr::null_mut(),
        );
        if !objects.is_null() {
            let count = send0_usize(objects, sel("count"));
            let mut out = Vec::new();
            for i in 0..count {
                let url = send_usize(objects, sel("objectAtIndex:"), i);
                if url.is_null() || !send0_bool(url, sel("isFileURL")) {
                    continue;
                }
                let rep = send0_cstr(url, sel("fileSystemRepresentation"));
                if !rep.is_null() {
                    let bytes = CStr::from_ptr(rep).to_bytes();
                    out.push(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)));
                } else if let Some(p) = ns_string_to_string(send0(url, sel("path"))) {
                    out.push(PathBuf::from(p));
                }
            }
            if !out.is_empty() {
                return out;
            }
        }
        // 2) Legacy filenames property list (an NSArray of NSString).
        let filenames_type = ns_string(PB_TYPE_FILENAMES);
        let plist = send_id(pb, sel("propertyListForType:"), filenames_type);
        if !plist.is_null() {
            let names = ns_array_strings(plist);
            if !names.is_empty() {
                return names.into_iter().map(PathBuf::from).collect();
            }
        }
        // 3) Raw payload parsed as a plist string array.
        let data = send_id(pb, sel("dataForType:"), filenames_type);
        if !data.is_null() {
            let len = send0_usize(data, sel("length"));
            let ptr = send0_ptr(data, sel("bytes"));
            if !ptr.is_null() && len > 0 {
                let bytes = std::slice::from_raw_parts(ptr, len);
                if let Ok(text) = std::str::from_utf8(bytes) {
                    let names = parse_filenames_plist(text);
                    if !names.is_empty() {
                        return names.into_iter().map(PathBuf::from).collect();
                    }
                }
            }
        }
        Vec::new()
    }

    // -----------------------------------------------------------------
    // Pixmap → NSImage (via CGImage over the shared BGRA bytes)
    // -----------------------------------------------------------------

    extern "C" fn provider_release(info: *mut c_void, _data: *const u8, _size: usize) {
        if !info.is_null() {
            unsafe {
                drop(Box::from_raw(info as *mut Vec<u8>));
            }
        }
    }

    unsafe fn ns_image_from_bgra(bgra: Vec<u8>, w: u32, h: u32) -> Id {
        let len = bgra.len();
        let data_ptr = bgra.as_ptr();
        let info = Box::into_raw(Box::new(bgra)) as *mut c_void;
        let provider = CGDataProviderCreateWithData(info, data_ptr, len, Some(provider_release));
        if provider.is_null() {
            return std::ptr::null_mut();
        }
        let space = CGColorSpaceCreateDeviceRGB();
        let cg = CGImageCreate(
            w as usize,
            h as usize,
            8,
            32,
            w as usize * 4,
            space,
            BITMAP_INFO_BGRA_PREMUL,
            provider,
            std::ptr::null(),
            false,
            0,
        );
        if !space.is_null() {
            CGColorSpaceRelease(space);
        }
        CGDataProviderRelease(provider);
        if cg.is_null() {
            return std::ptr::null_mut();
        }
        let cls = get_class("NSImage");
        let img = send0(cls, sel("alloc"));
        let img = send_image_init(
            img,
            sel("initWithCGImage:size:"),
            cg,
            NSSize {
                width: w as f64,
                height: h as f64,
            },
        );
        CGImageRelease(cg);
        img
    }

    unsafe fn set_image_view(image_view: usize, pm: &tiny_skia::Pixmap) {
        if image_view == 0 {
            return;
        }
        let img = ns_image_from_bgra(pixmap_to_bgra(pm), pm.width(), pm.height());
        if !img.is_null() {
            send_id_void(image_view as Id, sel("setImage:"), img);
            send0_void(img, sel("release"));
        }
    }

    // -----------------------------------------------------------------
    // Panels
    // -----------------------------------------------------------------

    /// Style mask: borderless | nonactivatingPanel (1 << 7).
    const PANEL_STYLE: usize = 128;
    /// NSWindowLevel popUpMenu.
    const PANEL_LEVEL: isize = 101;
    /// canJoinAllSpaces (1<<0) | stationary (1<<4).
    const PANEL_COLLECTION: usize = (1 << 0) | (1 << 4);
    const BACKING_BUFFERED: usize = 2;

    unsafe fn make_panel(w: f64, h: f64, content_view: Id) -> Id {
        let cls = get_class("NSPanel");
        if cls.is_null() {
            return std::ptr::null_mut();
        }
        let alloc = send0(cls, sel("alloc"));
        let panel = send_window_init(
            alloc,
            sel("initWithContentRect:styleMask:backing:defer:"),
            NSRect {
                origin: NSPoint { x: 0.0, y: 0.0 },
                size: NSSize {
                    width: w,
                    height: h,
                },
            },
            PANEL_STYLE,
            BACKING_BUFFERED,
            false,
        );
        if panel.is_null() {
            return panel;
        }
        send_bool_void(panel, sel("setOpaque:"), false);
        let color_cls = get_class("NSColor");
        let clear = send0(color_cls, sel("clearColor"));
        send_id_void(panel, sel("setBackgroundColor:"), clear);
        send_bool_void(panel, sel("setHasShadow:"), false);
        send_isize_void(panel, sel("setLevel:"), PANEL_LEVEL);
        send_usize_void(panel, sel("setCollectionBehavior:"), PANEL_COLLECTION);
        send_bool_void(panel, sel("setReleasedWhenClosed:"), false);
        send_bool_void(panel, sel("setFloatingPanel:"), true);
        send_bool_void(panel, sel("setBecomesKeyOnlyIfNeeded:"), true);
        if !content_view.is_null() {
            send_id_void(panel, sel("setContentView:"), content_view);
        }
        panel
    }

    unsafe fn make_image_view(w: f64, h: f64) -> Id {
        let cls = get_class("NSImageView");
        if cls.is_null() {
            return std::ptr::null_mut();
        }
        let alloc = send0(cls, sel("alloc"));
        send_rect(
            alloc,
            sel("initWithFrame:"),
            NSRect {
                origin: NSPoint { x: 0.0, y: 0.0 },
                size: NSSize {
                    width: w,
                    height: h,
                },
            },
        )
    }

    unsafe fn make_plain_view(w: f64, h: f64) -> Id {
        let cls = get_class("NSView");
        if cls.is_null() {
            return std::ptr::null_mut();
        }
        let alloc = send0(cls, sel("alloc"));
        send_rect(
            alloc,
            sel("initWithFrame:"),
            NSRect {
                origin: NSPoint { x: 0.0, y: 0.0 },
                size: NSSize {
                    width: w,
                    height: h,
                },
            },
        )
    }

    /// Main-display bounds in Quartz points (origin top-left). AppKit
    /// window origins are bottom-left, so y is flipped against the
    /// main display's height (exact for the primary display; see the
    /// module docs for the multi-display caveat).
    unsafe fn main_display_bounds() -> CGRect {
        CGDisplayBounds(CGMainDisplayID())
    }

    unsafe fn show_wheel_panel(
        panel: usize,
        image_view: usize,
        model: &WheelModel,
        cx: f64,
        cy: f64,
    ) {
        if panel == 0 {
            return;
        }
        let pm = render_wheel(model, None);
        set_image_view(image_view, &pm);
        let half = geom::WHEEL_SIZE as f64 / 2.0;
        let bounds = main_display_bounds();
        let frame = NSRect {
            origin: NSPoint {
                x: cx - half,
                y: bounds.size.height - cy - half,
            },
            size: NSSize {
                width: geom::WHEEL_SIZE as f64,
                height: geom::WHEEL_SIZE as f64,
            },
        };
        send_rect_bool_void(panel as Id, sel("setFrame:display:"), frame, true);
        send_id_void(panel as Id, sel("orderFront:"), std::ptr::null_mut());
    }

    unsafe fn hide_panel(panel: usize) {
        if panel != 0 {
            send_id_void(panel as Id, sel("orderOut:"), std::ptr::null_mut());
        }
    }

    unsafe fn show_toast_panel(panel: usize, image_view: usize, toast: &Toast, rtl: bool) {
        if panel == 0 {
            return;
        }
        let kind = match toast.kind {
            crate::ToastKind::Success => ToastKind::Success,
            crate::ToastKind::Error => ToastKind::Error,
        };
        let pm = render_toast(kind, &toast.title, &toast.body, rtl);
        let (w, h) = (pm.width(), pm.height());
        set_image_view(image_view, &pm);
        if image_view != 0 {
            send_rect_void(
                image_view as Id,
                sel("setFrame:"),
                NSRect {
                    origin: NSPoint { x: 0.0, y: 0.0 },
                    size: NSSize {
                        width: w as f64,
                        height: h as f64,
                    },
                },
            );
        }
        let bounds = main_display_bounds();
        let frame = NSRect {
            origin: NSPoint {
                x: (bounds.size.width - w as f64) / 2.0,
                y: 28.0,
            },
            size: NSSize {
                width: w as f64,
                height: h as f64,
            },
        };
        send_rect_bool_void(panel as Id, sel("setFrame:display:"), frame, true);
        send_id_void(panel as Id, sel("orderFront:"), std::ptr::null_mut());
        with_state(|s| {
            s.toast_shown_at = Some(Instant::now());
            s.toast_size = (w, h);
        });
    }

    // -----------------------------------------------------------------
    // NSDraggingDestination — a runtime-defined NSView subclass.
    // The IMPs stay tiny: pasteboard reads + state writes; rendering a
    // hover change is the only AppKit work done inline (it cannot
    // re-enter these methods).
    // -----------------------------------------------------------------

    /// Segment under a dragging location given in the destination
    /// window's coordinates (bottom-left origin, y up).
    fn segment_for_window_point(model: &WheelModel, loc: NSPoint) -> Option<usize> {
        let half = geom::WHEEL_SIZE as f64 / 2.0;
        let dx = loc.x - half;
        let dy = -(loc.y - half); // geom works in y-down screen space
        geom::segment_at(model.segments.len(), dx as f32, dy as f32)
    }

    unsafe fn dragging_location(sender: Id) -> NSPoint {
        send0_point(sender, sel("draggingLocation"))
    }

    extern "C" fn imp_dragging_entered(this: Id, _cmd: Sel, sender: Id) -> usize {
        let _ = this;
        if sender.is_null() {
            return DRAG_OP_NONE;
        }
        let loc = unsafe { dragging_location(sender) };
        let seg = with_state(|s| {
            if !s.summoned {
                return None;
            }
            let seg = s
                .model
                .as_ref()
                .and_then(|m| segment_for_window_point(m, loc));
            s.hover = seg;
            seg
        });
        if seg.is_some() {
            DRAG_OP_COPY
        } else {
            DRAG_OP_NONE
        }
    }

    extern "C" fn imp_dragging_updated(this: Id, _cmd: Sel, sender: Id) -> usize {
        let _ = this;
        if sender.is_null() {
            return DRAG_OP_NONE;
        }
        let loc = unsafe { dragging_location(sender) };
        // Compute + record under the lock; render after releasing it.
        let (changed, model, image_view) = with_state(|s| {
            if !s.summoned {
                return (false, None, 0usize);
            }
            let seg = s
                .model
                .as_ref()
                .and_then(|m| segment_for_window_point(m, loc));
            if seg != s.hover {
                s.hover = seg;
                (true, s.model.clone(), s.wheel_image_view)
            } else {
                (false, None, 0usize)
            }
        });
        if changed {
            if let Some(model) = &model {
                let hover = with_state(|s| s.hover);
                let pm = render_wheel(model, hover);
                unsafe { set_image_view(image_view, &pm) };
            }
        }
        let seg = with_state(|s| s.hover);
        if seg.is_some() {
            DRAG_OP_COPY
        } else {
            DRAG_OP_NONE
        }
    }

    extern "C" fn imp_dragging_exited(this: Id, _cmd: Sel, sender: Id) {
        let _ = (this, sender);
        let (model, image_view) = with_state(|s| {
            if s.hover.take().is_some() {
                (s.model.clone(), s.wheel_image_view)
            } else {
                (None, 0usize)
            }
        });
        if let Some(model) = &model {
            let pm = render_wheel(model, None);
            unsafe { set_image_view(image_view, &pm) };
        }
    }

    extern "C" fn imp_prepare(this: Id, _cmd: Sel, sender: Id) -> bool {
        let _ = (this, sender);
        with_state(|s| s.summoned)
    }

    extern "C" fn imp_perform(this: Id, _cmd: Sel, sender: Id) -> bool {
        let _ = this;
        if sender.is_null() {
            return false;
        }
        // The dragging pasteboard's paths are authoritative for the drop.
        let pb = unsafe { send0(sender, sel("draggingPasteboard")) };
        let paths = unsafe { pasteboard_paths(pb) };
        if paths.is_empty() {
            return false;
        }
        let loc = unsafe { dragging_location(sender) };
        let target = with_state(|s| {
            let model = s.model.as_ref()?;
            let seg = segment_for_window_point(model, loc).or(s.hover)?;
            model.segments.get(seg).cloned()
        });
        match target {
            Some(target) => {
                with_state(|s| {
                    s.drop_result = Some((paths, target));
                });
                true
            }
            None => false,
        }
    }

    extern "C" fn imp_conclude(this: Id, _cmd: Sel, sender: Id) {
        let _ = (this, sender);
        with_state(|s| {
            s.conclude_pending = true;
        });
    }

    #[cfg(target_arch = "aarch64")]
    const BOOL_ENCODING: &[u8] = b"B@:@\0";
    #[cfg(not(target_arch = "aarch64"))]
    const BOOL_ENCODING: &[u8] = b"c@:@\0";

    unsafe fn ensure_drag_view_class() -> Class {
        let name = CString::new("UrukDragView").expect("class name");
        let existing = objc_getClass(name.as_ptr());
        if !existing.is_null() {
            return existing;
        }
        let superclass = objc_getClass(CString::new("NSView").expect("NSView").as_ptr());
        if superclass.is_null() {
            return std::ptr::null_mut();
        }
        let cls = objc_allocateClassPair(superclass, name.as_ptr(), 0);
        if cls.is_null() {
            return cls;
        }
        let uint_enc = b"Q@:@\0".as_ptr() as *const c_char;
        let void_enc = b"v@:@\0".as_ptr() as *const c_char;
        let bool_enc = BOOL_ENCODING.as_ptr() as *const c_char;
        class_addMethod(
            cls,
            sel("draggingEntered:"),
            imp_dragging_entered as *const c_void,
            uint_enc,
        );
        class_addMethod(
            cls,
            sel("draggingUpdated:"),
            imp_dragging_updated as *const c_void,
            uint_enc,
        );
        class_addMethod(
            cls,
            sel("draggingExited:"),
            imp_dragging_exited as *const c_void,
            void_enc,
        );
        class_addMethod(
            cls,
            sel("prepareForDragOperation:"),
            imp_prepare as *const c_void,
            bool_enc,
        );
        class_addMethod(
            cls,
            sel("performDragOperation:"),
            imp_perform as *const c_void,
            bool_enc,
        );
        class_addMethod(
            cls,
            sel("concludeDragOperation:"),
            imp_conclude as *const c_void,
            void_enc,
        );
        objc_registerClassPair(cls);
        cls
    }

    unsafe fn make_drag_view(w: f64, h: f64) -> Id {
        let cls = ensure_drag_view_class();
        if cls.is_null() {
            return std::ptr::null_mut();
        }
        let alloc = send0(cls as Id, sel("alloc"));
        if alloc.is_null() {
            return alloc;
        }
        let view = send_rect(
            alloc,
            sel("initWithFrame:"),
            NSRect {
                origin: NSPoint { x: 0.0, y: 0.0 },
                size: NSSize {
                    width: w,
                    height: h,
                },
            },
        );
        if !view.is_null() {
            let types = {
                let arr_cls = get_class("NSMutableArray");
                let arr = send0(arr_cls, sel("array"));
                if !arr.is_null() {
                    let file_url = ns_string(PB_TYPE_FILE_URL);
                    send_id_void(arr, sel("addObject:"), file_url);
                }
                arr
            };
            send_id_void(view, sel("registerForDraggedTypes:"), types);
        }
        view
    }

    // -----------------------------------------------------------------
    // Event tap
    // -----------------------------------------------------------------

    unsafe fn try_create_tap() -> bool {
        let already = with_state(|s| s.tap_port != 0);
        if already {
            return true;
        }
        let mask: u64 = (1 << EV_LEFT_MOUSE_DOWN)
            | (1 << EV_LEFT_MOUSE_UP)
            | (1 << EV_LEFT_MOUSE_DRAGGED)
            | (1 << EV_KEY_DOWN)
            | (1 << EV_FLAGS_CHANGED);
        // .cgSessionEventTap (1), .headInsertEventTap (0), .listenOnly (1).
        let port = CGEventTapCreate(1, 0, 1, mask, Some(tap_callback), std::ptr::null_mut());
        if port.is_null() {
            return false;
        }
        let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
        if source.is_null() {
            CFRelease(port as CFTypeRef);
            return false;
        }
        let rl = with_state(|s| s.run_loop) as *mut c_void;
        CFRunLoopAddSource(rl, source, kCFRunLoopCommonModes);
        CGEventTapEnable(port, true);
        with_state(|s| {
            s.tap_port = port as usize;
            s.tap_source = source as usize;
            s.trusted = true;
        });
        true
    }

    /// Near-empty by design: record raw state, let `tick` do the work.
    extern "C" fn tap_callback(
        _proxy: *mut c_void,
        typ: u32,
        event: *mut c_void,
        _info: *mut c_void,
    ) -> *mut c_void {
        match typ {
            EV_TAP_DISABLED_BY_TIMEOUT | EV_TAP_DISABLED_BY_USER_INPUT => {
                let port = with_state(|s| s.tap_port);
                if port != 0 {
                    unsafe { CGEventTapEnable(port as *mut c_void, true) };
                }
            }
            EV_LEFT_MOUSE_DOWN => {
                let loc = unsafe { CGEventGetLocation(event) };
                with_state(|s| {
                    // A fresh press restarts the session bookkeeping; a
                    // previously summoned wheel is stale — tick hides it
                    // via the reset (panels are re-shown per session).
                    s.reset_drag();
                    s.button_down = true;
                    s.down_pos = (loc.x, loc.y);
                    s.last_pos = (loc.x, loc.y);
                });
            }
            EV_LEFT_MOUSE_DRAGGED => {
                let loc = unsafe { CGEventGetLocation(event) };
                let flags = unsafe { CGEventGetFlags(event) };
                with_state(|s| {
                    if s.ignore_until_up || !s.button_down {
                        return;
                    }
                    s.last_pos = (loc.x, loc.y);
                    if !s.candidate && beyond_threshold(s.down_pos, s.last_pos) {
                        s.candidate = true;
                        if !s.classify_done {
                            s.classify_pending = true;
                        }
                    }
                    note_modifier(s, flags);
                });
            }
            EV_FLAGS_CHANGED => {
                let flags = unsafe { CGEventGetFlags(event) };
                with_state(|s| {
                    if s.ignore_until_up {
                        return;
                    }
                    note_modifier(s, flags);
                });
            }
            EV_KEY_DOWN => {
                let code = unsafe { CGEventGetIntegerValueField(event, FIELD_KEYCODE) };
                if is_escape_keycode(code) {
                    with_state(|s| {
                        if s.summoned {
                            s.esc_pending = true;
                        }
                    });
                }
            }
            EV_LEFT_MOUSE_UP => {
                with_state(|s| {
                    s.button_down = false;
                    if s.mouse_up_at.is_none() {
                        s.mouse_up_at = Some(Instant::now());
                    }
                });
            }
            _ => {}
        }
        event
    }

    /// Track the configured modifier's state; a rising edge during a
    /// live drag asks `tick` to summon.
    fn note_modifier(s: &mut State, flags: u64) {
        let now = modifier_is_down(flags, s.config.modifier);
        if now && !s.modifier_down {
            if s.candidate && !s.summoned && !s.suppress && s.config.enabled {
                s.summon_pending = true;
            }
        }
        if !now {
            // A fresh tap is a fresh summon attempt.
            s.summon_tried = false;
        }
        s.modifier_down = now;
    }

    // -----------------------------------------------------------------
    // Run-loop tick: commands, permission poll, state machine.
    // -----------------------------------------------------------------

    extern "C" fn timer_callback(_timer: *mut c_void, _info: *mut c_void) {
        tick();
    }

    fn tick() {
        let mut actions: Vec<Action> = Vec::new();
        with_state(|s| {
            s.tick_count += 1;

            // --- host commands -------------------------------------
            let mut drained: Vec<SensorCommand> = Vec::new();
            if let Some(rx) = &s.commands {
                while let Ok(cmd) = rx.try_recv() {
                    drained.push(cmd);
                }
            }
            for cmd in drained {
                match cmd {
                    SensorCommand::Shutdown => {
                        s.shutdown = true;
                        actions.push(Action::StopLoop);
                    }
                    SensorCommand::UpdateConfig(cfg) => {
                        let was_enabled = s.config.enabled;
                        let now_enabled = cfg.enabled;
                        s.config = cfg;
                        if was_enabled && !now_enabled && s.summoned {
                            s.summoned = false;
                            actions.push(Action::HideWheel);
                            if s.session_active {
                                emit(s, SensorEvent::DragEnded);
                                s.session_active = false;
                            }
                            s.suppress = true;
                        }
                    }
                    SensorCommand::ShowToast(toast) => {
                        let rtl = s.config.lang.is_rtl();
                        actions.push(Action::ShowToast(toast, rtl));
                    }
                }
            }

            // --- toast auto-dismiss --------------------------------
            if let Some(shown) = s.toast_shown_at {
                if shown.elapsed() >= TOAST_TTL {
                    s.toast_shown_at = None;
                    actions.push(Action::HideToast);
                }
            }

            // --- permission poll / tap creation ---------------------
            if s.config.enabled && s.tap_port == 0 {
                if s.tick_count % PERMISSION_POLL_TICKS == 1 {
                    actions.push(Action::CreateTap);
                }
            }

            // --- Esc dismissal --------------------------------------
            if s.esc_pending {
                s.esc_pending = false;
                if s.summoned {
                    s.summoned = false;
                    actions.push(Action::HideWheel);
                }
                if s.session_active {
                    emit(s, SensorEvent::DragEnded);
                    s.session_active = false;
                }
                // The source app sees the same Esc and usually cancels
                // the drag; ignore this drag until the button comes up.
                s.suppress = true;
                s.ignore_until_up = true;
                s.candidate = false;
                s.classified = false;
                s.summon_pending = false;
            }

            // --- drop concluded on the wheel ------------------------
            if s.conclude_pending {
                s.conclude_pending = false;
                if let Some((paths, target)) = s.drop_result.take() {
                    actions.push(Action::HideWheel);
                    emit(s, SensorEvent::ConvertRequested { paths, target });
                } else if s.summoned {
                    actions.push(Action::HideWheel);
                    if s.session_active {
                        emit(s, SensorEvent::DragEnded);
                    }
                }
                s.reset_drag();
                // The physical button may still be down briefly; the
                // coming mouse-up finds an idle session and is a no-op.
                s.ignore_until_up = true;
            }

            // --- classify -------------------------------------------
            // The pasteboard read itself runs in the action phase
            // (below), never under the state lock.
            if s.classify_pending && !s.ignore_until_up {
                s.classify_pending = false;
                s.classify_done = true;
                actions.push(Action::ClassifyPlaceholder);
            }

            // --- summon fallback: modifier held as drag confirms ----
            if s.candidate
                && s.classified
                && s.modifier_down
                && !s.summoned
                && !s.suppress
                && !s.summon_tried
                && s.config.enabled
            {
                s.summon_pending = true;
            }
            if s.summon_pending {
                s.summon_pending = false;
                s.summon_tried = true;
                if s.classified && !s.summoned && !s.suppress && s.config.enabled {
                    let req_paths = s.paths.clone();
                    let req_types = s.drag_types.clone();
                    let pos = s.last_pos;
                    actions.push(Action::Resolve {
                        paths: req_paths,
                        mime_types: req_types,
                        x: pos.0,
                        y: pos.1,
                    });
                }
            }

            // --- physical drag end ----------------------------------
            if let Some(up_at) = s.mouse_up_at {
                if s.drop_result.is_some() || up_at.elapsed() >= DROP_GRACE {
                    let drop = s.drop_result.take();
                    let had_session = s.session_active;
                    let was_summoned = s.summoned;
                    s.reset_drag();
                    if was_summoned {
                        actions.push(Action::HideWheel);
                    }
                    match drop {
                        Some((paths, target)) => {
                            emit(s, SensorEvent::ConvertRequested { paths, target });
                        }
                        None => {
                            if had_session {
                                emit(s, SensorEvent::DragEnded);
                            }
                        }
                    }
                }
            }
        });

        // Execute actions WITHOUT holding the state lock.
        let mut classify_after: Vec<Action> = Vec::new();
        for action in actions {
            match action {
                Action::StopLoop => {
                    let rl = with_state(|s| s.run_loop);
                    if rl != 0 {
                        unsafe { CFRunLoopStop(rl as *mut c_void) };
                    }
                }
                Action::CreateTap => unsafe {
                    if AXIsProcessTrusted() {
                        try_create_tap();
                    }
                },
                Action::HideWheel => {
                    let panel = with_state(|s| s.wheel_panel);
                    unsafe { hide_panel(panel) };
                }
                Action::HideToast => {
                    let panel = with_state(|s| s.toast_panel);
                    unsafe { hide_panel(panel) };
                }
                Action::ShowToast(toast, rtl) => {
                    let (panel, image_view) = with_state(|s| (s.toast_panel, s.toast_image_view));
                    unsafe { show_toast_panel(panel, image_view, &toast, rtl) };
                }
                Action::Resolve {
                    paths,
                    mime_types,
                    x,
                    y,
                } => {
                    do_resolve_and_summon(paths, mime_types, x, y);
                }
                Action::ClassifyPlaceholder => {
                    classify_after.push(Action::ClassifyPlaceholder);
                }
            }
        }
        for _ in classify_after {
            do_classify();
            // A classification that confirms a file drag while the
            // modifier is already down should summon on the next
            // tick's fallback; if a rising edge arrived while
            // classification was pending, summon right away.
            let summon_now = with_state(|s| {
                s.classified && s.summon_pending && !s.summoned && !s.suppress && s.config.enabled
            });
            if summon_now {
                with_state(|s| {
                    s.summon_pending = false;
                    s.summon_tried = true;
                });
                let (paths, types, pos) =
                    with_state(|s| (s.paths.clone(), s.drag_types.clone(), s.last_pos));
                do_resolve_and_summon(paths, types, pos.0, pos.1);
            }
        }
    }

    /// Classify the current drag from the drag pasteboard's type list.
    fn do_classify() {
        let types = unsafe {
            let pb = drag_pasteboard();
            pasteboard_types(pb)
        };
        if !is_file_drag_types(&types) {
            with_state(|s| {
                s.classified = false;
            });
            return;
        }
        with_state(|s| {
            if s.ignore_until_up || !s.candidate {
                return;
            }
            s.classified = true;
            s.drag_types = types.clone();
            s.session_active = true;
            emit(s, SensorEvent::DragClassified { mime_types: types });
        });
    }

    /// Read the dragged paths, ask the host resolver for a wheel, and
    /// show it at the cursor if the host offers one.
    fn do_resolve_and_summon(paths_hint: Vec<PathBuf>, mime_types: Vec<String>, x: f64, y: f64) {
        let paths = if paths_hint.is_empty() {
            unsafe {
                let pb = drag_pasteboard();
                pasteboard_paths(pb)
            }
        } else {
            paths_hint
        };
        let resolve = with_state(|s| s.resolve.clone());
        let model = resolve.and_then(|r| {
            r(&WheelRequest {
                paths: paths.clone(),
                mime_types,
            })
        });
        let Some(model) = model else {
            return;
        };
        let proceed = with_state(|s| {
            if s.ignore_until_up || s.summoned || !s.button_down {
                false
            } else {
                s.model = Some(model.clone());
                s.paths = paths.clone();
                s.summoned = true;
                s.session_active = true;
                true
            }
        });
        if !proceed {
            return;
        }
        let (panel, image_view) = with_state(|s| (s.wheel_panel, s.wheel_image_view));
        unsafe { show_wheel_panel(panel, image_view, &model, x, y) };
        with_state(|s| {
            emit(
                s,
                SensorEvent::Summoned {
                    x: x as i32,
                    y: y as i32,
                },
            );
        });
    }

    // -----------------------------------------------------------------
    // Entry point
    // -----------------------------------------------------------------

    pub fn run(
        config: SensorConfig,
        events: Sender<SensorEvent>,
        commands: Receiver<SensorCommand>,
        resolve: WheelResolver,
    ) -> Result<(), SensorError> {
        let already = with_state(|s| {
            if s.running {
                true
            } else {
                s.running = true;
                false
            }
        });
        if already {
            return Err(SensorError::new(
                "already_running",
                "the macOS sensor is already running",
            ));
        }

        let result = run_inner(config, events, commands, resolve);

        with_state(|s| {
            s.running = false;
            s.events = None;
            s.commands = None;
            s.resolve = None;
            s.reset_drag();
        });
        result
    }

    fn run_inner(
        config: SensorConfig,
        events: Sender<SensorEvent>,
        commands: Receiver<SensorCommand>,
        resolve: WheelResolver,
    ) -> Result<(), SensorError> {
        unsafe {
            // --- NSApplication (accessory: no Dock icon) ------------
            let app_cls = get_class("NSApplication");
            let app = if app_cls.is_null() {
                std::ptr::null_mut()
            } else {
                send0(app_cls, sel("sharedApplication"))
            };
            if !app.is_null() {
                // NSApplicationActivationPolicyAccessory = 1.
                let _ = send_isize_void(app, sel("setActivationPolicy:"), 1);
                send0_void(app, sel("finishLaunching"));
            }

            // --- panels (pre-created, hidden) ------------------------
            let wheel_half = geom::WHEEL_SIZE as f64;
            let drag_view = make_drag_view(wheel_half, wheel_half);
            let wheel_image = make_image_view(wheel_half, wheel_half);
            if !drag_view.is_null() && !wheel_image.is_null() {
                send_id_void(drag_view, sel("addSubview:"), wheel_image);
            }
            let wheel_panel = make_panel(wheel_half, wheel_half, drag_view);

            let toast_view = make_plain_view(320.0, TOAST_H as f64);
            let toast_image = make_image_view(320.0, TOAST_H as f64);
            if !toast_view.is_null() && !toast_image.is_null() {
                send_id_void(toast_view, sel("addSubview:"), toast_image);
            }
            let toast_panel = make_panel(320.0, TOAST_H as f64, toast_view);

            let run_loop = CFRunLoopGetCurrent();
            with_state(|s| {
                s.config = config;
                s.events = Some(events);
                s.commands = Some(commands);
                s.resolve = Some(resolve);
                s.run_loop = run_loop as usize;
                s.wheel_panel = wheel_panel as usize;
                s.wheel_image_view = wheel_image as usize;
                s.toast_panel = toast_panel as usize;
                s.toast_image_view = toast_image as usize;
            });

            // --- main-thread notice (panels want the main thread) ----
            if pthread_main_np() == 0 {
                with_state(|s| {
                    emit(
                        s,
                        SensorEvent::Notice {
                            code: "main_thread_required".into(),
                            message: "the macOS sensor should run on the main thread for \
                                      its overlay panels; sensing continues on this thread"
                                .into(),
                        },
                    );
                });
            }

            // --- permission ladder -----------------------------------
            let trusted = AXIsProcessTrusted();
            with_state(|s| {
                s.trusted = trusted;
            });
            if trusted {
                try_create_tap();
            } else {
                with_state(|s| {
                    if !s.notice_sent {
                        s.notice_sent = true;
                        emit(
                            s,
                            SensorEvent::Notice {
                                code: "permission_missing".into(),
                                message: "Uruk Convert needs Accessibility permission \
                                          (System Settings → Privacy & Security → \
                                          Accessibility) — and Input Monitoring on \
                                          macOS 10.15+ — to sense file drags system-wide. \
                                          Until it is granted, drag sensing is inactive."
                                    .into(),
                            },
                        );
                    }
                    if !s.prompt_sent {
                        s.prompt_sent = true;
                        // Prompt once via the trusted-check options
                        // dictionary { kAXTrustedCheckOptionPrompt: true },
                        // built as an NSDictionary (toll-free bridged).
                        let dict_cls = get_class("NSDictionary");
                        if !dict_cls.is_null() {
                            let dict = send_id_id(
                                dict_cls,
                                sel("dictionaryWithObject:forKey:"),
                                kCFBooleanTrue as Id,
                                kAXTrustedCheckOptionPrompt as Id,
                            );
                            let _ = AXIsProcessTrustedWithOptions(dict as CFTypeRef);
                        }
                    }
                });
            }

            // --- run-loop timer (commands / polls / state machine) ----
            let timer = CFRunLoopTimerCreate(
                std::ptr::null(),
                CFAbsoluteTimeGetCurrent() + TICK_INTERVAL,
                TICK_INTERVAL,
                0,
                0,
                Some(timer_callback),
                std::ptr::null_mut(),
            );
            if !timer.is_null() {
                CFRunLoopAddTimer(run_loop, timer, kCFRunLoopCommonModes);
                with_state(|s| {
                    s.timer = timer as usize;
                });
            }

            CFRunLoopRun();

            // --- teardown ---------------------------------------------
            let (tap_port, tap_source, timer) = with_state(|s| (s.tap_port, s.tap_source, s.timer));
            if tap_port != 0 {
                CGEventTapEnable(tap_port as *mut c_void, false);
                CFMachPortInvalidate(tap_port as *mut c_void);
            }
            if tap_source != 0 {
                CFRunLoopRemoveSource(run_loop, tap_source as *mut c_void, kCFRunLoopCommonModes);
                CFRelease(tap_source as CFTypeRef);
            }
            if tap_port != 0 {
                CFRelease(tap_port as CFTypeRef);
            }
            if timer != 0 {
                CFRunLoopTimerInvalidate(timer as *mut c_void);
                CFRelease(timer as CFTypeRef);
            }
            with_state(|s| {
                s.tap_port = 0;
                s.tap_source = 0;
                s.timer = 0;
                s.run_loop = 0;
            });
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Pure-logic tests — these run on every OS, including Linux CI.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn types(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn classification_table() {
        assert!(is_file_drag_types(&types(&["NSFilenamesPboardType"])));
        assert!(is_file_drag_types(&types(&["public.file-url"])));
        assert!(is_file_drag_types(&types(&[
            "public.utf8-plain-text",
            "NSFilenamesPboardType"
        ])));
        assert!(!is_file_drag_types(&types(&["public.utf8-plain-text"])));
        assert!(!is_file_drag_types(&types(&["public.png"])));
        assert!(!is_file_drag_types(&types(&[])));
    }

    #[test]
    fn modifier_flag_mapping() {
        // CGEventFlags generic mask bits.
        let shift = 1u64 << 17;
        let control = 1u64 << 18;
        let alt = 1u64 << 19;
        let command = 1u64 << 20;
        assert_eq!(modifier_mask(SummonModifier::Shift), shift);
        assert_eq!(modifier_mask(SummonModifier::Control), control);
        assert_eq!(modifier_mask(SummonModifier::Alt), alt);
        assert!(modifier_is_down(shift, SummonModifier::Shift));
        assert!(modifier_is_down(shift | command, SummonModifier::Shift));
        assert!(!modifier_is_down(control, SummonModifier::Shift));
        assert!(modifier_is_down(control | alt, SummonModifier::Control));
        assert!(modifier_is_down(control | alt, SummonModifier::Alt));
        assert!(!modifier_is_down(0, SummonModifier::Alt));
    }

    #[test]
    fn escape_keycode_is_53() {
        assert!(is_escape_keycode(53));
        assert!(!is_escape_keycode(49)); // space
        assert!(!is_escape_keycode(36)); // return
    }

    #[test]
    fn drag_threshold_needs_real_motion() {
        assert!(!beyond_threshold((100.0, 100.0), (102.0, 101.0)));
        assert!(beyond_threshold((100.0, 100.0), (110.0, 100.0)));
        assert!(beyond_threshold((0.0, 0.0), (3.0, 4.0))); // exactly 5px
    }

    #[test]
    fn plist_body_is_a_launch_agent_for_the_exe() {
        let body = launch_agent_plist(Path::new("/opt/Uruk Convert/uruk-convert"));
        assert!(body.contains("<string>org.uruk.convert.agent</string>"));
        assert!(body.contains("<string>/opt/Uruk Convert/uruk-convert</string>"));
        assert!(body.contains("<string>--agent</string>"));
        assert!(body.contains("<key>RunAtLoad</key>"));
        assert!(body.contains("<true/>"));
        assert!(body.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
    }

    #[test]
    fn plist_escapes_xml_in_paths() {
        let body = launch_agent_plist(Path::new("/tmp/a&b<c>/app"));
        assert!(body.contains("/tmp/a&amp;b&lt;c&gt;/app"));
        assert!(!body.contains("a&b<"));
    }

    #[test]
    fn launch_agent_path_lives_in_library() {
        let p = launch_agent_path(Path::new("/Users/test"));
        assert_eq!(
            p,
            PathBuf::from("/Users/test/Library/LaunchAgents/org.uruk.convert.agent.plist")
        );
    }

    #[test]
    fn percent_decode_handles_hex_and_junk() {
        assert_eq!(percent_decode("My%20File"), b"My File".to_vec());
        assert_eq!(percent_decode("%C3%A9"), "é".as_bytes().to_vec());
        assert_eq!(percent_decode("100%zz"), b"100%zz".to_vec());
        assert_eq!(percent_decode("trail%"), b"trail%".to_vec());
        assert_eq!(percent_decode("plain"), b"plain".to_vec());
    }

    #[test]
    fn file_url_to_path_cases() {
        assert_eq!(
            path_from_file_url("file:///Users/a/My%20File.png"),
            Some(PathBuf::from("/Users/a/My File.png"))
        );
        assert_eq!(
            path_from_file_url("file://localhost/tmp/x.jpg"),
            Some(PathBuf::from("/tmp/x.jpg"))
        );
        assert_eq!(path_from_file_url("https://example.com/x.png"), None);
        assert_eq!(path_from_file_url("file://otherhost/tmp/x"), None);
        assert_eq!(path_from_file_url("/just/a/path"), None);
    }

    #[test]
    fn filenames_plist_openstep_quoted() {
        let parsed = parse_filenames_plist("(\"/Users/a b.png\", \"/tmp/c.jpg\")");
        assert_eq!(parsed, vec!["/Users/a b.png", "/tmp/c.jpg"]);
    }

    #[test]
    fn filenames_plist_openstep_bare_and_escaped() {
        let parsed = parse_filenames_plist("(/tmp/a.png, \"/tmp/with\\\"quote.png\")");
        assert_eq!(parsed, vec!["/tmp/a.png", "/tmp/with\"quote.png"]);
        assert!(parse_filenames_plist("()").is_empty());
        assert!(parse_filenames_plist("").is_empty());
    }

    #[test]
    fn filenames_plist_xml_form() {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                   <plist version=\"1.0\"><array>\
                   <string>/tmp/a&amp;b.png</string><string>/tmp/c.jpg</string>\
                   </array></plist>";
        assert_eq!(
            parse_filenames_plist(xml),
            vec!["/tmp/a&b.png", "/tmp/c.jpg"]
        );
    }
}
