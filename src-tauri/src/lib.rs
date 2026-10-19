//! Uruk Convert — Tauri shell (Stage 3: background agent + global wheel).
//!
//! The shell stays thin: conversion logic lives in `uruk-engine-core`;
//! drag sensing lives in `uruk-drag-sensor` behind a per-OS trait.
//!
//! Stage 3 shape:
//! - The app launches as a **background agent**: tray icon only, no main
//!   window. The window (Settings / History / fallback drop) is created on
//!   demand from the tray and hides instead of closing.
//! - A platform drag sensor (X11 full path; macOS/Windows modules; Wayland
//!   orb tier) runs on its own thread. When the user drops on the native
//!   wheel overlay, the sensor emits `ConvertRequested`; the shell runs the
//!   conversion through engine-core and answers with a toast on the sensor
//!   channel, recording the outcome in the persistent history.
//! - Overlay conversions honor the same output settings as window jobs:
//!   the webview pushes its output preferences into `agent settings`
//!   (Rust-persisted), which both paths read.
//! - History is Rust-side (`history.json` in the app data dir), capped,
//!   and shared by window jobs and overlay conversions.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};
use uruk_drag_sensor::{
    SensorCommand, SensorConfig, SensorEvent, SensorLang, SummonModifier, Toast, ToastKind,
    WheelModel, WheelRequest, WheelResolver,
};
use uruk_engine_core::{analyze, run_job, Family, FileInfo, JobSpec, Tools};

static QUITTING: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Agent settings (Rust-persisted; the webview mirrors its prefs here)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AgentSettings {
    /// "ar" | "en" — drives native toast language + tray labels.
    pub lang: String,
    /// "shift" | "control" | "alt" — the wheel summon modifier.
    pub modifier: String,
    pub autostart: bool,
    /// Master switch for the global drag sensor.
    pub sensing: bool,
    pub onboarding_done: bool,
    /// Window-job output prefs, mirrored so overlay conversions match.
    pub output_dir: Option<String>,
    /// "converted" or "" (no suffix).
    pub suffix: String,
    pub image_quality: u8,
}

impl Default for AgentSettings {
    fn default() -> Self {
        AgentSettings {
            lang: "ar".into(),
            modifier: "shift".into(),
            autostart: false,
            sensing: true,
            onboarding_done: false,
            output_dir: None,
            suffix: "converted".into(),
            image_quality: 92,
        }
    }
}

impl AgentSettings {
    fn sensor_config(&self) -> SensorConfig {
        SensorConfig {
            modifier: SummonModifier::from_code(&self.modifier),
            lang: SensorLang::from_code(&self.lang),
            enabled: self.sensing,
        }
    }
    fn is_ar(&self) -> bool {
        self.lang != "en"
    }
}

fn settings_path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|d| d.join("settings.json"))
}

fn load_settings_from_disk(app: &AppHandle) -> AgentSettings {
    if let Some(p) = settings_path(app) {
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Ok(s) = serde_json::from_str::<AgentSettings>(&text) {
                return s;
            }
        }
    }
    AgentSettings::default()
}

fn persist_settings(app: &AppHandle, s: &AgentSettings) {
    if let Some(p) = settings_path(app) {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(s) {
            let _ = std::fs::write(&p, text);
        }
    }
}

// ---------------------------------------------------------------------------
// History (Rust-persisted, capped)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HistoryRecord {
    /// Unix seconds.
    pub ts: u64,
    pub input_name: String,
    pub input_path: String,
    pub output_path: Option<String>,
    /// Target extension, or "compress" / "extract" for tool jobs.
    pub target: String,
    /// "done" | "error"
    pub status: String,
    pub error: Option<String>,
    /// "overlay" | "window"
    pub origin: String,
}

const HISTORY_CAP: usize = 300;

fn history_path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|d| d.join("history.json"))
}

fn read_history(app: &AppHandle) -> Vec<HistoryRecord> {
    if let Some(p) = history_path(app) {
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<Vec<HistoryRecord>>(&text) {
                return v;
            }
        }
    }
    Vec::new()
}

fn record_history(app: &AppHandle, rec: HistoryRecord) {
    let mut all = read_history(app);
    all.push(rec.clone());
    if all.len() > HISTORY_CAP {
        let skip = all.len() - HISTORY_CAP;
        all.drain(..skip);
    }
    match history_path(app) {
        Some(p) => {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match serde_json::to_string_pretty(&all) {
                Ok(text) => {
                    if let Err(e) = std::fs::write(&p, text) {
                        eprintln!("uruk-convert: history write failed ({}): {e}", p.display());
                    }
                }
                Err(e) => eprintln!("uruk-convert: history serialize failed: {e}"),
            }
        }
        None => eprintln!("uruk-convert: history path unavailable (no app data dir)"),
    }
    let _ = app.emit("history-updated", rec);
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn file_name_of(path: &str) -> String {
    PathBuf::from(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

struct AppState {
    tools: Mutex<Option<Tools>>,
    jobs: Mutex<HashMap<String, Arc<AtomicBool>>>,
    agent: Arc<Mutex<AgentSettings>>,
    sensor_cmd: Arc<Mutex<Option<mpsc::Sender<SensorCommand>>>>,
    /// (platform label, honest tier note) for the settings UI.
    platform_note: Arc<Mutex<(String, String)>>,
}

impl AppState {
    fn tools(&self, app: &AppHandle) -> Tools {
        let mut guard = self.tools.lock().unwrap();
        if let Some(t) = &*guard {
            return t.clone();
        }
        // Look for bundled sidecars next to the executable and in the
        // resource directory; engine-core also falls back to PATH in dev.
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Ok(res) = app.path().resource_dir() {
            dirs.push(res.join("binaries"));
            dirs.push(res);
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                dirs.push(parent.join("binaries"));
                dirs.push(parent.to_path_buf());
            }
        }
        let tools = Tools::resolve(&dirs);
        *guard = Some(tools.clone());
        tools
    }

    fn send_sensor(&self, cmd: SensorCommand) {
        if let Some(tx) = &*self.sensor_cmd.lock().unwrap() {
            let _ = tx.send(cmd);
        }
    }

    fn toast(&self, kind: ToastKind, title: String, body: String) {
        self.send_sensor(SensorCommand::ShowToast(Toast { kind, title, body }));
    }
}

// ---------------------------------------------------------------------------
// Window commands (Stage 1 surface — unchanged behavior)
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct ProgressPayload {
    id: String,
    percent: f32,
}

#[derive(Serialize, Clone)]
struct DonePayload {
    id: String,
    output: String,
    note: Option<String>,
}

#[derive(Serialize, Clone)]
struct ErrorPayload {
    id: String,
    code: String,
    message: String,
}

#[tauri::command]
fn analyze_paths(app: AppHandle, state: State<AppState>, paths: Vec<String>) -> Vec<FileInfo> {
    let tools = state.tools(&app);
    paths
        .iter()
        .map(|p| analyze(&tools, &PathBuf::from(p)))
        .collect()
}

#[tauri::command]
fn tools_report(app: AppHandle, state: State<AppState>) -> HashMap<String, String> {
    let tools = state.tools(&app);
    uruk_engine_core::tool_report(&tools)
}

fn job_history_fields(job: &JobSpec) -> (String, String) {
    match job {
        JobSpec::Convert { input, target, .. } => (input.clone(), target.clone()),
        JobSpec::CompressImage { input, .. } => (input.clone(), "compress".into()),
        JobSpec::ExtractArchive { input, .. } => (input.clone(), "extract".into()),
        JobSpec::CreateArchive { inputs, format, .. } => {
            (inputs.first().cloned().unwrap_or_default(), format.clone())
        }
    }
}

#[tauri::command]
fn start_job(app: AppHandle, state: State<AppState>, id: String, job: JobSpec) {
    let tools = state.tools(&app);
    let (hist_input, hist_target) = job_history_fields(&job);
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut jobs = state.jobs.lock().unwrap();
        jobs.insert(id.clone(), cancel.clone());
    }
    let app2 = app.clone();
    std::thread::spawn(move || {
        let mut last_sent = -2.0f32;
        let result = run_job(&tools, &job, &cancel, &mut |p| {
            if (p - last_sent).abs() > 0.4 || p >= 100.0 {
                last_sent = p;
                let _ = app2.emit(
                    "job-progress",
                    ProgressPayload {
                        id: id.clone(),
                        percent: p,
                    },
                );
            }
        });
        match result {
            Ok(outcome) => {
                record_history(
                    &app2,
                    HistoryRecord {
                        ts: now_secs(),
                        input_name: file_name_of(&hist_input),
                        input_path: hist_input.clone(),
                        output_path: Some(outcome.output.to_string_lossy().into_owned()),
                        target: hist_target.clone(),
                        status: "done".into(),
                        error: None,
                        origin: "window".into(),
                    },
                );
                let _ = app2.emit(
                    "job-done",
                    DonePayload {
                        id: id.clone(),
                        output: outcome.output.to_string_lossy().into_owned(),
                        note: outcome.note,
                    },
                );
            }
            Err(e) => {
                record_history(
                    &app2,
                    HistoryRecord {
                        ts: now_secs(),
                        input_name: file_name_of(&hist_input),
                        input_path: hist_input.clone(),
                        output_path: None,
                        target: hist_target.clone(),
                        status: "error".into(),
                        error: Some(e.message.clone()),
                        origin: "window".into(),
                    },
                );
                let _ = app2.emit(
                    "job-error",
                    ErrorPayload {
                        id: id.clone(),
                        code: e.code,
                        message: e.message,
                    },
                );
            }
        }
        if let Some(state) = app2.try_state::<AppState>() {
            let mut jobs = state.jobs.lock().unwrap();
            jobs.remove(&id);
        }
    });
}

#[tauri::command]
fn cancel_job(state: State<AppState>, id: String) {
    let jobs = state.jobs.lock().unwrap();
    if let Some(flag) = jobs.get(&id) {
        flag.store(true, Ordering::Relaxed);
    }
}

/// File action: show a file in the OS file manager (Explorer / Finder /
/// Files), or open the folder itself.
#[tauri::command]
fn reveal_in_folder(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    #[cfg(target_os = "windows")]
    {
        let mut cmd = std::process::Command::new("explorer");
        if p.is_dir() {
            cmd.arg(&path);
        } else {
            cmd.arg(format!("/select,{path}"));
        }
        cmd.spawn().map_err(|e| e.to_string())?;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let mut cmd = std::process::Command::new("open");
        if p.is_dir() {
            cmd.arg(&path);
        } else {
            cmd.args(["-R", &path]);
        }
        cmd.spawn().map_err(|e| e.to_string())?;
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = if p.is_dir() {
            p.clone()
        } else {
            p.parent().map(|d| d.to_path_buf()).unwrap_or(p.clone())
        };
        std::process::Command::new("xdg-open")
            .arg(dir)
            .spawn()
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    #[allow(unreachable_code)]
    Ok(())
}

/// File action: open a file/folder with the OS default application.
#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    std::process::Command::new("cmd")
        .args(["/C", "start", "", &path])
        .spawn()
        .map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    std::process::Command::new("open")
        .arg(&path)
        .spawn()
        .map_err(|e| e.to_string())?;
    #[cfg(all(unix, not(target_os = "macos")))]
    std::process::Command::new("xdg-open")
        .arg(&path)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Stage 3 commands: agent settings, history, sensor status
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_agent_settings(state: State<AppState>) -> AgentSettings {
    state.agent.lock().unwrap().clone()
}

#[tauri::command]
fn set_agent_settings(app: AppHandle, state: State<AppState>, settings: AgentSettings) {
    let (old_autostart,) = {
        let mut guard = state.agent.lock().unwrap();
        let old = guard.autostart;
        *guard = settings.clone();
        (old,)
    };
    persist_settings(&app, &settings);
    state.send_sensor(SensorCommand::UpdateConfig(settings.sensor_config()));
    if settings.autostart != old_autostart {
        if let Err(e) = uruk_drag_sensor::set_autostart(settings.autostart) {
            eprintln!("uruk-convert: autostart update failed: {}", e.message);
        }
    }
}

#[tauri::command]
fn history_list(app: AppHandle) -> Vec<HistoryRecord> {
    let mut all = read_history(&app);
    all.reverse();
    all
}

#[derive(Serialize, Clone)]
struct SensorStatus {
    platform: String,
    note: String,
}

#[tauri::command]
fn sensor_status(state: State<AppState>) -> SensorStatus {
    let (platform, note) = state.platform_note.lock().unwrap().clone();
    SensorStatus { platform, note }
}

// ---------------------------------------------------------------------------
// Overlay conversion dispatch
// ---------------------------------------------------------------------------

/// Mirror of the window UI's launch logic for a wheel drop.
fn overlay_job(info: &FileInfo, target: &str, s: &AgentSettings) -> JobSpec {
    if info.is_dir || info.family == Family::Unknown {
        JobSpec::CreateArchive {
            inputs: vec![info.path.clone()],
            format: target.to_string(),
            output_dir: s.output_dir.clone(),
            suffix: s.suffix.clone(),
        }
    } else {
        JobSpec::Convert {
            input: info.path.clone(),
            target: target.to_string(),
            output_dir: s.output_dir.clone(),
            suffix: s.suffix.clone(),
            image_quality: s.image_quality,
        }
    }
}

fn run_overlay_conversions(app: &AppHandle, paths: Vec<PathBuf>, target: String) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let tools = state.tools(app);
    let settings = state.agent.lock().unwrap().clone();
    let ar = settings.is_ar();
    let mut ok_count = 0usize;
    let mut first_output: Option<String> = None;
    let mut first_error: Option<String> = None;
    for path in &paths {
        let input_s = path.to_string_lossy().into_owned();
        let info = analyze(&tools, path);
        let job = overlay_job(&info, &target, &settings);
        let cancel = Arc::new(AtomicBool::new(false));
        match run_job(&tools, &job, &cancel, &mut |_| {}) {
            Ok(outcome) => {
                ok_count += 1;
                let out_s = outcome.output.to_string_lossy().into_owned();
                if first_output.is_none() {
                    first_output = Some(file_name_of(&out_s));
                }
                record_history(
                    app,
                    HistoryRecord {
                        ts: now_secs(),
                        input_name: file_name_of(&input_s),
                        input_path: input_s,
                        output_path: Some(out_s),
                        target: target.clone(),
                        status: "done".into(),
                        error: None,
                        origin: "overlay".into(),
                    },
                );
            }
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e.message.clone());
                }
                record_history(
                    app,
                    HistoryRecord {
                        ts: now_secs(),
                        input_name: file_name_of(&input_s),
                        input_path: input_s,
                        output_path: None,
                        target: target.clone(),
                        status: "error".into(),
                        error: Some(e.message),
                        origin: "overlay".into(),
                    },
                );
            }
        }
    }
    // One summary toast for the whole drop.
    if ok_count == paths.len() {
        let (title, body) = if paths.len() == 1 {
            (
                if ar {
                    "تم التحويل"
                } else {
                    "Converted"
                }
                .to_string(),
                first_output.unwrap_or_default(),
            )
        } else if ar {
            (
                format!("تم تحويل {} ملفات", paths.len()),
                first_output.unwrap_or_default(),
            )
        } else {
            (
                format!("Converted {} files", paths.len()),
                first_output.unwrap_or_default(),
            )
        };
        state.toast(ToastKind::Success, title, body);
    } else {
        let title = if ar {
            "تعذّر التحويل"
        } else {
            "Conversion failed"
        }
        .to_string();
        let body = first_error.unwrap_or_default();
        state.toast(ToastKind::Error, title, body);
    }
}

fn handle_sensor_event(app: &AppHandle, ev: SensorEvent) {
    match ev {
        SensorEvent::ConvertRequested { paths, target } => {
            let app = app.clone();
            std::thread::spawn(move || run_overlay_conversions(&app, paths, target));
        }
        SensorEvent::Notice { code, message } => {
            if code == "permission_missing" {
                if let Some(state) = app.try_state::<AppState>() {
                    let ar = state.agent.lock().unwrap().is_ar();
                    let (title, body) = if ar {
                        (
                            "يلزم إذن إمكانية الوصول".to_string(),
                            "فعّل «محوّل أوروك» في إعدادات النظام ← الخصوصية والأمان ← إمكانية الوصول لتعمل عجلة السحب.".to_string(),
                        )
                    } else {
                        (
                            "Accessibility permission needed".to_string(),
                            "Enable Uruk Convert in System Settings → Privacy & Security → Accessibility so the drag wheel can work.".to_string(),
                        )
                    };
                    state.toast(ToastKind::Error, title, body);
                }
            } else {
                eprintln!("uruk-convert sensor notice [{code}]: {message}");
            }
        }
        SensorEvent::DragClassified { .. }
        | SensorEvent::Summoned { .. }
        | SensorEvent::DragEnded => {}
    }
}

// ---------------------------------------------------------------------------
// Window / tray
// ---------------------------------------------------------------------------

fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("Uruk Convert — محوّل أوروك")
        .inner_size(1120.0, 780.0)
        .min_inner_size(420.0, 560.0);
    match builder.build() {
        Ok(w) => {
            let _ = w.set_focus();
        }
        Err(e) => eprintln!("uruk-convert: cannot create main window: {e}"),
    }
}

fn build_tray(app: &AppHandle, ar: bool) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    let open_item = MenuItem::with_id(
        app,
        "uruk-open",
        if ar {
            "فتح محوّل أوروك"
        } else {
            "Open Uruk Convert"
        },
        true,
        None::<&str>,
    )?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit_item = MenuItem::with_id(
        app,
        "uruk-quit",
        if ar { "خروج" } else { "Quit" },
        true,
        None::<&str>,
    )?;
    let menu = Menu::with_items(app, &[&open_item, &sep, &quit_item])?;
    let mut builder = TrayIconBuilder::with_id("uruk-tray")
        .menu(&menu)
        .tooltip("Uruk Convert — محوّل أوروك")
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "uruk-open" => show_main_window(app),
            "uruk-quit" => {
                QUITTING.store(true, Ordering::Relaxed);
                if let Some(state) = app.try_state::<AppState>() {
                    state.send_sensor(SensorCommand::Shutdown);
                }
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Sensor / orb startup
// ---------------------------------------------------------------------------

fn is_wayland_session() -> bool {
    std::env::var("XDG_SESSION_TYPE")
        .map(|v| v == "wayland")
        .unwrap_or(false)
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

fn start_sensor(app: &AppHandle, state: &AppState) {
    let settings = state.agent.lock().unwrap().clone();
    if is_wayland_session() {
        start_wayland_tier(app, state, &settings);
        return;
    }
    let Some(mut sensor) = uruk_drag_sensor::create_sensor() else {
        let mut note = state.platform_note.lock().unwrap();
        *note = (
            "unavailable".into(),
            "No drag sensor could be created for this session.".into(),
        );
        return;
    };
    let platform = sensor.platform().to_string();
    {
        let mut note = state.platform_note.lock().unwrap();
        *note = (platform.clone(), String::new());
    }
    let (ev_tx, ev_rx) = mpsc::channel::<SensorEvent>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<SensorCommand>();
    *state.sensor_cmd.lock().unwrap() = Some(cmd_tx);
    let tools = state.tools(app);
    let resolver: WheelResolver = Arc::new(move |req: &WheelRequest| {
        let first = req.paths.first()?;
        let info = analyze(&tools, first);
        if info.targets.is_empty() {
            return None;
        }
        let hub_ext = if !info.ext.is_empty() {
            info.ext.to_uppercase()
        } else if info.is_dir {
            "DIR".into()
        } else {
            "FILE".into()
        };
        Some(WheelModel {
            segments: info.targets.clone(),
            hub_ext,
            hub_title: info.name.clone(),
            file_count: req.paths.len(),
        })
    });
    let config = settings.sensor_config();
    std::thread::Builder::new()
        .name("uruk-drag-sensor".into())
        .spawn(move || {
            if let Err(e) = sensor.start(config, ev_tx, cmd_rx, resolver) {
                eprintln!(
                    "uruk-convert: drag sensor stopped: [{}] {}",
                    e.code, e.message
                );
            }
        })
        .ok();
    let app2 = app.clone();
    std::thread::Builder::new()
        .name("uruk-sensor-events".into())
        .spawn(move || {
            while let Ok(ev) = ev_rx.recv() {
                handle_sensor_event(&app2, ev);
            }
        })
        .ok();
    eprintln!("uruk-convert: drag sensor running ({platform})");
}

fn start_wayland_tier(app: &AppHandle, state: &AppState, settings: &AgentSettings) {
    use uruk_drag_sensor::wayland::{self, OrbEvent, WaylandTier};
    let info = wayland::probe_tier();
    let lang = SensorLang::from_code(&settings.lang);
    let explanation = wayland::tier_explanation(&info, lang);
    {
        let mut note = state.platform_note.lock().unwrap();
        *note = (
            match info.tier {
                WaylandTier::Orb => "wayland-orb".into(),
                WaylandTier::MenuOnly => "wayland-fallback".into(),
            },
            explanation,
        );
    }
    if info.tier != WaylandTier::Orb {
        return;
    }
    let (tx, rx) = mpsc::channel::<OrbEvent>();
    match wayland::start_orb(tx) {
        Ok(_handle) => {
            // The orb thread lives for the app's lifetime; the handle is
            // intentionally leaked (stopping happens at process exit).
            std::mem::forget(_handle);
            let app2 = app.clone();
            std::thread::Builder::new()
                .name("uruk-orb-events".into())
                .spawn(move || {
                    while let Ok(OrbEvent::Dropped { paths }) = rx.recv() {
                        show_main_window(&app2);
                        let strs: Vec<String> = paths
                            .iter()
                            .map(|p| p.to_string_lossy().into_owned())
                            .collect();
                        let _ = app2.emit("orb-drop", strs);
                    }
                })
                .ok();
            eprintln!("uruk-convert: wayland orb tier running");
        }
        Err(e) => {
            eprintln!("uruk-convert: wayland orb failed to start: {}", e.message);
        }
    }
}

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

pub fn run() {
    let agent = Arc::new(Mutex::new(AgentSettings::default()));
    let sensor_cmd: Arc<Mutex<Option<mpsc::Sender<SensorCommand>>>> = Arc::new(Mutex::new(None));
    let platform_note: Arc<Mutex<(String, String)>> =
        Arc::new(Mutex::new(("starting".into(), String::new())));
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            tools: Mutex::new(None),
            jobs: Mutex::new(HashMap::new()),
            agent: agent.clone(),
            sensor_cmd: sensor_cmd.clone(),
            platform_note: platform_note.clone(),
        })
        .on_window_event(|window, event| {
            // The main window is a settings/history/fallback surface:
            // closing hides it; the agent keeps running in the tray.
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .setup(move |app| {
            let handle = app.handle().clone();
            // Agent mode: on macOS a background agent belongs in the
            // menu bar, not the Dock.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            // Persisted agent settings (loaded before the sensor starts so
            // the summon modifier/language are correct from the first drag).
            let settings = load_settings_from_disk(&handle);
            let first_run = !settings.onboarding_done;
            let ar = settings.is_ar();
            *agent.lock().unwrap() = settings;
            if let Some(state) = handle.try_state::<AppState>() {
                start_sensor(&handle, &state);
            }
            // Tray: failure must never kill the agent (headless sessions
            // without a tray host still convert via the fallback window).
            if let Err(e) = build_tray(&handle, ar) {
                eprintln!("uruk-convert: tray unavailable: {e}");
            }
            if first_run {
                show_main_window(&handle);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            analyze_paths,
            tools_report,
            start_job,
            cancel_job,
            reveal_in_folder,
            open_path,
            get_agent_settings,
            set_agent_settings,
            history_list,
            sensor_status
        ])
        .build(tauri::generate_context!())
        .expect("error while building Uruk Convert");
    app.run(|_handle, event| {
        if let tauri::RunEvent::ExitRequested { api, .. } = event {
            if !QUITTING.load(Ordering::Relaxed) {
                api.prevent_exit();
            }
        }
    });
}
