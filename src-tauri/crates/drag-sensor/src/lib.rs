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

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

pub mod arabic;
pub mod geom;
// The macOS module compiles on every OS: its pure helpers and unit
// tests run in Linux CI, and all OS FFI is cfg-gated inside the file.
pub mod macos;
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
    pub modifier: SummonModifier,
    pub lang: SensorLang,
    pub enabled: bool,
}

impl Default for SensorConfig {
    fn default() -> Self {
        SensorConfig {
            modifier: SummonModifier::Shift,
            lang: SensorLang::Ar,
            enabled: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Wheel model — what the host wants painted for a given drag
// ---------------------------------------------------------------------------

/// A request from a sensor to the host: "the user is dragging this —
/// what should the wheel offer?"
#[derive(Debug, Clone)]
pub struct WheelRequest {
    /// File paths when the platform could read them before the drop
    /// (X11 bystander peek, macOS drag pasteboard). Empty when the
    /// platform only learns paths at drop time (Windows pre-DragEnter).
    pub paths: Vec<PathBuf>,
    /// Drag content types as the platform reports them (MIME types on
    /// X11/Wayland, pasteboard types on macOS, clipboard formats on
    /// Windows) — the classifier input when `paths` is empty.
    pub mime_types: Vec<String>,
}

/// What the host wants the wheel to show.
#[derive(Debug, Clone)]
pub struct WheelModel {
    /// Target extensions (lowercase) in display order — one per segment.
    pub segments: Vec<String>,
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
    /// The drag ended with no drop on the wheel (Esc, missed drop,
    /// source cancelled). Any summoned overlay is already dismissed.
    DragEnded,
    /// A platform notice the user should eventually see: permission
    /// missing, degraded tier active, sensor unavailable. `code` is a
    /// stable machine string; `message` is a human explanation.
    Notice { code: String, message: String },
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
