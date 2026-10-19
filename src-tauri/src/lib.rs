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
    CanvasMode, CanvasShape, CanvasSpec, PanelField, PanelOutcome, PanelSpec, SensorCommand,
    SensorConfig, SensorEvent, SensorLang, SummonModifier, Toast, ToastKind, WheelKind,
    WheelModel, WheelRequest, WheelResolver, WheelSegment,
};
use uruk_engine_core::{
    analyze, read_metadata, run_job, tool_ids_for, DrawShape, Family, FileInfo, JobSpec, PdfRegion,
    ToolJob, Tools,
};

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
    /// Extra key held with `modifier` to summon the TOOLS wheel.
    #[serde(default = "default_tools_modifier")]
    pub tools_modifier: String,
    /// Key that alone summons the ACTIONS wheel.
    #[serde(default = "default_actions_modifier")]
    pub actions_modifier: String,
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

fn default_tools_modifier() -> String {
    "alt".into()
}
fn default_actions_modifier() -> String {
    "control".into()
}

impl Default for AgentSettings {
    fn default() -> Self {
        AgentSettings {
            lang: "ar".into(),
            modifier: "shift".into(),
            tools_modifier: default_tools_modifier(),
            actions_modifier: default_actions_modifier(),
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
            tools_modifier: SummonModifier::from_code(&self.tools_modifier),
            actions_modifier: SummonModifier::from_code(&self.actions_modifier),
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

/// What an open HUD panel is for — remembered shell-side so a
/// `PanelClosed` outcome can be turned into an engine job.
enum PanelCtx {
    /// A tools-wheel tool panel for these paths.
    Tool { tool: String, paths: Vec<PathBuf> },
    /// The Move-to action's destination panel.
    Move { paths: Vec<PathBuf> },
}

struct AppState {
    tools: Mutex<Option<Tools>>,
    jobs: Mutex<HashMap<String, Arc<AtomicBool>>>,
    agent: Arc<Mutex<AgentSettings>>,
    sensor_cmd: Arc<Mutex<Option<mpsc::Sender<SensorCommand>>>>,
    /// (platform label, honest tier note) for the settings UI.
    platform_note: Arc<Mutex<(String, String)>>,
    /// Open HUD panels by id.
    panels: Mutex<HashMap<String, PanelCtx>>,
    panel_seq: std::sync::atomic::AtomicU64,
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

/// History "target" label for a tool job = the tool's wheel id.
fn tool_job_id(tool: &ToolJob) -> &'static str {
    match tool {
        ToolJob::ImageCrop { .. } => uruk_engine_core::TOOL_CROP,
        ToolJob::ImageResize { .. } => uruk_engine_core::TOOL_RESIZE,
        ToolJob::ImageTransform { .. } => uruk_engine_core::TOOL_TRANSFORM,
        ToolJob::ImageBackground { .. } => uruk_engine_core::TOOL_BACKGROUND,
        ToolJob::ImageStripMetadata => uruk_engine_core::TOOL_METADATA,
        ToolJob::ImageAnnotate { .. } => uruk_engine_core::TOOL_ANNOTATE,
        ToolJob::MediaTrim { .. } => uruk_engine_core::TOOL_TRIM,
        ToolJob::PdfSplit { .. } => uruk_engine_core::TOOL_PDF_SPLIT,
        ToolJob::PdfMerge { .. } => uruk_engine_core::TOOL_PDF_MERGE,
        ToolJob::PdfExtract { .. } => uruk_engine_core::TOOL_PDF_EXTRACT,
        ToolJob::PdfRotate { .. } => uruk_engine_core::TOOL_PDF_ROTATE,
        ToolJob::PdfCompress { .. } => uruk_engine_core::TOOL_PDF_COMPRESS,
        ToolJob::PdfRedact { .. } => uruk_engine_core::TOOL_PDF_REDACT,
    }
}

fn job_history_fields(job: &JobSpec) -> (String, String) {
    match job {
        JobSpec::Convert { input, target, .. } => (input.clone(), target.clone()),
        JobSpec::CompressImage { input, .. } => (input.clone(), "compress".into()),
        JobSpec::ExtractArchive { input, .. } => (input.clone(), "extract".into()),
        JobSpec::CreateArchive { inputs, format, .. } => {
            (inputs.first().cloned().unwrap_or_default(), format.clone())
        }
        JobSpec::Tool { input, tool, .. } => (input.clone(), tool_job_id(tool).to_string()),
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

// ---------------------------------------------------------------------------
// Stage 4: tools wheel + actions wheel + HUD panels
// ---------------------------------------------------------------------------

fn tool_label(id: &str, ar: bool) -> String {
    let (a, e) = match id {
        "crop" => ("قصّ", "Crop"),
        "resize" => ("تغيير الحجم", "Resize"),
        "transform" => ("تدوير وقلب", "Rotate & Flip"),
        "background" => ("إضافة خلفية", "Add Background"),
        "annotate" => ("رسم وتعليق", "Annotate"),
        "metadata" => ("البيانات الوصفية", "Metadata"),
        "compress" => ("ضغط", "Compress"),
        "trim" => ("قصّ المقطع", "Trim"),
        "split" => ("تقسيم", "Split"),
        "merge" => ("دمج", "Merge"),
        "extract-pages" => ("استخراج الصفحات", "Extract Pages"),
        "rotate-pdf" => ("تدوير الصفحات", "Rotate Pages"),
        "compress-pdf" => ("ضغط PDF", "Compress PDF"),
        "redact" => ("حجب نص", "Redact"),
        _ => (id, id),
    };
    if ar { a } else { e }.to_string()
}

fn action_label(id: &str, ar: bool) -> String {
    let (a, e) = match id {
        "copy-file" => ("نسخ الملف", "Copy File"),
        "copy-path" => ("نسخ المسار", "Copy Path"),
        "move" => ("نقل إلى…", "Move to…"),
        "reveal" => ("إظهار في المدير", "Reveal in Manager"),
        "terminal" => ("طرفية هنا", "Terminal Here"),
        "share" => ("مشاركة", "Share"),
        _ => (id, id),
    };
    if ar { a } else { e }.to_string()
}

/// The actions wheel's segments on this platform. Share exists only
/// where a native system share sheet is wired (macOS); PARITY.md
/// documents why it is absent elsewhere.
fn action_ids() -> Vec<&'static str> {
    let mut v = vec!["copy-file", "copy-path", "move", "reveal", "terminal"];
    if cfg!(target_os = "macos") {
        v.push("share");
    }
    v
}

fn dims_of(info: &FileInfo) -> Option<(u32, u32)> {
    let d = info.dimensions.as_deref()?;
    let (a, b) = d.split_once('x')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

fn pdf_info_of(tools: &Tools, path: &PathBuf) -> (u32, Option<(f32, f32)>) {
    let mut pages = 0u32;
    let mut size = None;
    if let Ok(entries) = read_metadata(tools, path) {
        for (k, v) in entries {
            if k == "Pages" {
                pages = v.parse().unwrap_or(0);
            } else if k == "Page size" {
                let toks: Vec<&str> = v.split_whitespace().collect();
                if toks.len() >= 3 {
                    if let (Ok(w), Ok(h)) = (toks[0].parse::<f32>(), toks[2].parse::<f32>()) {
                        size = Some((w, h));
                    }
                }
            }
        }
    }
    (pages, size)
}

fn panel_temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("uruk-convert-panels");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn cleanup_panel_files(id: &str) {
    if let Ok(rd) = std::fs::read_dir(panel_temp_dir()) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with(id) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Render one PDF page to a PNG preview for a panel canvas.
/// Returns the page size in points.
fn render_pdf_page_preview(
    tools: &Tools,
    pdf: &PathBuf,
    page: u32,
    out_png: &PathBuf,
) -> Result<(f32, f32), String> {
    let pdftoppm = tools
        .pdftoppm
        .clone()
        .ok_or("poppler (pdftoppm) is not available".to_string())?;
    let prefix = out_png.with_extension("");
    let status = std::process::Command::new(&pdftoppm)
        .args([
            "-f",
            &page.to_string(),
            "-l",
            &page.to_string(),
            "-r",
            "150",
            "-png",
            "-singlefile",
            &pdf.to_string_lossy(),
            &prefix.to_string_lossy(),
        ])
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() || !out_png.is_file() {
        return Err("could not render the page preview".into());
    }
    let (_, size) = pdf_info_of(tools, pdf);
    size.ok_or_else(|| "could not read the page size".into())
}

/// Scale an image into a PNG preview for the annotate canvas.
fn render_image_preview(tools: &Tools, input: &PathBuf, out_png: &PathBuf) -> Result<(), String> {
    let (magick, _) = tools
        .magick_argv()
        .ok_or("ImageMagick is not available".to_string())?;
    let status = std::process::Command::new(&magick)
        .args([
            input.to_string_lossy().as_ref(),
            "-resize",
            "1040x1040>",
            out_png.to_string_lossy().as_ref(),
        ])
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() || !out_png.is_file() {
        return Err("could not render the image preview".into());
    }
    Ok(())
}

fn next_panel_id(state: &AppState) -> String {
    let n = state
        .panel_seq
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("panel-{n}")
}

fn open_panel(app: &AppHandle, id: String, ctx: PanelCtx, mut spec: PanelSpec) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    spec.id = id.clone();
    state.panels.lock().unwrap().insert(id, ctx);
    state.send_sensor(SensorCommand::ShowPanel(spec));
}

fn err_toast(app: &AppHandle, ar: bool, msg: &str) {
    if let Some(state) = app.try_state::<AppState>() {
        state.toast(
            ToastKind::Error,
            if ar { "تعذّر التنفيذ".into() } else { "Could not do that".into() },
            msg.to_string(),
        );
    }
}

/// Build the HUD panel spec for a tool (None = the tool cannot run
/// here, with the reason already toasted).
fn build_tool_panel(
    app: &AppHandle,
    id: &str,
    tools: &Tools,
    info: &FileInfo,
    tool: &str,
    anchor: (i32, i32),
    ar: bool,
) -> Option<PanelSpec> {
    let first = PathBuf::from(&info.path);
    let mut spec = PanelSpec {
        id: id.to_string(),
        title: tool_label(tool, ar),
        anchor,
        fields: Vec::new(),
        canvas: None,
        apply_label: if ar { "تطبيق".into() } else { "Apply".into() },
        cancel_label: if ar { "إلغاء".into() } else { "Cancel".into() },
        rtl: ar,
    };
    let text = |key: &str, label: &str, value: String| PanelField::Text {
        key: key.into(),
        label: label.into(),
        value,
    };
    let chips = |key: &str, label: &str, options: Vec<&str>, selected: usize| PanelField::Chips {
        key: key.into(),
        label: label.into(),
        options: options.iter().map(|s| s.to_string()).collect(),
        selected,
    };
    let toggle = |key: &str, label: &str, on: bool| PanelField::Toggle {
        key: key.into(),
        label: label.into(),
        on,
    };
    let info_field = |text_s: String| PanelField::Info {
        key: "note".into(),
        text: text_s,
    };
    match tool {
        "crop" => {
            let (w, h) = dims_of(info).unwrap_or((0, 0));
            spec.fields = vec![
                text("x", "X", "0".into()),
                text("y", "Y", "0".into()),
                text("w", if ar { "العرض" } else { "Width" }, w.to_string()),
                text("h", if ar { "الارتفاع" } else { "Height" }, h.to_string()),
                chips(
                    "aspect",
                    if ar { "النسبة" } else { "Aspect" },
                    vec!["Free", "1:1", "4:3", "3:2", "16:9"],
                    0,
                ),
            ];
        }
        "resize" => {
            let (w, h) = dims_of(info).unwrap_or((0, 0));
            spec.fields = vec![
                text("w", if ar { "العرض" } else { "Width" }, w.to_string()),
                text("h", if ar { "الارتفاع" } else { "Height" }, h.to_string()),
                toggle("keep", if ar { "الحفاظ على النسبة" } else { "Keep aspect ratio" }, true),
            ];
        }
        "transform" => {
            spec.fields = vec![
                chips(
                    "rotate",
                    if ar { "التدوير" } else { "Rotate" },
                    vec!["0°", "90°", "180°", "270°"],
                    0,
                ),
                toggle("flip_h", if ar { "قلب أفقي" } else { "Flip horizontal" }, false),
                toggle("flip_v", if ar { "قلب عمودي" } else { "Flip vertical" }, false),
            ];
        }
        "background" => {
            spec.fields = vec![chips(
                "color",
                if ar { "لون الخلفية" } else { "Background color" },
                vec!["#FFFFFF", "#000000", "#C9A227", "#2A6FDB", "#D23C28", "#3E8853"],
                0,
            )];
        }
        "metadata" => {
            match read_metadata(tools, &first) {
                Ok(entries) => {
                    let total = entries.len();
                    for (i, (k, v)) in entries.iter().take(16).enumerate() {
                        spec.fields.push(PanelField::Info {
                            key: format!("m{i}"),
                            text: format!("{k}: {v}"),
                        });
                    }
                    if total > 16 {
                        spec.fields.push(info_field(if ar {
                            format!("+ {} إدخالًا آخر", total - 16)
                        } else {
                            format!("+ {} more entries", total - 16)
                        }));
                    }
                    // Strip is offered for images only (the engine's
                    // proven -strip route); other families view-only.
                    if info.family != Family::Image {
                        spec.apply_label = String::new();
                    } else {
                        spec.apply_label = if ar {
                            "حذف كل البيانات".into()
                        } else {
                            "Strip all metadata".into()
                        };
                    }
                }
                Err(e) => {
                    err_toast(app, ar, &e.message);
                    return None;
                }
            }
        }
        "annotate" => {
            let (w, h) = dims_of(info).unwrap_or((0, 0));
            if w == 0 {
                err_toast(app, ar, "image size unknown");
                return None;
            }
            let preview = panel_temp_dir().join(format!("{id}-preview.png"));
            if let Err(e) = render_image_preview(tools, &first, &preview) {
                err_toast(app, ar, &e);
                return None;
            }
            spec.canvas = Some(CanvasSpec {
                image_png: preview,
                coord_w: w as f32,
                coord_h: h as f32,
                mode: CanvasMode::Annotate,
                page: 1,
                pages: 1,
                tool_labels: if ar {
                    vec!["مستطيل".into(), "سهم".into(), "نص".into()]
                } else {
                    vec!["Rect".into(), "Arrow".into(), "Text".into()]
                },
                label_caption: if ar { "نص التعليق" } else { "Label text" }.into(),
            });
        }
        "compress" => {
            spec.fields = vec![chips(
                "percent",
                if ar { "تقليل الحجم %" } else { "Reduce size by %" },
                vec!["25", "50", "75", "90"],
                1,
            )];
        }
        "trim" => {
            let dur = info.duration.unwrap_or(0.0);
            spec.fields = vec![
                text("start", if ar { "البداية (ث)" } else { "Start (s)" }, "0".into()),
                text(
                    "end",
                    if ar { "النهاية (ث)" } else { "End (s)" },
                    format!("{dur:.1}"),
                ),
                info_field(if ar {
                    format!("المدة الكلية: {dur:.1} ثانية")
                } else {
                    format!("Total duration: {dur:.1} s")
                }),
            ];
        }
        "split" | "extract-pages" => {
            let (pages, _) = pdf_info_of(tools, &first);
            spec.fields = vec![
                text(
                    "ranges",
                    if ar { "الصفحات (مثال 1-3،5)" } else { "Pages (e.g. 1-3,5)" },
                    format!("1-{pages}"),
                ),
                info_field(if ar {
                    format!("عدد الصفحات: {pages}")
                } else {
                    format!("Pages: {pages}")
                }),
            ];
        }
        "rotate-pdf" => {
            spec.fields = vec![chips(
                "angle",
                if ar { "زاوية التدوير" } else { "Rotation" },
                vec!["90°", "180°", "270°"],
                0,
            )];
        }
        "compress-pdf" => {
            spec.fields = vec![
                chips(
                    "tier",
                    if ar { "مستوى الضغط" } else { "Quality tier" },
                    vec!["screen", "ebook", "printer"],
                    1,
                ),
                info_field(if ar {
                    "screen = الأصغر، printer = الأوضح".into()
                } else {
                    "screen = smallest, printer = sharpest".into()
                }),
            ];
        }
        "redact" => {
            let (pages, _) = pdf_info_of(tools, &first);
            if pages == 0 {
                err_toast(app, ar, "page count unknown");
                return None;
            }
            let preview = panel_temp_dir().join(format!("{id}-page-1.png"));
            match render_pdf_page_preview(tools, &first, 1, &preview) {
                Ok((pw, ph)) => {
                    spec.canvas = Some(CanvasSpec {
                        image_png: preview,
                        coord_w: pw,
                        coord_h: ph,
                        mode: CanvasMode::Rects,
                        page: 1,
                        pages,
                        tool_labels: Vec::new(),
                        label_caption: String::new(),
                    });
                }
                Err(e) => {
                    err_toast(app, ar, &e);
                    return None;
                }
            }
        }
        _ => {
            err_toast(app, ar, &tool_label(tool, ar));
            return None;
        }
    }
    Some(spec)
}

fn on_tool_requested(app: &AppHandle, paths: Vec<PathBuf>, tool: String, x: i32, y: i32) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let settings = state.agent.lock().unwrap().clone();
    let ar = settings.is_ar();
    let tools = state.tools(app);
    let Some(first) = paths.first().cloned() else {
        return;
    };
    if tool == "merge" {
        // Panel-less: the multi-file drop IS the input.
        if paths.len() < 2 {
            err_toast(
                app,
                ar,
                if ar {
                    "الدمج يحتاج ملفين على الأقل — اسحب عدة ملفات معًا"
                } else {
                    "Merge needs at least two files — drag several PDFs together"
                },
            );
            return;
        }
        let job = JobSpec::Tool {
            input: first.to_string_lossy().into_owned(),
            tool: ToolJob::PdfMerge {
                inputs: paths.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
            },
            output_dir: settings.output_dir.clone(),
            suffix: settings.suffix.clone(),
        };
        run_overlay_tool_jobs(app, vec![job]);
        return;
    }
    let info = analyze(&tools, &first);
    let id = next_panel_id(&state);
    match build_tool_panel(app, &id, &tools, &info, &tool, (x, y), ar) {
        Some(spec) => open_panel(app, id, PanelCtx::Tool { tool, paths }, spec),
        None => cleanup_panel_files(&id),
    }
}

/// Turn panel values into a job for ONE path (the caller fans out).
#[allow(clippy::too_many_arguments)]
fn tool_job_from_values(
    app: &AppHandle,
    tool: &str,
    values: &std::collections::HashMap<String, String>,
    pages: &[uruk_drag_sensor::PageShapes],
    path: &PathBuf,
    settings: &AgentSettings,
) -> Result<JobSpec, String> {
    let tools = app
        .try_state::<AppState>()
        .map(|s| s.tools(app))
        .ok_or("no app state")?;
    let info = analyze(&tools, path);
    let out = |tool_job: ToolJob| JobSpec::Tool {
        input: path.to_string_lossy().into_owned(),
        tool: tool_job,
        output_dir: settings.output_dir.clone(),
        suffix: settings.suffix.clone(),
    };
    let get = |k: &str| values.get(k).cloned().unwrap_or_default();
    let parse_u32 = |k: &str| -> Result<u32, String> {
        get(k).trim().parse::<u32>().map_err(|_| format!("'{k}' must be a whole number"))
    };
    match tool {
        "crop" => {
            let (iw, ih) = dims_of(&info).unwrap_or((0, 0));
            let mut w = parse_u32("w")?;
            let mut h = parse_u32("h")?;
            let aspect = get("aspect");
            if aspect != "Free" && aspect.contains(':') {
                if let Some((a, b)) = aspect.split_once(':') {
                    if let (Ok(a), Ok(b)) = (a.parse::<f32>(), b.parse::<f32>()) {
                        if a > 0.0 {
                            h = ((w as f32) * b / a).round() as u32;
                        }
                    }
                }
            }
            let x = parse_u32("x")?.min(iw.saturating_sub(1));
            let y = parse_u32("y")?.min(ih.saturating_sub(1));
            w = w.max(1).min(iw.saturating_sub(x).max(1));
            h = h.max(1).min(ih.saturating_sub(y).max(1));
            Ok(out(ToolJob::ImageCrop { x, y, w, h }))
        }
        "resize" => {
            let (iw, ih) = dims_of(&info).unwrap_or((0, 0));
            let w = parse_u32("w").ok().filter(|v| *v > 0);
            let h = parse_u32("h").ok().filter(|v| *v > 0);
            let keep = get("keep") == "true";
            let (w, h) = if keep && w.is_some() && h.is_some() {
                // Only the dimension the user actually changed drives.
                if w != Some(iw) {
                    (w, None)
                } else if h != Some(ih) {
                    (None, h)
                } else {
                    (w, None)
                }
            } else {
                (w, h)
            };
            Ok(out(ToolJob::ImageResize { w, h }))
        }
        "transform" => {
            let rotate: i32 = get("rotate")
                .trim_end_matches('°')
                .trim()
                .parse()
                .map_err(|_| "bad rotation".to_string())?;
            Ok(out(ToolJob::ImageTransform {
                rotate,
                flip_h: get("flip_h") == "true",
                flip_v: get("flip_v") == "true",
            }))
        }
        "background" => Ok(out(ToolJob::ImageBackground { color: get("color") })),
        "metadata" => Ok(out(ToolJob::ImageStripMetadata)),
        "annotate" => {
            let (iw, _) = dims_of(&info).unwrap_or((1000, 1000));
            let text_size = ((iw as f32) * 0.05).clamp(18.0, 72.0);
            let mut shapes = Vec::new();
            for page in pages {
                for sh in &page.shapes {
                    shapes.push(match sh {
                        CanvasShape::Rect { x, y, w, h } => DrawShape::Rect {
                            x: *x,
                            y: *y,
                            w: *w,
                            h: *h,
                        },
                        CanvasShape::Arrow { x1, y1, x2, y2 } => DrawShape::Arrow {
                            x1: *x1,
                            y1: *y1,
                            x2: *x2,
                            y2: *y2,
                        },
                        CanvasShape::Text { x, y, text } => DrawShape::Text {
                            x: *x,
                            y: *y,
                            text: text.clone(),
                            size: text_size,
                        },
                    });
                }
            }
            if shapes.is_empty() {
                return Err("nothing was drawn".into());
            }
            Ok(out(ToolJob::ImageAnnotate { shapes }))
        }
        "compress" => {
            let value: f64 = get("percent")
                .parse()
                .map_err(|_| "bad percent".to_string())?;
            Ok(JobSpec::CompressImage {
                input: path.to_string_lossy().into_owned(),
                mode: "percent".into(),
                value,
                output_dir: settings.output_dir.clone(),
                suffix: settings.suffix.clone(),
            })
        }
        "trim" => {
            let start: f64 = get("start").parse().map_err(|_| "bad start time".to_string())?;
            let end: f64 = get("end").parse().map_err(|_| "bad end time".to_string())?;
            Ok(out(ToolJob::MediaTrim { start, end }))
        }
        "split" | "extract-pages" => {
            let ranges = uruk_engine_core::parse_page_ranges(&get("ranges"))?;
            if tool == "split" {
                Ok(out(ToolJob::PdfSplit { ranges }))
            } else {
                Ok(out(ToolJob::PdfExtract { ranges }))
            }
        }
        "rotate-pdf" => {
            let angle: i32 = get("angle")
                .trim_end_matches('°')
                .trim()
                .parse()
                .map_err(|_| "bad angle".to_string())?;
            Ok(out(ToolJob::PdfRotate { angle }))
        }
        "compress-pdf" => Ok(out(ToolJob::PdfCompress { tier: get("tier") })),
        "redact" => {
            let mut regions = Vec::new();
            for page in pages {
                for sh in &page.shapes {
                    if let CanvasShape::Rect { x, y, w, h } = sh {
                        regions.push(PdfRegion {
                            page: page.page,
                            x: *x,
                            y: *y,
                            w: *w,
                            h: *h,
                        });
                    }
                }
            }
            if regions.is_empty() {
                return Err("no redaction regions were drawn".into());
            }
            Ok(out(ToolJob::PdfRedact { regions }))
        }
        other => Err(format!("unknown tool {other}")),
    }
}

/// Run tool jobs (one per dragged file) on a thread, record history
/// with origin `overlay`, and answer with one summary toast.
fn run_overlay_tool_jobs(app: &AppHandle, jobs: Vec<JobSpec>) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let tools = state.tools(app);
    let settings = state.agent.lock().unwrap().clone();
    let ar = settings.is_ar();
    let app = app.clone();
    std::thread::spawn(move || {
        let mut ok_count = 0usize;
        let mut first_output: Option<String> = None;
        let mut first_error: Option<String> = None;
        let total = jobs.len();
        for job in &jobs {
            let (hist_input, hist_target) = job_history_fields(job);
            let cancel = Arc::new(AtomicBool::new(false));
            match run_job(&tools, job, &cancel, &mut |_| {}) {
                Ok(outcome) => {
                    ok_count += 1;
                    let out_s = outcome.output.to_string_lossy().into_owned();
                    if first_output.is_none() {
                        first_output = Some(match &outcome.note {
                            Some(n) => format!("{} — {}", file_name_of(&out_s), n),
                            None => file_name_of(&out_s),
                        });
                    }
                    record_history(
                        &app,
                        HistoryRecord {
                            ts: now_secs(),
                            input_name: file_name_of(&hist_input),
                            input_path: hist_input,
                            output_path: Some(out_s),
                            target: hist_target,
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
                        &app,
                        HistoryRecord {
                            ts: now_secs(),
                            input_name: file_name_of(&hist_input),
                            input_path: hist_input,
                            output_path: None,
                            target: hist_target,
                            status: "error".into(),
                            error: Some(e.message),
                            origin: "overlay".into(),
                        },
                    );
                }
            }
        }
        if let Some(state) = app.try_state::<AppState>() {
            if ok_count == total {
                let title = if ar {
                    if total > 1 {
                        format!("تم تطبيق الأداة على {total} ملفات")
                    } else {
                        "تم تطبيق الأداة".to_string()
                    }
                } else if total > 1 {
                    format!("Tool applied to {total} files")
                } else {
                    "Tool applied".to_string()
                };
                state.toast(ToastKind::Success, title, first_output.unwrap_or_default());
            } else {
                state.toast(
                    ToastKind::Error,
                    if ar { "تعذّرت الأداة".into() } else { "Tool failed".into() },
                    first_error.unwrap_or_default(),
                );
            }
        }
    });
}

fn apply_panel(app: &AppHandle, ctx: PanelCtx, outcome: PanelOutcome) {
    let PanelOutcome::Applied { values, pages } = outcome else {
        return; // Cancelled: nothing to do.
    };
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let settings = state.agent.lock().unwrap().clone();
    let ar = settings.is_ar();
    match ctx {
        PanelCtx::Tool { tool, paths } => {
            let mut jobs = Vec::new();
            for path in &paths {
                match tool_job_from_values(app, &tool, &values, &pages, path, &settings) {
                    Ok(job) => jobs.push(job),
                    Err(e) => {
                        err_toast(app, ar, &e);
                        return;
                    }
                }
            }
            if !jobs.is_empty() {
                run_overlay_tool_jobs(app, jobs);
            }
        }
        PanelCtx::Move { paths } => {
            let dest = values.get("dest").cloned().unwrap_or_default();
            do_move(app, paths, &dest);
        }
    }
}

/// Move files into a destination folder (the Move-to action).
fn do_move(app: &AppHandle, paths: Vec<PathBuf>, dest_dir: &str) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let ar = state.agent.lock().unwrap().is_ar();
    let dest = PathBuf::from(dest_dir.trim());
    if dest_dir.trim().is_empty() || (!dest.is_dir() && std::fs::create_dir_all(&dest).is_err()) {
        err_toast(
            app,
            ar,
            if ar { "مجلد الوجهة غير صالح" } else { "The destination folder is not valid" },
        );
        return;
    }
    let mut moved = 0usize;
    let mut first_new: Option<String> = None;
    for path in &paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let mut target = dest.join(&name);
        let mut n = 2;
        while target.exists() {
            let stem = name.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(name.clone());
            let ext = name.rsplit_once('.').map(|(_, e)| e.to_string()).unwrap_or_default();
            target = dest.join(if ext.is_empty() {
                format!("{stem} ({n})")
            } else {
                format!("{stem} ({n}).{ext}")
            });
            n += 1;
        }
        let ok = std::fs::rename(path, &target).is_ok()
            || (std::fs::copy(path, &target).is_ok() && std::fs::remove_file(path).is_ok());
        if ok {
            moved += 1;
            if first_new.is_none() {
                first_new = Some(target.to_string_lossy().into_owned());
            }
            record_history(
                app,
                HistoryRecord {
                    ts: now_secs(),
                    input_name: name.clone(),
                    input_path: path.to_string_lossy().into_owned(),
                    output_path: Some(target.to_string_lossy().into_owned()),
                    target: "move".into(),
                    status: "done".into(),
                    error: None,
                    origin: "overlay".into(),
                },
            );
        }
    }
    if moved > 0 {
        state.toast(
            ToastKind::Success,
            if ar { "تم النقل".into() } else { "Moved".into() },
            first_new.unwrap_or_default(),
        );
    } else {
        err_toast(app, ar, if ar { "لم يُنقل أي ملف" } else { "No file was moved" });
    }
}

/// Open a terminal in `dir` (the Terminal-here action), per platform.
fn open_terminal(dir: &PathBuf) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .args(["-a", "Terminal"])
            .arg(dir)
            .spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "cmd", "/K", "cd", "/d"])
            .arg(dir)
            .spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for term in [
            "x-terminal-emulator",
            "gnome-terminal",
            "konsole",
            "kitty",
            "alacritty",
            "xfce4-terminal",
            "xterm",
        ] {
            if std::process::Command::new(term)
                .current_dir(dir)
                .spawn()
                .is_ok()
            {
                break;
            }
        }
    }
}

fn on_action_requested(app: &AppHandle, paths: Vec<PathBuf>, action: String, x: i32, y: i32) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let ar = state.agent.lock().unwrap().is_ar();
    match action.as_str() {
        "move" => {
            let parent = paths
                .first()
                .and_then(|p| p.parent())
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_default();
            let id = next_panel_id(&state);
            let spec = PanelSpec {
                id: id.clone(),
                title: action_label("move", ar),
                anchor: (x, y),
                fields: vec![PanelField::Text {
                    key: "dest".into(),
                    label: if ar { "مجلد الوجهة".into() } else { "Destination folder".into() },
                    value: parent,
                }],
                canvas: None,
                apply_label: if ar { "نقل".into() } else { "Move".into() },
                cancel_label: if ar { "إلغاء".into() } else { "Cancel".into() },
                rtl: ar,
            };
            open_panel(app, id, PanelCtx::Move { paths }, spec);
        }
        "reveal" => {
            for p in &paths {
                let _ = reveal_in_folder(p.to_string_lossy().into_owned());
            }
        }
        "terminal" => {
            if let Some(first) = paths.first() {
                let dir = if first.is_dir() {
                    first.clone()
                } else {
                    first.parent().map(|d| d.to_path_buf()).unwrap_or(first.clone())
                };
                open_terminal(&dir);
            }
        }
        _ => {}
    }
}

fn handle_sensor_event(app: &AppHandle, ev: SensorEvent) {
    match ev {
        SensorEvent::ConvertRequested { paths, target } => {
            let app = app.clone();
            std::thread::spawn(move || run_overlay_conversions(&app, paths, target));
        }
        SensorEvent::ToolRequested { paths, tool, x, y } => {
            let app = app.clone();
            std::thread::spawn(move || on_tool_requested(&app, paths, tool, x, y));
        }
        SensorEvent::ActionRequested { paths, action, x, y } => {
            on_action_requested(app, paths, action, x, y);
        }
        SensorEvent::ActionDone { paths, action } => {
            if let Some(state) = app.try_state::<AppState>() {
                let ar = state.agent.lock().unwrap().is_ar();
                let body = if action == "copy-path" {
                    paths
                        .first()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default()
                } else if paths.len() > 1 {
                    if ar {
                        format!("{} ملفات", paths.len())
                    } else {
                        format!("{} files", paths.len())
                    }
                } else {
                    paths
                        .first()
                        .map(|p| file_name_of(&p.to_string_lossy()))
                        .unwrap_or_default()
                };
                let title = match action.as_str() {
                    "copy-path" => {
                        if ar { "تم نسخ المسار" } else { "Path copied" }
                    }
                    "share" => {
                        if ar { "تمت المشاركة" } else { "Shared" }
                    }
                    _ => {
                        if ar { "تم نسخ الملف" } else { "File copied" }
                    }
                };
                state.toast(ToastKind::Success, title.to_string(), body);
            }
        }
        SensorEvent::PanelClosed { id, outcome } => {
            cleanup_panel_files(&id);
            if let Some(state) = app.try_state::<AppState>() {
                if let Some(ctx) = state.panels.lock().unwrap().remove(&id) {
                    let app = app.clone();
                    std::thread::spawn(move || apply_panel(&app, ctx, outcome));
                }
            }
        }
        SensorEvent::PanelPageRequest { id, page } => {
            if let Some(state) = app.try_state::<AppState>() {
                let ctx_path = {
                    let panels = state.panels.lock().unwrap();
                    match panels.get(&id) {
                        Some(PanelCtx::Tool { tool, paths }) if tool == "redact" => {
                            paths.first().cloned()
                        }
                        _ => None,
                    }
                };
                if let Some(pdf) = ctx_path {
                    let tools = state.tools(app);
                    let (pages, _) = pdf_info_of(&tools, &pdf);
                    let out = panel_temp_dir().join(format!("{id}-page-{page}.png"));
                    if render_pdf_page_preview(&tools, &pdf, page, &out).is_ok() {
                        state.send_sensor(SensorCommand::UpdatePanelCanvas {
                            id,
                            image_png: out,
                            page,
                            pages,
                        });
                    }
                }
            }
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
    let agent_for_resolver = state.agent.clone();
    let resolver: WheelResolver = Arc::new(move |req: &WheelRequest| {
        let first = req.paths.first()?;
        let info = analyze(&tools, first);
        let ar = agent_for_resolver
            .lock()
            .map(|s| s.is_ar())
            .unwrap_or(true);
        let segments: Vec<WheelSegment> = match req.kind {
            WheelKind::Convert => {
                if info.targets.is_empty() {
                    return None;
                }
                info.targets
                    .iter()
                    .map(|t| WheelSegment::new(t.clone(), t.to_uppercase()))
                    .collect()
            }
            WheelKind::Tools => {
                let mut ids = tool_ids_for(first, info.is_dir);
                // Merge is meaningless for a single dragged file.
                if req.paths.len() < 2 {
                    ids.retain(|id| *id != uruk_engine_core::TOOL_PDF_MERGE);
                }
                if ids.is_empty() {
                    return None;
                }
                ids.iter()
                    .map(|id| WheelSegment::new(*id, tool_label(id, ar)))
                    .collect()
            }
            WheelKind::Actions => action_ids()
                .iter()
                .map(|id| WheelSegment::new(*id, action_label(id, ar)))
                .collect(),
        };
        if segments.is_empty() {
            return None;
        }
        let hub_ext = if !info.ext.is_empty() {
            info.ext.to_uppercase()
        } else if info.is_dir {
            "DIR".into()
        } else {
            "FILE".into()
        };
        let hub_title = if req.paths.len() > 1 {
            if ar {
                format!("{} ملفات", req.paths.len())
            } else {
                format!("{} files", req.paths.len())
            }
        } else {
            info.name.clone()
        };
        Some(WheelModel {
            kind: req.kind,
            segments,
            hub_ext,
            hub_title,
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
    // The orb tier exists only on Linux (the drag-sensor's `wayland`
    // module is Linux-only); other platforms never report a Wayland
    // session, so this is a deliberate no-op there.
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (app, state, settings);
    }
    #[cfg(target_os = "linux")]
    {
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
            panels: Mutex::new(HashMap::new()),
            panel_seq: std::sync::atomic::AtomicU64::new(1),
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
