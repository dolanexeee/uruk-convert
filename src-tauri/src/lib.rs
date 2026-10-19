//! Uruk Convert — Tauri shell.
//!
//! The shell is thin by design: all conversion logic lives in
//! `uruk-engine-core`. These commands analyze dropped files, run jobs on
//! worker threads (streaming progress back as events), support cancel, and
//! offer small OS integrations (reveal in folder). No network access is
//! used anywhere; the app is fully local.

use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};
use uruk_engine_core::{analyze, run_job, FileInfo, JobSpec, Tools};

struct AppState {
    tools: Mutex<Option<Tools>>,
    jobs: Mutex<HashMap<String, Arc<AtomicBool>>>,
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
}

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

#[tauri::command]
fn start_job(app: AppHandle, state: State<AppState>, id: String, job: JobSpec) {
    let tools = state.tools(&app);
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
                    ProgressPayload { id: id.clone(), percent: p },
                );
            }
        });
        match result {
            Ok(outcome) => {
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
                let _ = app2.emit(
                    "job-error",
                    ErrorPayload { id: id.clone(), code: e.code, message: e.message },
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

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            tools: Mutex::new(None),
            jobs: Mutex::new(HashMap::new()),
        })
        .invoke_handler(tauri::generate_handler![
            analyze_paths,
            tools_report,
            start_job,
            cancel_job,
            reveal_in_folder,
            open_path
        ])
        .run(tauri::generate_context!())
        .expect("error while running Uruk Convert");
}
