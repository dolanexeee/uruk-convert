//! Uruk Convert — drag sensing + native wheel overlay.
//!
//! One paradigm on every platform (see the Stage-3 DESIGN.md in the
//! spikes project): the app is a background agent; the user drags a file
//! anywhere in the OS; a modifier tap mid-drag summons the wheel overlay
//! at the cursor; dropping on a segment converts via `uruk-engine-core`.
//! The Tauri webview never sees the drag — all sensing and drop capture
//! is native, in the per-platform modules behind [`DragSensor`]:
//!
//! ```text
//! detect → classify → summon → participate → convert
//! ```
//!
//! Shared across platforms: the wheel geometry ([`geom`]), the software
//! wheel/toast renderer ([`render`], tiny-skia), TTF text with a small
//! Arabic shaper ([`text`], [`arabic`]) for the native surfaces, and the
//! event/command protocol below. Everything above the trait — wheel
//! contents, engine handoff, history — is the host shell's job; the host
//! answers "what should the wheel offer for these paths?" through
//! [`WheelResolver`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

pub mod arabic;
pub mod geom;
// The macOS module compiles on every OS: its pure helpers and unit
// tests run in Linux CI, and all OS FFI is cfg-gated inside the file.
pub mod macos;
pub mod panel;
pub mod render;
pub mod text;

#[cfg(target_os = "linux")]
pub mod wayland;
#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(target_os = "linux")]
pub mod x11;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// UI language for native surfaces (toasts, orb). Mirrors the app setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorLang {
    Ar,
    En,
}

impl SensorLang {
    pub fn code(&self) -> &'static str {
        match self {
            SensorLang::Ar => "ar",
            SensorLang::En => "en",
        }
    }
    pub fn from_code(code: &str) -> SensorLang {
        if code == "en" {
            SensorLang::En
        } else {
            SensorLang::Ar
        }
    }
    pub fn is_rtl(&self) -> bool {
        matches!(self, SensorLang::Ar)
    }
}

/// The modifier tapped mid-drag to summon the wheel. Shift is the
/// reference gesture and the default; the key set is configurable because
/// some source apps eat one or another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummonModifier {
    Shift,
    Control,
    Alt,
}

impl SummonModifier {
    pub fn from_code(code: &str) -> SummonModifier {
        match code {
            "control" => SummonModifier::Control,
            "alt" => SummonModifier::Alt,
            _ => SummonModifier::Shift,
        }
    }
    pub fn code(&self) -> &'static str {
        match self {
            SummonModifier::Shift => "shift",
            SummonModifier::Control => "control",
            SummonModifier::Alt => "alt",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SensorConfig {
    /// Primary summon key: its wheel is the conversion wheel.
    pub modifier: SummonModifier,
    /// Extra key that, held TOGETHER with the primary, summons the
    /// tools wheel (the reference's Shift+Option gesture).
    pub tools_modifier: SummonModifier,
    /// Key that alone summons the file-actions wheel.
    pub actions_modifier: SummonModifier,
    pub lang: SensorLang,
    pub enabled: bool,
}

impl Default for SensorConfig {
    fn default() -> Self {
        SensorConfig {
            modifier: SummonModifier::Shift,
            tools_modifier: SummonModifier::Alt,
            actions_modifier: SummonModifier::Control,
            lang: SensorLang::Ar,
            enabled: true,
        }
    }
}

/// Which wheel a summon chord selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelKind {
    Convert,
    Tools,
    Actions,
}

/// The set of summon-relevant modifiers physically held right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HeldMods {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
}

impl HeldMods {
    pub fn has(&self, m: SummonModifier) -> bool {
        match m {
            SummonModifier::Shift => self.shift,
            SummonModifier::Control => self.control,
            SummonModifier::Alt => self.alt,
        }
    }
    pub fn count(&self) -> u32 {
        self.shift as u32 + self.control as u32 + self.alt as u32
    }
}

/// Resolve a held-modifier set to a wheel kind — the one chord rule
/// every platform shares. A wheel's chord must be fully held; among
/// matching chords the largest wins, ties break Tools > Actions >
/// Convert (the more deliberate gesture wins). With the defaults:
/// Shift → Convert, Shift+Alt → Tools, Control → Actions.
pub fn chord_for(held: HeldMods, cfg: &SensorConfig) -> Option<WheelKind> {
    let mut candidates: Vec<(u32, u8, WheelKind)> = Vec::new(); // (size, priority, kind)
    if held.has(cfg.modifier) {
        candidates.push((1, 0, WheelKind::Convert));
    }
    if cfg.tools_modifier != cfg.modifier && held.has(cfg.modifier) && held.has(cfg.tools_modifier) {
        candidates.push((2, 2, WheelKind::Tools));
    }
    if cfg.actions_modifier != cfg.modifier && held.has(cfg.actions_modifier) {
        candidates.push((1, 1, WheelKind::Actions));
    }
    candidates.sort_by_key(|(size, prio, _)| (*size, *prio));
    candidates.last().map(|(_, _, kind)| *kind)
}

// File-action segment ids (protocol constants: the sensors execute the
// two clipboard actions natively; the rest are the shell's to run).
pub const ACTION_COPY_FILE: &str = "copy-file";
pub const ACTION_COPY_PATH: &str = "copy-path";
pub const ACTION_MOVE: &str = "move";
pub const ACTION_REVEAL: &str = "reveal";
pub const ACTION_TERMINAL: &str = "terminal";
pub const ACTION_SHARE: &str = "share";

/// Percent-encode local paths into a `text/uri-list` payload
/// (CRLF-separated `file://` URIs) — the clipboard's file payload.
pub fn paths_to_uri_list(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for p in paths {
        out.push_str("file://");
        for b in p.to_string_lossy().bytes() {
            let keep = b.is_ascii_alphanumeric()
                || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/' | b':' | b'@' | b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=');
            if keep {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out.push_str("\r\n");
    }
    out
}

// ---------------------------------------------------------------------------
// Wheel model — what the host wants painted for a given drag
// ---------------------------------------------------------------------------

/// A request from a sensor to the host: "the user is dragging this —
/// what should the wheel offer?"
#[derive(Debug, Clone)]
pub struct WheelRequest {
    /// Which wheel the summon chord selected.
    pub kind: WheelKind,
    /// File paths when the platform could read them before the drop
    /// (X11 bystander peek, macOS drag pasteboard). Empty when the
    /// platform only learns paths at drop time (Windows pre-DragEnter).
    pub paths: Vec<PathBuf>,
    /// Drag content types as the platform reports them (MIME types on
    /// X11/Wayland, pasteboard types on macOS, clipboard formats on
    /// Windows) — the classifier input when `paths` is empty.
    pub mime_types: Vec<String>,
}

/// One wheel segment: a stable `id` (target extension for conversion,
/// tool id, action id) plus the label the renderer paints. Labels are
/// host-composed (localized for tools/actions, the extension itself
/// for conversions) — the renderer never transforms them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WheelSegment {
    pub id: String,
    pub label: String,
}

impl WheelSegment {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        WheelSegment {
            id: id.into(),
            label: label.into(),
        }
    }
}

/// What the host wants the wheel to show.
#[derive(Debug, Clone)]
pub struct WheelModel {
    pub kind: WheelKind,
    /// Segments in display order — one per sector.
    pub segments: Vec<WheelSegment>,
    /// Hub line 1: the dragged file's extension, e.g. "PNG"
    /// (or "7z" / "FILE" for a folder).
    pub hub_ext: String,
    /// Hub line 2: the file name, or a host-composed "N files" label.
    pub hub_title: String,
    /// How many files the drop will convert (0 = not yet known).
    pub file_count: usize,
}

/// Host callback: resolve a drag to a wheel model. Returning `None`
/// means "not a convertible file drag — do not summon (or un-summon)".
pub type WheelResolver = Arc<dyn Fn(&WheelRequest) -> Option<WheelModel> + Send + Sync>;

// ---------------------------------------------------------------------------
// Events (sensor → host) and commands (host → sensor)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum SensorEvent {
    /// A drag exists and was classified. Informational (the UI may show
    /// a tray state); conversion flows through `ConvertRequested`.
    DragClassified { mime_types: Vec<String> },
    /// The wheel was summoned (shown) at this root position.
    Summoned { x: i32, y: i32 },
    /// The user dropped on a segment: convert all `paths` to `target`.
    ConvertRequested { paths: Vec<PathBuf>, target: String },
    /// The user dropped on a TOOLS segment at root position (x, y):
    /// the host opens the tool's HUD panel (anchored there) or runs a
    /// panel-less tool (PDF merge) directly.
    ToolRequested { paths: Vec<PathBuf>, tool: String, x: i32, y: i32 },
    /// The user dropped on an ACTIONS segment the sensor does not
    /// execute itself (Move / Reveal / Terminal / Share). (x, y) is the
    /// drop point, for panels the action may open.
    ActionRequested { paths: Vec<PathBuf>, action: String, x: i32, y: i32 },
    /// A clipboard action (copy file / copy path) the sensor executed
    /// natively — the host records it / toasts as it sees fit.
    ActionDone { paths: Vec<PathBuf>, action: String },
    /// A HUD panel closed. `outcome` says whether the user applied.
    PanelClosed { id: String, outcome: PanelOutcome },
    /// The panel's canvas needs another page rendered (PDF redact):
    /// the host answers with `SensorCommand::UpdatePanelCanvas`.
    PanelPageRequest { id: String, page: u32 },
    /// The drag ended with no drop on the wheel (Esc, missed drop,
    /// source cancelled). Any summoned overlay is already dismissed.
    DragEnded,
    /// A platform notice the user should eventually see: permission
    /// missing, degraded tier active, sensor unavailable. `code` is a
    /// stable machine string; `message` is a human explanation.
    Notice { code: String, message: String },
}

// ---------------------------------------------------------------------------
// HUD panels — the floating tool panels (Crop is the reference model).
// The shared layout/state/renderer lives in [`crate::panel`]; platform
// modules only own a window and feed it pointer/key events.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum PanelField {
    /// Free text (numbers as text; the host validates).
    Text { key: String, label: String, value: String },
    /// Single-choice pill row; outcome = the selected option string.
    Chips { key: String, label: String, options: Vec<String>, selected: usize },
    /// On/off switch; outcome = "true" / "false".
    Toggle { key: String, label: String, on: bool },
    /// Read-only text block (metadata view). No outcome entry.
    Info { key: String, text: String },
}

impl PanelField {
    pub fn key(&self) -> &str {
        match self {
            PanelField::Text { key, .. }
            | PanelField::Chips { key, .. }
            | PanelField::Toggle { key, .. }
            | PanelField::Info { key, .. } => key,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasMode {
    /// Draw filled rectangles (PDF redact).
    Rects,
    /// Draw rectangles / arrows / text labels (image annotate).
    Annotate,
}

#[derive(Debug, Clone)]
pub struct CanvasSpec {
    /// Preview image (PNG file) the shell rendered for this panel.
    pub image_png: PathBuf,
    /// Coordinate space shapes are reported in: image pixels for
    /// annotate; PDF points (page size) for redact.
    pub coord_w: f32,
    pub coord_h: f32,
    pub mode: CanvasMode,
    /// 1-based current page and total pages (annotate: 1 of 1).
    pub page: u32,
    pub pages: u32,
    /// Localized labels for the annotate tool row [Rect, Arrow, Text].
    pub tool_labels: Vec<String>,
    /// Label of the annotate text-entry box.
    pub label_caption: String,
}

/// A shape drawn on a panel canvas, in CanvasSpec coordinate space.
#[derive(Debug, Clone, PartialEq)]
pub enum CanvasShape {
    Rect { x: f32, y: f32, w: f32, h: f32 },
    Arrow { x1: f32, y1: f32, x2: f32, y2: f32 },
    Text { x: f32, y: f32, text: String },
}

/// All shapes drawn on one canvas page.
#[derive(Debug, Clone, PartialEq)]
pub struct PageShapes {
    pub page: u32,
    pub shapes: Vec<CanvasShape>,
}

#[derive(Debug, Clone)]
pub struct PanelSpec {
    /// Host-chosen id, echoed back in `PanelClosed`.
    pub id: String,
    pub title: String,
    /// Root coordinates of the drop that asked for this panel; the
    /// platform anchors the panel near it (clamped to the screen).
    pub anchor: (i32, i32),
    pub fields: Vec<PanelField>,
    pub canvas: Option<CanvasSpec>,
    /// Apply button label; empty = no Apply button (view-only panel).
    pub apply_label: String,
    pub cancel_label: String,
    pub rtl: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PanelOutcome {
    Applied {
        /// Field outcomes by key: text content / selected chip option /
        /// "true"|"false" for toggles. Info fields are absent.
        values: HashMap<String, String>,
        /// Every canvas page's shapes (empty for form panels).
        pages: Vec<PageShapes>,
    },
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Success,
    Error,
}

/// A small floating result panel (native window). `title` and `body` are
/// host-composed in the user's language; the renderer shapes Arabic.
#[derive(Debug, Clone)]
pub struct Toast {
    pub kind: ToastKind,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub enum SensorCommand {
    ShowToast(Toast),
    /// Open a floating HUD panel (replaces any open panel, which is
    /// closed as Cancelled).
    ShowPanel(PanelSpec),
    /// Swap the open panel's canvas preview (answer to PanelPageRequest).
    UpdatePanelCanvas { id: String, image_png: PathBuf, page: u32, pages: u32 },
    /// Dismiss the open panel without an outcome event (host-side abort).
    ClosePanel { id: String },
    UpdateConfig(SensorConfig),
    Shutdown,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SensorError {
    pub code: String,
    pub message: String,
}

impl SensorError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        SensorError {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SensorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}
impl std::error::Error for SensorError {}

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

/// Everything a platform module must provide. `start` BLOCKS, running
/// the platform event loop on the caller's thread, until a
/// [`SensorCommand::Shutdown`] arrives or a fatal error occurs. Sensing
/// channels must be event-driven: idle cost ≈ 0% CPU.
pub trait DragSensor: Send {
    /// Stable platform id: "x11" | "macos" | "windows".
    fn platform(&self) -> &'static str;
    fn start(
        &mut self,
        config: SensorConfig,
        events: Sender<SensorEvent>,
        commands: Receiver<SensorCommand>,
        resolve: WheelResolver,
    ) -> Result<(), SensorError>;
}

/// Create the sensor for this OS/session, if one can run here.
/// Linux: the X11 sensor when an X display is reachable (under a Wayland
/// session that means XWayland — X apps are covered; native Wayland apps
/// are served by the orb tier in [`wayland`]).
pub fn create_sensor() -> Option<Box<dyn DragSensor>> {
    #[cfg(target_os = "macos")]
    {
        Some(Box::new(macos::MacSensor::new()))
    }
    #[cfg(target_os = "windows")]
    {
        Some(Box::new(windows::WinSensor::new()))
    }
    #[cfg(target_os = "linux")]
    {
        x11::create_sensor()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        None
    }
}

/// Enable/disable launch-at-login for the agent. Per-OS implementation:
/// macOS writes a LaunchAgent plist, Windows a `Run` registry value,
/// Linux an XDG autostart desktop entry.
pub fn set_autostart(enabled: bool) -> Result<(), SensorError> {
    #[cfg(target_os = "macos")]
    {
        macos::set_autostart(enabled)
    }
    #[cfg(target_os = "windows")]
    {
        windows::set_autostart(enabled)
    }
    #[cfg(target_os = "linux")]
    {
        linux_autostart(enabled)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = enabled;
        Err(SensorError::new(
            "unsupported",
            "autostart not supported on this OS",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(shift: bool, control: bool, alt: bool) -> HeldMods {
        HeldMods { shift, control, alt }
    }

    #[test]
    fn chord_defaults_match_the_reference_gestures() {
        let cfg = SensorConfig::default(); // Shift / Shift+Alt / Control
        assert_eq!(chord_for(held(true, false, false), &cfg), Some(WheelKind::Convert));
        assert_eq!(chord_for(held(true, false, true), &cfg), Some(WheelKind::Tools));
        assert_eq!(chord_for(held(false, true, false), &cfg), Some(WheelKind::Actions));
        assert_eq!(chord_for(held(false, false, false), &cfg), None);
        assert_eq!(chord_for(held(false, false, true), &cfg), None, "Alt alone summons nothing");
    }

    #[test]
    fn chord_largest_match_wins_then_priority() {
        let cfg = SensorConfig::default();
        // Shift+Control: two size-1 chords match; Actions outranks Convert.
        assert_eq!(chord_for(held(true, true, false), &cfg), Some(WheelKind::Actions));
        // All three: the size-2 Tools chord wins outright.
        assert_eq!(chord_for(held(true, true, true), &cfg), Some(WheelKind::Tools));
    }

    #[test]
    fn chord_config_is_respected() {
        let cfg = SensorConfig {
            modifier: SummonModifier::Control,
            tools_modifier: SummonModifier::Shift,
            actions_modifier: SummonModifier::Alt,
            ..SensorConfig::default()
        };
        assert_eq!(chord_for(held(false, true, false), &cfg), Some(WheelKind::Convert));
        assert_eq!(chord_for(held(true, true, false), &cfg), Some(WheelKind::Tools));
        assert_eq!(chord_for(held(false, false, true), &cfg), Some(WheelKind::Actions));
    }

    #[test]
    fn uri_list_percent_encodes() {
        let out = paths_to_uri_list(&[PathBuf::from("/tmp/a b/صورة.png")]);
        assert!(out.starts_with("file:///tmp/a%20b/"), "{out}");
        assert!(out.ends_with("\r\n"));
        assert!(!out.contains(' '), "no raw spaces in a URI list");
    }
}

#[cfg(target_os = "linux")]
fn linux_autostart(enabled: bool) -> Result<(), SensorError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| SensorError::new("io", "HOME is not set"))?;
    let dir = home.join(".config").join("autostart");
    let file = dir.join("uruk-convert.desktop");
    if !enabled {
        if file.exists() {
            std::fs::remove_file(&file).map_err(|e| {
                SensorError::new("io", format!("cannot remove autostart entry: {e}"))
            })?;
        }
        return Ok(());
    }
    let exe = std::env::current_exe()
        .map_err(|e| SensorError::new("io", format!("cannot locate own executable: {e}")))?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| SensorError::new("io", format!("cannot create autostart dir: {e}")))?;
    let body = format!(
        "[Desktop Entry]\nType=Application\nName=Uruk Convert\nComment=Uruk Convert background agent\nExec=\"{}\" --agent\nX-GNOME-Autostart-enabled=true\nNoDisplay=false\n",
        exe.to_string_lossy()
    );
    std::fs::write(&file, body)
        .map_err(|e| SensorError::new("io", format!("cannot write autostart entry: {e}")))?;
    Ok(())
}
