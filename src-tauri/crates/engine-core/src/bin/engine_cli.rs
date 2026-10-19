//! engine-cli — drives uruk-engine-core from the command line.
//!
//! Used by the automated engine tests (tests/engine/run.mjs) so the exact
//! code the GUI runs is what gets tested. Also handy for debugging.
//!
//! Usage:
//!   engine-cli tools                       → JSON report of resolved tools
//!   engine-cli analyze <path>              → JSON FileInfo
//!   engine-cli run <job.json>              → runs a JobSpec (JSON), printing
//!                                            "PROGRESS <n>" lines, then
//!                                            "DONE <output path>" or
//!                                            "ERROR <code> <message>"
//!
//! Tool resolution: URUK_SIDECAR_DIR env var, then PATH.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use uruk_engine_core::{analyze, run_job, tool_report, JobSpec, Tools};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let tools = Tools::resolve(&[]);
    match cmd {
        "tools" => {
            println!("{}", serde_json::to_string_pretty(&tool_report(&tools)).unwrap());
        }
        "analyze" => {
            let path = PathBuf::from(args.get(2).expect("path required"));
            let info = analyze(&tools, &path);
            println!("{}", serde_json::to_string_pretty(&info).unwrap());
        }
        "run" => {
            let job_path = args.get(2).expect("job json path required");
            let raw = std::fs::read_to_string(job_path).expect("read job json");
            let job: JobSpec = serde_json::from_str(&raw).expect("parse job json");
            let cancel = Arc::new(AtomicBool::new(false));
            let mut last = -2.0f32;
            let result = run_job(&tools, &job, &cancel, &mut |p| {
                if (p - last).abs() > 0.5 || p >= 100.0 {
                    println!("PROGRESS {p:.0}");
                    last = p;
                }
            });
            match result {
                Ok(outcome) => {
                    if let Some(note) = outcome.note {
                        println!("NOTE {note}");
                    }
                    println!("DONE {}", outcome.output.display());
                }
                Err(e) => {
                    println!("ERROR {} {}", e.code, e.message);
                    std::process::exit(3);
                }
            }
        }
        _ => {
            eprintln!("usage: engine-cli <tools|analyze <path>|run <job.json>>");
            std::process::exit(2);
        }
    }
}
