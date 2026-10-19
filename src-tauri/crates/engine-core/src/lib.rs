//! Uruk Convert — engine layer.
//!
//! Every conversion in the app flows through this crate. It knows nothing
//! about Tauri or the UI: it resolves the sidecar tools (bundled binaries
//! first, PATH as a development fallback), classifies files into families,
//! builds the exact command lines for ffmpeg / ImageMagick / 7-Zip, runs
//! them with progress reporting and cancellation, and turns tool failures
//! into plain-language errors.
//!
//! The GUI (src-tauri) and the `engine-cli` test binary both call this code,
//! so the automated engine tests exercise the real conversion paths.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineError {
    /// Machine-readable code the UI localizes.
    pub code: String,
    /// Plain-language explanation (English; UI adds localized framing).
    pub message: String,
}

impl EngineError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        EngineError {
            code: code.into(),
            message: message.into(),
        }
    }
    pub fn tool_missing(tool: &str) -> Self {
        Self::new(
            "tool_missing",
            format!("The {tool} tool is not available, so this conversion cannot run."),
        )
    }
    pub fn unsupported(msg: impl Into<String>) -> Self {
        Self::new("unsupported", msg)
    }
    pub fn cancelled() -> Self {
        Self::new("cancelled", "Conversion cancelled.")
    }
    pub fn failed(tool: &str, tail: &str) -> Self {
        let tail = tail.trim();
        let tail = if tail.len() > 400 {
            &tail[tail.len() - 400..]
        } else {
            tail
        };
        Self::new(
            "tool_failed",
            format!("{tool} could not complete this conversion. Details: {tail}"),
        )
    }
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}
impl std::error::Error for EngineError {}

pub type EngineResult<T> = Result<T, EngineError>;

// ---------------------------------------------------------------------------
// Families and conversion targets (Stage 1 scope — see PARITY.md)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    Image,
    Audio,
    Video,
    Archive,
    Pdf,
    Document,
    Slides,
    Spreadsheet,
    Subtitle,
    Unknown,
}

pub const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "heic", "heif", "tiff", "tif", "avif", "bmp", "svg",
];
pub const AUDIO_EXTS: &[&str] = &[
    "mp3", "m4a", "wav", "flac", "ogg", "opus", "aif", "aiff", "wma",
];
pub const VIDEO_EXTS: &[&str] = &["mp4", "mov", "mkv", "webm", "avi", "wmv", "gif"];
pub const ARCHIVE_EXTS: &[&str] = &["zip", "tar", "gz", "tgz", "tar.gz", "rar"];
pub const PDF_EXTS: &[&str] = &["pdf"];
/// Document inputs. `odt` is a LibreOffice-native extra beyond the
/// reference's list; `pages` is deliberately absent (Apple's format has
/// no open reader/writer — Pages files fall through to Unknown and can
/// still be archived).
pub const DOC_EXTS: &[&str] = &[
    "docx", "odt", "rtf", "txt", "md", "markdown", "html", "htm", "epub",
];
/// Slide inputs. `ppt` is legacy input-only per the reference; `odp` is a
/// LibreOffice-native extra.
pub const SLIDE_EXTS: &[&str] = &["pptx", "ppt", "odp"];
/// Spreadsheet inputs. `xls` is legacy input-only per the reference.
pub const SHEET_EXTS: &[&str] = &["xlsx", "ods", "csv", "tsv", "xls"];
pub const SUBTITLE_EXTS: &[&str] = &["srt", "vtt"];

/// Target formats offered for an image input (Stage 1). `svg` is input-only.
pub const IMAGE_TARGETS: &[&str] = &["jpg", "png", "webp", "heic", "tiff", "avif", "bmp", "pdf"];
/// Target formats offered for an audio input (Stage 1).
pub const AUDIO_TARGETS: &[&str] = &["mp3", "m4a", "wav", "flac", "ogg", "opus", "aiff", "wma"];
/// Target formats offered for a video input (Stage 1), incl. audio export.
pub const VIDEO_TARGETS: &[&str] = &["mp4", "mov", "mkv", "webm", "avi", "wmv", "gif", "mp3"];
/// Archive re-pack targets (Stage 1: create ZIP / TAR / GZIP).
pub const ARCHIVE_TARGETS: &[&str] = &["zip", "tar", "gz"];
/// PDF targets (Stage 2). The reference also lists Pages, an Apple-only
/// format no open toolchain can write — it is not offered (see PARITY.md).
/// DOCX/PPTX are text-faithful best-effort routes and XLSX is refused
/// with a plain-language error; both are documented in PARITY.md.
pub const PDF_TARGETS: &[&str] = &["docx", "pptx", "xlsx", "jpg", "png", "epub", "txt", "md"];
/// Document targets (Stage 2): the reference's set (DOCX, MD, TXT, EPUB,
/// HTML) plus PDF export. Pages is not offered (see PDF_TARGETS note).
pub const DOC_TARGETS: &[&str] = &["pdf", "docx", "md", "txt", "epub", "html"];
/// Slide targets (Stage 2).
pub const SLIDE_TARGETS: &[&str] = &["pdf", "jpg", "png"];
/// Spreadsheet targets (Stage 2). `xls` is input-only, never a target.
pub const SHEET_TARGETS: &[&str] = &["xlsx", "ods", "csv", "tsv", "pdf"];
/// Subtitle targets (Stage 2): the other timed-text format + plain text.
pub const SUBTITLE_TARGETS: &[&str] = &["srt", "vtt", "txt"];

/// Lowercased extension of a path, treating `.tar.gz` as one extension.
pub fn extension_of(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if name.ends_with(".tar.gz") {
        return "tar.gz".into();
    }
    Path::new(&name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

pub fn family_of(path: &Path, is_dir: bool) -> Family {
    if is_dir {
        return Family::Archive; // folders can be archived
    }
    let ext = extension_of(path);
    let ext = ext.as_str();
    if IMAGE_EXTS.contains(&ext) {
        Family::Image
    } else if AUDIO_EXTS.contains(&ext) {
        Family::Audio
    } else if VIDEO_EXTS.contains(&ext) {
        Family::Video
    } else if ARCHIVE_EXTS.contains(&ext) {
        Family::Archive
    } else if PDF_EXTS.contains(&ext) {
        Family::Pdf
    } else if DOC_EXTS.contains(&ext) {
        Family::Document
    } else if SLIDE_EXTS.contains(&ext) {
        Family::Slides
    } else if SHEET_EXTS.contains(&ext) {
        Family::Spreadsheet
    } else if SUBTITLE_EXTS.contains(&ext) {
        Family::Subtitle
    } else {
        Family::Unknown
    }
}

/// Valid conversion targets for a file: family targets minus identity.
/// Folders and unknown files can still be archived (zip/tar/gz).
pub fn targets_for(path: &Path, is_dir: bool) -> Vec<String> {
    let family = family_of(path, is_dir);
    let ext = extension_of(path);
    let norm = |e: &str| -> String {
        match e {
            "jpeg" => "jpg".into(),
            "tif" => "tiff".into(),
            "aif" => "aiff".into(),
            "tgz" => "tar".into(),
            "markdown" => "md".into(),
            "htm" => "html".into(),
            other => other.into(),
        }
    };
    let mine = norm(&ext);
    let pool: &[&str] = match family {
        Family::Image => IMAGE_TARGETS,
        Family::Audio => AUDIO_TARGETS,
        Family::Video => VIDEO_TARGETS,
        Family::Archive => ARCHIVE_TARGETS,
        Family::Pdf => PDF_TARGETS,
        Family::Document => DOC_TARGETS,
        Family::Slides => SLIDE_TARGETS,
        Family::Spreadsheet => SHEET_TARGETS,
        Family::Subtitle => SUBTITLE_TARGETS,
        Family::Unknown => ARCHIVE_TARGETS,
    };
    pool.iter()
        .filter(|t| **t != mine)
        .map(|t| t.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// File analysis
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub name: String,
    pub ext: String,
    pub family: Family,
    pub is_dir: bool,
    pub size: u64,
    pub targets: Vec<String>,
    /// Media duration in seconds when ffprobe can read it.
    pub duration: Option<f64>,
    /// "1920x1080" for images/video when known.
    pub dimensions: Option<String>,
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                total += dir_size(&p);
            } else if let Ok(m) = e.metadata() {
                total += m.len();
            }
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Tool resolution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Tools {
    pub ffmpeg: Option<PathBuf>,
    pub ffprobe: Option<PathBuf>,
    /// ImageMagick binary and whether it needs the `convert` subcommand form
    /// (standalone `magick` binary = false, legacy `convert` binary = true).
    pub magick: Option<PathBuf>,
    pub magick_is_convert: bool,
    pub sevenz: Option<PathBuf>,
    /// LibreOffice (`soffice`) — documents, slides, spreadsheets, PDF export.
    pub soffice: Option<PathBuf>,
    /// Pandoc — semantic document formats (MD/HTML/EPUB/TXT/DOCX).
    pub pandoc: Option<PathBuf>,
    /// poppler-utils: PDF rasterizer (300 DPI page images).
    pub pdftoppm: Option<PathBuf>,
    /// poppler-utils: PDF text extractor.
    pub pdftotext: Option<PathBuf>,
    /// poppler-utils: PDF image extractor (resolved for completeness).
    pub pdfimages: Option<PathBuf>,
    /// poppler-utils: PDF metadata (page count).
    pub pdfinfo: Option<PathBuf>,
    /// Uppercase ImageMagick formats writable by this build (from `-list format`).
    pub magick_writable: Vec<String>,
    /// ffmpeg muxer names (from `-muxers`), e.g. contains "avif".
    pub ffmpeg_muxers: Vec<String>,
    /// ffmpeg encoder names (from `-encoders`).
    pub ffmpeg_encoders: Vec<String>,
}

fn find_in_dirs(dirs: &[PathBuf], names: &[String]) -> Option<PathBuf> {
    for d in dirs {
        for n in names {
            let p = d.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn find_on_path(names: &[&str]) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for n in names {
            let p = dir.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Names a sidecar may have on disk: bundled builds carry the target triple
/// (`uruk-ffmpeg-x86_64-unknown-linux-gnu`, `uruk-ffmpeg.exe` on Windows).
fn sidecar_candidates(base: &str) -> Vec<String> {
    let mut v = vec![base.to_string(), format!("{base}.exe")];
    if let Ok(triples) = std::env::var("URUK_TARGET_TRIPLES") {
        for t in triples.split(',') {
            v.push(format!("{base}-{t}"));
            v.push(format!("{base}-{t}.exe"));
        }
    }
    // Common triples so resolution works without env plumbing.
    for t in [
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
    ] {
        v.push(format!("{base}-{t}"));
        v.push(format!("{base}-{t}.exe"));
    }
    v
}

fn run_capture(program: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    Some(s)
}

impl Tools {
    /// Resolve tools: explicit dirs first (bundled sidecars), then
    /// `URUK_SIDECAR_DIR`, then PATH (development fallback).
    pub fn resolve(extra_dirs: &[PathBuf]) -> Tools {
        let mut dirs: Vec<PathBuf> = extra_dirs.to_vec();
        if let Ok(d) = std::env::var("URUK_SIDECAR_DIR") {
            if !d.is_empty() {
                dirs.push(PathBuf::from(d));
            }
        }
        let find = |base: &str, path_names: &[&str]| -> Option<PathBuf> {
            find_in_dirs(&dirs, &sidecar_candidates(base)).or_else(|| find_on_path(path_names))
        };

        let ffmpeg = find("uruk-ffmpeg", &["ffmpeg", "ffmpeg.exe"]);
        let ffprobe = find("uruk-ffprobe", &["ffprobe", "ffprobe.exe"]);
        let sevenz = find("uruk-7z", &["7z", "7zz", "7za", "7z.exe"]);
        let soffice = find("uruk-soffice", &["soffice", "libreoffice", "soffice.exe"]);
        let pandoc = find("uruk-pandoc", &["pandoc", "pandoc.exe"]);
        let pdftoppm = find("uruk-pdftoppm", &["pdftoppm", "pdftoppm.exe"]);
        let pdftotext = find("uruk-pdftotext", &["pdftotext", "pdftotext.exe"]);
        let pdfimages = find("uruk-pdfimages", &["pdfimages", "pdfimages.exe"]);
        let pdfinfo = find("uruk-pdfinfo", &["pdfinfo", "pdfinfo.exe"]);

        // ImageMagick: prefer a real `magick` binary; fall back to `convert`.
        let (magick, magick_is_convert) = match find("uruk-magick", &["magick", "magick.exe"]) {
            Some(p) => (Some(p), false),
            None => match find_on_path(&["convert", "convert.exe"]) {
                // On Windows, `convert.exe` in System32 is the NTFS tool, not
                // ImageMagick — only trust it off-Windows or when it answers
                // like ImageMagick.
                Some(p) => {
                    let ok = run_capture(&p, &["-version"])
                        .map(|s| s.contains("ImageMagick"))
                        .unwrap_or(false);
                    if ok {
                        (Some(p), true)
                    } else {
                        (None, false)
                    }
                }
                None => (None, false),
            },
        };

        let mut tools = Tools {
            ffmpeg,
            ffprobe,
            magick,
            magick_is_convert,
            sevenz,
            soffice,
            pandoc,
            pdftoppm,
            pdftotext,
            pdfimages,
            pdfinfo,
            magick_writable: Vec::new(),
            ffmpeg_muxers: Vec::new(),
            ffmpeg_encoders: Vec::new(),
        };
        tools.detect_capabilities();
        tools
    }

    fn detect_capabilities(&mut self) {
        if let Some(m) = &self.magick {
            let args: &[&str] = if self.magick_is_convert {
                &["-list", "format"]
            } else {
                &["-list", "format"]
            };
            if let Some(list) = run_capture(m, args) {
                for line in list.lines() {
                    let mut parts = line.split_whitespace();
                    if let (Some(fmt), Some(_module), Some(mode)) =
                        (parts.next(), parts.next(), parts.next())
                    {
                        if mode.contains('w') {
                            self.magick_writable
                                .push(fmt.trim_end_matches('*').to_uppercase());
                        }
                    }
                }
            }
        }
        if let Some(f) = &self.ffmpeg {
            if let Some(m) = run_capture(f, &["-hide_banner", "-muxers"]) {
                self.ffmpeg_muxers = m
                    .lines()
                    .filter_map(|l| l.split_whitespace().nth(1).map(|s| s.to_string()))
                    .collect();
            }
            if let Some(e) = run_capture(f, &["-hide_banner", "-encoders"]) {
                self.ffmpeg_encoders = e
                    .lines()
                    .filter_map(|l| l.split_whitespace().nth(1).map(|s| s.to_string()))
                    .collect();
            }
        }
    }

    pub fn magick_can_write(&self, fmt_upper: &str) -> bool {
        self.magick.is_some() && self.magick_writable.iter().any(|f| f == fmt_upper)
    }
    pub fn ffmpeg_has_muxer(&self, name: &str) -> bool {
        self.ffmpeg_muxers.iter().any(|m| m == name)
    }
    pub fn ffmpeg_has_encoder(&self, name: &str) -> bool {
        self.ffmpeg_encoders.iter().any(|e| e == name)
    }

    /// Base argv for ImageMagick (`magick …` or `convert …`).
    pub fn magick_argv(&self) -> Option<(PathBuf, Vec<String>)> {
        self.magick.clone().map(|p| (p, Vec::new()))
    }
}

// ---------------------------------------------------------------------------
// Probing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Probe {
    pub duration: Option<f64>,
    pub has_audio: bool,
    pub has_video: bool,
    pub dimensions: Option<String>,
    pub format_name: Option<String>,
}

pub fn probe(tools: &Tools, path: &Path) -> Probe {
    let mut pr = Probe::default();
    let family = family_of(path, path.is_dir());
    if let Some(ffprobe) = &tools.ffprobe {
        if matches!(family, Family::Audio | Family::Video) {
            if let Some(out) = run_capture(
                ffprobe,
                &[
                    "-v",
                    "error",
                    "-show_entries",
                    "stream=codec_type,width,height",
                    "-show_entries",
                    "format=duration,format_name",
                    "-of",
                    "json",
                    &path.to_string_lossy(),
                ],
            ) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&out) {
                    if let Some(streams) = v.get("streams").and_then(|s| s.as_array()) {
                        for s in streams {
                            match s.get("codec_type").and_then(|c| c.as_str()) {
                                Some("audio") => pr.has_audio = true,
                                Some("video") => {
                                    pr.has_video = true;
                                    if let (Some(w), Some(h)) = (
                                        s.get("width").and_then(|x| x.as_u64()),
                                        s.get("height").and_then(|x| x.as_u64()),
                                    ) {
                                        pr.dimensions = Some(format!("{w}x{h}"));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    if let Some(fmt) = v.get("format") {
                        pr.duration = fmt.get("duration").and_then(|d| {
                            d.as_str()
                                .and_then(|s| s.parse::<f64>().ok())
                                .or_else(|| d.as_f64())
                        });
                        pr.format_name = fmt
                            .get("format_name")
                            .and_then(|f| f.as_str())
                            .map(|s| s.to_string());
                    }
                }
            }
            return pr;
        }
    }
    if family == Family::Image {
        // IM7: `magick identify …`; IM6: the separate `identify` binary.
        let identify_prog: Option<PathBuf> = if tools.magick_is_convert {
            tools.magick.as_ref().and_then(|m| {
                let sibling = m.with_file_name("identify");
                if sibling.is_file() {
                    Some(sibling)
                } else {
                    find_on_path(&["identify"])
                }
            })
        } else {
            tools.magick.clone()
        };
        if let Some(prog) = identify_prog {
            let mut args: Vec<String> = Vec::new();
            if !tools.magick_is_convert {
                args.push("identify".into());
            }
            args.push("-format".into());
            args.push("%wx%h".into());
            args.push(path.to_string_lossy().into_owned());
            let magick = prog;
            let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            if let Some(out) = run_capture(&magick, &arg_refs) {
                let d = out.trim().to_string();
                if !d.is_empty() && d.contains('x') {
                    pr.dimensions = Some(d);
                }
            }
        }
    }
    pr
}

pub fn analyze(tools: &Tools, path: &Path) -> FileInfo {
    let is_dir = path.is_dir();
    let size = if is_dir {
        dir_size(path)
    } else {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    };
    let pr = if is_dir {
        Probe::default()
    } else {
        probe(tools, path)
    };
    FileInfo {
        path: path.to_string_lossy().into_owned(),
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned()),
        ext: extension_of(path),
        family: family_of(path, is_dir),
        is_dir,
        size,
        targets: targets_for(path, is_dir),
        duration: pr.duration,
        dimensions: pr.dimensions,
    }
}

// ---------------------------------------------------------------------------
// Output naming
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSettings {
    /// None = same folder as the source file.
    pub output_dir: Option<String>,
    /// Suffix inserted before the new extension: "converted" (default),
    /// or "" for no suffix (custom output dir only; same-dir falls back
    /// to "converted" so a conversion never overwrites its own source).
    pub suffix: String,
}

impl Default for OutputSettings {
    fn default() -> Self {
        OutputSettings {
            output_dir: None,
            suffix: "converted".into(),
        }
    }
}

pub fn output_path(input: &Path, target_ext: &str, settings: &OutputSettings) -> PathBuf {
    let dir = settings
        .output_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            input
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."))
        });
    let stem = {
        let name = input
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ext = extension_of(input);
        if ext.is_empty() {
            name.clone()
        } else {
            name[..name.len() - ext.len() - 1].to_string()
        }
    };
    let same_dir = settings.output_dir.is_none();
    let suffix = if settings.suffix.is_empty() && same_dir {
        "converted".to_string()
    } else {
        settings.suffix.clone()
    };
    let base = if suffix.is_empty() {
        stem.clone()
    } else {
        format!("{stem}.{suffix}")
    };
    let mut candidate = dir.join(format!("{base}.{target_ext}"));
    let mut n = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{base} ({n}).{target_ext}"));
        n += 1;
    }
    candidate
}

// ---------------------------------------------------------------------------
// Command construction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub tool_name: String,
}

fn s(v: impl Into<String>) -> String {
    v.into()
}

fn ffmpeg_video_args(input: &str, output: &str, target: &str) -> EngineResult<Vec<String>> {
    let mut a: Vec<String> = vec![s("-y"), s("-i"), s(input)];
    match target {
        "mp4" | "mov" | "mkv" => {
            a.extend(
                [
                    "-c:v", "libx264", "-preset", "medium", "-crf", "20", "-pix_fmt", "yuv420p",
                    "-c:a", "aac", "-b:a", "160k",
                ]
                .map(s),
            );
            if target == "mp4" {
                a.extend(["-movflags", "+faststart"].map(s));
            }
        }
        "webm" => {
            a.extend(
                [
                    "-c:v",
                    "libvpx-vp9",
                    "-b:v",
                    "0",
                    "-crf",
                    "30",
                    "-deadline",
                    "good",
                    "-cpu-used",
                    "2",
                    "-c:a",
                    "libopus",
                    "-b:a",
                    "128k",
                ]
                .map(s),
            );
        }
        "avi" => {
            a.extend(
                [
                    "-c:v",
                    "mpeg4",
                    "-q:v",
                    "4",
                    "-c:a",
                    "libmp3lame",
                    "-q:a",
                    "4",
                ]
                .map(s),
            );
        }
        "wmv" => {
            a.extend(
                [
                    "-c:v", "wmv2", "-b:v", "1400k", "-c:a", "wmav2", "-b:a", "128k",
                ]
                .map(s),
            );
        }
        "gif" => {
            a.extend([
                "-vf",
                "fps=12,scale=480:-1:flags=lanczos,split[s0][s1];[s0]palettegen[p];[s1][p]paletteuse",
                "-loop", "0",
            ].map(s));
        }
        "mp3" => {
            a.extend(["-vn", "-c:a", "libmp3lame", "-q:a", "2"].map(s));
        }
        _ => {
            return Err(EngineError::unsupported(format!(
                "Cannot convert video to {target}."
            )))
        }
    }
    a.push(s(output));
    Ok(a)
}

fn ffmpeg_audio_args(input: &str, output: &str, target: &str) -> EngineResult<Vec<String>> {
    let mut a: Vec<String> = vec![s("-y"), s("-i"), s(input), s("-vn")];
    match target {
        "mp3" => a.extend(["-c:a", "libmp3lame", "-q:a", "2"].map(s)),
        "m4a" => a.extend(["-c:a", "aac", "-b:a", "192k"].map(s)),
        "wav" => a.extend(["-c:a", "pcm_s16le"].map(s)),
        "flac" => a.extend(["-c:a", "flac"].map(s)),
        "ogg" => a.extend(["-c:a", "libvorbis", "-q:a", "5"].map(s)),
        "opus" => a.extend(["-c:a", "libopus", "-b:a", "160k"].map(s)),
        "aiff" => a.extend(["-c:a", "pcm_s16be"].map(s)),
        "wma" => a.extend(["-c:a", "wmav2", "-b:a", "128k"].map(s)),
        _ => {
            return Err(EngineError::unsupported(format!(
                "Cannot convert audio to {target}."
            )))
        }
    }
    a.push(s(output));
    Ok(a)
}

/// Build the command for a format conversion (not archive/compress jobs).
pub fn build_convert_command(
    tools: &Tools,
    input: &Path,
    output: &Path,
    target: &str,
    image_quality: u8,
) -> EngineResult<CommandSpec> {
    let family = family_of(input, input.is_dir());
    let input_s = input.to_string_lossy().into_owned();
    let output_s = output.to_string_lossy().into_owned();
    match family {
        Family::Video => {
            let ffmpeg = tools
                .ffmpeg
                .clone()
                .ok_or_else(|| EngineError::tool_missing("FFmpeg"))?;
            let args = ffmpeg_video_args(&input_s, &output_s, target)?;
            Ok(CommandSpec {
                program: ffmpeg,
                args,
                tool_name: "FFmpeg".into(),
            })
        }
        Family::Audio => {
            let ffmpeg = tools
                .ffmpeg
                .clone()
                .ok_or_else(|| EngineError::tool_missing("FFmpeg"))?;
            let args = ffmpeg_audio_args(&input_s, &output_s, target)?;
            Ok(CommandSpec {
                program: ffmpeg,
                args,
                tool_name: "FFmpeg".into(),
            })
        }
        Family::Image => build_image_command(tools, &input_s, &output_s, target, image_quality),
        _ => Err(EngineError::unsupported(
            "This file type has no format conversions in this version.",
        )),
    }
}

fn build_image_command(
    tools: &Tools,
    input: &str,
    output: &str,
    target: &str,
    quality: u8,
) -> EngineResult<CommandSpec> {
    // AVIF: prefer ImageMagick when its build can write AVIF, else ffmpeg.
    if target == "avif" && !tools.magick_can_write("AVIF") {
        let ffmpeg = tools
            .ffmpeg
            .clone()
            .ok_or_else(|| EngineError::tool_missing("FFmpeg"))?;
        if !tools.ffmpeg_has_muxer("avif") {
            return Err(EngineError::new(
                "avif_unavailable",
                "AVIF export is not available in the bundled tools on this system.",
            ));
        }
        let encoder = if tools.ffmpeg_has_encoder("libsvtav1") {
            vec!["-c:v", "libsvtav1", "-preset", "8", "-crf", "35"]
        } else {
            vec!["-c:v", "libaom-av1", "-cpu-used", "6", "-crf", "32"]
        };
        let mut args: Vec<String> = vec![s("-y"), s("-i"), s(input)];
        args.extend(encoder.into_iter().map(s));
        args.extend(["-still-picture", "1", "-pix_fmt", "yuv420p"].map(s));
        args.push(s(output));
        return Ok(CommandSpec {
            program: ffmpeg,
            args,
            tool_name: "FFmpeg".into(),
        });
    }
    if target == "heic" && !tools.magick_can_write("HEIC") && !tools.ffmpeg_has_muxer("heif") {
        return Err(EngineError::new(
            "heic_encode_unavailable",
            "HEIC export is not available in the bundled tools on this platform yet (HEIC files can still be opened and converted to other formats).",
        ));
    }
    let (magick, _) = tools
        .magick_argv()
        .ok_or_else(|| EngineError::tool_missing("ImageMagick"))?;
    let mut args: Vec<String> = Vec::new();
    if !tools.magick_is_convert {
        // standalone `magick` binary: first arg is implicit; nothing needed.
    }
    args.push(s(input));
    if matches!(target, "jpg" | "webp") {
        args.extend(["-quality".into(), quality.to_string()]);
    }
    if target == "jpg" {
        args.extend(["-background".into(), "white".into(), "-flatten".into()]);
        // -flatten must come before output; ImageMagick applies ops in order,
        // background/flatten after input is correct ordering.
    }
    args.push(s(output));
    Ok(CommandSpec {
        program: magick,
        args,
        tool_name: "ImageMagick".into(),
    })
}

// ---------------------------------------------------------------------------
// Subtitles (Stage 2) — pure Rust, no external tool.
//
// SRT and VTT cue blocks are structurally identical once header lines,
// cue indices/identifiers and VTT NOTE/STYLE blocks are skipped, so one
// parser serves both. Timestamps are handled as integer milliseconds:
// "HH:MM:SS,mmm" (SRT), "HH:MM:SS.mmm" or "MM:SS.mmm" (VTT). Hours may
// exceed 99 (long recordings) and are never truncated.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub lines: Vec<String>,
}

/// Parse "HH:MM:SS,mmm" / "MM:SS.mmm" (comma or dot, any hour width)
/// into milliseconds. Fractional digits are right-padded / truncated to
/// exactly 3 (".5" = 500 ms, ",05" = 50 ms).
pub fn parse_timestamp(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let (whole, frac) = match raw.rfind(|c| c == ',' || c == '.') {
        Some(i) => (&raw[..i], &raw[i + 1..]),
        None => (raw, ""),
    };
    let parts: Vec<&str> = whole.split(':').collect();
    let (h, m, sec) = match parts.len() {
        3 => (
            parts[0].parse::<u64>().ok()?,
            parts[1].parse::<u64>().ok()?,
            parts[2].parse::<u64>().ok()?,
        ),
        2 => (
            0,
            parts[0].parse::<u64>().ok()?,
            parts[1].parse::<u64>().ok()?,
        ),
        _ => return None,
    };
    let mut f = frac.to_string();
    while f.len() < 3 {
        f.push('0');
    }
    f.truncate(3);
    let ms: u64 = if f.is_empty() { 0 } else { f.parse().ok()? };
    Some(((h * 60 + m) * 60 + sec) * 1000 + ms)
}

/// Format milliseconds as "HH:MM:SS<sep>mmm". Hours print with at least
/// two digits and grow naturally (100 hours → "100:00:00,000").
pub fn format_timestamp(ms: u64, sep: char) -> String {
    let h = ms / 3_600_000;
    let m = (ms % 3_600_000) / 60_000;
    let sec = (ms % 60_000) / 1000;
    let mmm = ms % 1000;
    format!("{h:02}:{m:02}:{sec:02}{sep}{mmm:03}")
}

/// Parse SRT or VTT text into cues. Blocks without a "-->" timing line
/// (the WEBVTT header, NOTE/STYLE/REGION blocks) are skipped; lines
/// before the timing line (SRT indices, VTT cue identifiers) are
/// ignored; VTT cue settings after the end time are dropped.
pub fn parse_cues(text: &str) -> Vec<Cue> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut cues = Vec::new();
    for block in normalized.split("\n\n") {
        let lines: Vec<&str> = block.lines().collect();
        let Some(tpos) = lines.iter().position(|l| l.contains("-->")) else {
            continue;
        };
        let Some((a, b)) = lines[tpos].split_once("-->") else {
            continue;
        };
        let start = parse_timestamp(a);
        let end = b.split_whitespace().next().and_then(parse_timestamp);
        let (Some(start_ms), Some(end_ms)) = (start, end) else {
            continue;
        };
        let text_lines: Vec<String> = lines[tpos + 1..].iter().map(|l| l.to_string()).collect();
        cues.push(Cue {
            start_ms,
            end_ms,
            lines: text_lines,
        });
    }
    cues
}

pub fn write_srt(cues: &[Cue]) -> String {
    let mut out = String::new();
    for (i, c) in cues.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            format_timestamp(c.start_ms, ','),
            format_timestamp(c.end_ms, ','),
            c.lines.join("\n")
        ));
    }
    out
}

pub fn write_vtt(cues: &[Cue]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for c in cues {
        out.push_str(&format!(
            "{} --> {}\n{}\n\n",
            format_timestamp(c.start_ms, '.'),
            format_timestamp(c.end_ms, '.'),
            c.lines.join("\n")
        ));
    }
    out
}

/// Dialogue lines only, in order — the TXT export.
pub fn cues_to_txt(cues: &[Cue]) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for c in cues {
        for l in &c.lines {
            lines.push(l);
        }
    }
    if lines.is_empty() {
        String::new()
    } else {
        lines.join("\n") + "\n"
    }
}

/// Convert subtitle file content between srt / vtt / txt.
pub fn subtitle_convert(text: &str, target: &str) -> EngineResult<String> {
    let cues = parse_cues(text);
    if cues.is_empty() {
        return Err(EngineError::unsupported(
            "No subtitle cues were found in this file, so there is nothing to convert.",
        ));
    }
    match target {
        "srt" => Ok(write_srt(&cues)),
        "vtt" => Ok(write_vtt(&cues)),
        "txt" => Ok(cues_to_txt(&cues)),
        other => Err(EngineError::unsupported(format!(
            "Cannot convert subtitles to {other}."
        ))),
    }
}

// ---------------------------------------------------------------------------
// Stage 2 runners — documents, PDF, slides, spreadsheets.
//
// Routing (decided by prototyping the real tools, see PARITY.md):
//   · LibreOffice (`soffice --headless --convert-to`) owns layout-bound
//     conversions: anything → PDF, office formats between themselves,
//     spreadsheets, slides. Each job gets a private user-profile dir so
//     concurrent jobs never fight over LibreOffice's single-instance lock.
//   · Pandoc owns semantic text formats: Markdown, HTML, EPUB, plain
//     text, and DOCX-from-text. LibreOffice is the fallback for
//     HTML/TXT/DOCX/EPUB when Pandoc is absent; Markdown output and any
//     EPUB *input* strictly require Pandoc (LibreOffice cannot read EPUB).
//   · poppler (pdftoppm / pdftotext / pdfinfo) owns PDF rasterization
//     (all pages, 300 DPI) and PDF text extraction. LibreOffice has NO
//     export filter from PDF to office formats ("Error: no export
//     filter" — verified), so PDF → DOCX/PPTX route through extracted
//     text + Pandoc and are labelled text-faithful, never layout-true.
// ---------------------------------------------------------------------------

static STAGE2_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn staging_dir(tag: &str) -> EngineResult<PathBuf> {
    let n = STAGE2_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("uruk-stage2-{}-{tag}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .map_err(|e| EngineError::new("io", format!("Cannot create temp folder: {e}")))?;
    Ok(dir)
}

/// File stem using the same extension rules as `output_path`
/// (`.tar.gz` counts as one extension).
fn stem_of(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = extension_of(path);
    if ext.is_empty() {
        name.clone()
    } else {
        name[..name.len() - ext.len() - 1].to_string()
    }
}

fn move_file(from: &Path, to: &Path) -> EngineResult<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| EngineError::new("io", format!("Cannot create output folder: {e}")))?;
    }
    if std::fs::rename(from, to).is_err() {
        std::fs::copy(from, to)
            .map_err(|e| EngineError::new("io", format!("Cannot save the converted file: {e}")))?;
        let _ = std::fs::remove_file(from);
    }
    Ok(())
}

/// Run LibreOffice headless: convert `input` with the `--convert-to`
/// spec (e.g. "pdf", "docx", "csv:Text - txt - csv (StarCalc):44,34,76")
/// into `staging`, then move the produced file to `dest`. `infilter`
/// optionally pins the import filter (delimited-text inputs need their
/// separator declared, or Calc imports each line as a single cell).
/// Returns the destination path.
#[allow(clippy::too_many_arguments)]
fn lo_convert(
    tools: &Tools,
    input: &Path,
    spec: &str,
    produced_ext: &str,
    infilter: Option<&str>,
    staging: &Path,
    dest: &Path,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(f32),
) -> EngineResult<PathBuf> {
    let soffice = tools
        .soffice
        .clone()
        .ok_or_else(|| EngineError::tool_missing("LibreOffice"))?;
    let out_dir = staging.join("lo-out");
    let profile = staging.join("lo-profile");
    std::fs::create_dir_all(&out_dir)
        .map_err(|e| EngineError::new("io", format!("Cannot create temp folder: {e}")))?;
    let mut args: Vec<String> = vec![
        s("--headless"),
        s("--norestore"),
        format!("-env:UserInstallation=file://{}", profile.to_string_lossy()),
    ];
    if let Some(f) = infilter {
        args.push(format!("--infilter={f}"));
    }
    args.extend([
        s("--convert-to"),
        s(spec),
        s("--outdir"),
        out_dir.to_string_lossy().into_owned(),
        input.to_string_lossy().into_owned(),
    ]);
    let cmd = CommandSpec {
        program: soffice,
        args,
        tool_name: "LibreOffice".into(),
    };
    progress(-1.0);
    run_streaming(&cmd, None, cancel, progress)?;
    let produced = out_dir.join(format!("{}.{}", stem_of(input), produced_ext));
    if !produced.is_file() {
        return Err(EngineError::new(
            "tool_failed",
            "LibreOffice finished without producing a file — this conversion route is not available for this document.",
        ));
    }
    move_file(&produced, dest)?;
    Ok(dest.to_path_buf())
}

/// Run Pandoc: `pandoc <input> -o <output>` plus extra args.
fn pandoc_convert(
    tools: &Tools,
    input: &Path,
    output: &Path,
    extra: &[String],
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(f32),
) -> EngineResult<()> {
    let pandoc = tools
        .pandoc
        .clone()
        .ok_or_else(|| EngineError::tool_missing("Pandoc"))?;
    let mut args: Vec<String> = vec![
        input.to_string_lossy().into_owned(),
        s("-o"),
        output.to_string_lossy().into_owned(),
    ];
    args.extend(extra.iter().cloned());
    let cmd = CommandSpec {
        program: pandoc,
        args,
        tool_name: "Pandoc".into(),
    };
    progress(-1.0);
    run_streaming(&cmd, None, cancel, progress)?;
    if !output.is_file() {
        return Err(EngineError::new(
            "tool_failed",
            "Pandoc finished without producing a file.",
        ));
    }
    Ok(())
}

/// Extract a PDF's text with pdftotext into a file inside `staging`.
fn pdf_extract_text(
    tools: &Tools,
    input: &Path,
    staging: &Path,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(f32),
) -> EngineResult<PathBuf> {
    let pdftotext = tools
        .pdftotext
        .clone()
        .ok_or_else(|| EngineError::tool_missing("poppler (pdftotext)"))?;
    let txt = staging.join("extracted.txt");
    let cmd = CommandSpec {
        program: pdftotext,
        args: vec![
            s("-enc"),
            s("UTF-8"),
            input.to_string_lossy().into_owned(),
            txt.to_string_lossy().into_owned(),
        ],
        tool_name: "poppler".into(),
    };
    progress(-1.0);
    run_streaming(&cmd, None, cancel, progress)?;
    if !txt.is_file() {
        return Err(EngineError::new(
            "tool_failed",
            "pdftotext produced no text file.",
        ));
    }
    Ok(txt)
}

fn pdf_page_count(tools: &Tools, pdf: &Path) -> Option<u32> {
    let pdfinfo = tools.pdfinfo.as_ref()?;
    let p = pdf.to_string_lossy().into_owned();
    let out = run_capture(pdfinfo, &[p.as_str()])?;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("Pages:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Rasterize every page of `pdf` at 300 DPI into the output folder,
/// named `<base>-page-001.<ext>` etc. `naming_input` is the user's
/// original file (its name/settings drive the output names).
/// Outcome.output is the first page; the note carries the page count.
fn rasterize_pdf(
    tools: &Tools,
    pdf: &Path,
    naming_input: &Path,
    target: &str,
    settings: &OutputSettings,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(f32),
) -> EngineResult<JobOutcome> {
    let pdftoppm = tools
        .pdftoppm
        .clone()
        .ok_or_else(|| EngineError::tool_missing("poppler (pdftoppm)"))?;
    let base = output_path(naming_input, target, settings);
    let out_dir = base
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&out_dir)
        .map_err(|e| EngineError::new("io", format!("Cannot create output folder: {e}")))?;
    let base_stem = stem_of(&base);
    // Find a free page-name stem: <stem>, then "<stem> (2)", …
    let mut page_stem = base_stem.clone();
    let mut n = 2;
    while out_dir
        .join(format!("{page_stem}-page-001.{target}"))
        .exists()
    {
        page_stem = format!("{base_stem} ({n})");
        n += 1;
    }
    // pdftoppm appends "-<page>" itself, so the prefix ends at "-page"
    // (a trailing hyphen here would produce "page--1" names).
    let prefix = out_dir.join(format!("{page_stem}-page"));
    let mut args: Vec<String> = vec![s("-r"), s("300")];
    if target == "jpg" {
        args.extend(["-jpeg".into(), "-jpegopt".into(), "quality=90".into()]);
    } else {
        args.push("-png".into());
    }
    args.push(pdf.to_string_lossy().into_owned());
    args.push(prefix.to_string_lossy().into_owned());
    let cmd = CommandSpec {
        program: pdftoppm,
        args,
        tool_name: "poppler".into(),
    };
    progress(-1.0);
    run_streaming(&cmd, None, cancel, progress)?;
    // pdftoppm writes <prefix><N>.<ext> with N padded to the page-count
    // width (page-1, page-01, page-001 …). Collect them in order, then
    // normalize every name to the uniform `-page-###` form.
    let want_prefix = format!("{page_stem}-page-");
    let want_suffix = format!(".{target}");
    let mut produced: Vec<(u32, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&out_dir) {
        for e in rd.flatten() {
            let p = e.path();
            let name = p
                .file_name()
                .map(|x| x.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let Some(mid) = name
                .strip_prefix(&want_prefix)
                .and_then(|rest| rest.strip_suffix(&want_suffix))
            {
                if let Ok(num) = mid.parse::<u32>() {
                    produced.push((num, p));
                }
            }
        }
    }
    produced.sort();
    if produced.is_empty() {
        return Err(EngineError::new(
            "tool_failed",
            "pdftoppm produced no page images.",
        ));
    }
    // Two passes through temp names so renames can never collide.
    let mut finals: Vec<PathBuf> = Vec::new();
    for (i, (_, p)) in produced.iter().enumerate() {
        let tmp = out_dir.join(format!("{page_stem}-urutmp-{:03}.{target}", i + 1));
        std::fs::rename(p, &tmp)
            .map_err(|e| EngineError::new("io", format!("Cannot rename page image: {e}")))?;
        finals.push(tmp);
    }
    let mut pages: Vec<PathBuf> = Vec::new();
    for (i, tmp) in finals.iter().enumerate() {
        let fin = out_dir.join(format!("{page_stem}-page-{:03}.{target}", i + 1));
        std::fs::rename(tmp, &fin)
            .map_err(|e| EngineError::new("io", format!("Cannot rename page image: {e}")))?;
        pages.push(fin);
    }
    let count = pages.len();
    let first = pages[0].clone();
    Ok(JobOutcome {
        output: first,
        note: Some(format!("{count} pages exported at 300 DPI")),
    })
}

/// The extracted-text fidelity note attached to PDF → office/text routes.
const PDF_TEXT_NOTE: &str =
    "Text-faithful conversion: the PDF's text was rebuilt into the new format; the original page layout, fonts and images are not preserved.";

fn convert_pdf(
    tools: &Tools,
    input: &Path,
    target: &str,
    settings: &OutputSettings,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(f32),
) -> EngineResult<JobOutcome> {
    match target {
        "jpg" | "png" => rasterize_pdf(tools, input, input, target, settings, cancel, progress),
        "xlsx" => Err(EngineError::new(
            "pdf_to_spreadsheet_unavailable",
            "PDF to XLSX is not offered: a PDF stores drawn text, not spreadsheet cells, and no bundled tool can honestly rebuild real cells, formulas or column structure from it.",
        )),
        "txt" | "md" => {
            let staging = staging_dir("pdftext")?;
            let result = (|| -> EngineResult<JobOutcome> {
                let txt = pdf_extract_text(tools, input, &staging, cancel, progress)?;
                let out = output_path(input, target, settings);
                move_file(&txt, &out)?;
                let note = if target == "md" {
                    Some("Markdown here is the PDF's extracted text; the original layout is not preserved.".to_string())
                } else {
                    None
                };
                Ok(JobOutcome { output: out, note })
            })();
            let _ = std::fs::remove_dir_all(&staging);
            result
        }
        "epub" | "docx" | "pptx" => {
            if tools.pandoc.is_none() {
                return Err(EngineError::tool_missing("Pandoc"));
            }
            let staging = staging_dir("pdfoffice")?;
            let result = (|| -> EngineResult<JobOutcome> {
                let txt = pdf_extract_text(tools, input, &staging, cancel, progress)?;
                let out = output_path(input, target, settings);
                match target {
                    "docx" => {
                        // Pandoc reads the .txt as Markdown; plain text
                        // passes through as paragraphs.
                        let tmp_out = staging.join("out.docx");
                        pandoc_convert(
                            tools,
                            &txt,
                            &tmp_out,
                            &[format!("--metadata=title={}", stem_of(input))],
                            cancel,
                            progress,
                        )?;
                        move_file(&tmp_out, &out)?;
                    }
                    "epub" => {
                        let tmp_out = staging.join("out.epub");
                        pandoc_convert(
                            tools,
                            &txt,
                            &tmp_out,
                            &[format!("--metadata=title={}", stem_of(input))],
                            cancel,
                            progress,
                        )?;
                        move_file(&tmp_out, &out)?;
                    }
                    _ => {
                        // pptx: one slide per PDF page. pdftotext
                        // separates pages with form feeds (\u{c}).
                        let raw = std::fs::read_to_string(&txt).unwrap_or_default();
                        let pages: Vec<String> = raw
                            .split('\u{c}')
                            .map(|p| p.trim().to_string())
                            .filter(|p| !p.is_empty())
                            .collect();
                        if pages.is_empty() {
                            return Err(EngineError::unsupported(
                                "This PDF has no extractable text to turn into slides.",
                            ));
                        }
                        let md: String = pages
                            .iter()
                            .enumerate()
                            .map(|(i, p)| format!("# Page {}\n\n{}", i + 1, p))
                            .collect::<Vec<_>>()
                            .join("\n\n---\n\n");
                        let md_path = staging.join("slides.md");
                        std::fs::write(&md_path, md)
                            .map_err(|e| EngineError::new("io", format!("Cannot stage slides: {e}")))?;
                        let tmp_out = staging.join("out.pptx");
                        pandoc_convert(tools, &md_path, &tmp_out, &[], cancel, progress)?;
                        move_file(&tmp_out, &out)?;
                    }
                }
                Ok(JobOutcome { output: out, note: Some(PDF_TEXT_NOTE.to_string()) })
            })();
            let _ = std::fs::remove_dir_all(&staging);
            result
        }
        other => Err(EngineError::unsupported(format!("Cannot convert PDF to {other}."))),
    }
}

fn convert_document(
    tools: &Tools,
    input: &Path,
    target: &str,
    settings: &OutputSettings,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(f32),
) -> EngineResult<JobOutcome> {
    let in_ext = extension_of(input);
    let out = output_path(input, target, settings);
    let staging = staging_dir("doc")?;
    let result = (|| -> EngineResult<JobOutcome> {
        // --- PDF export: LibreOffice owns layout. MD/EPUB inputs take a
        // Pandoc → DOCX pre-step so structure survives into the PDF.
        if target == "pdf" {
            match in_ext.as_str() {
                "epub" | "md" | "markdown" => {
                    if tools.pandoc.is_none() {
                        return Err(EngineError::tool_missing("Pandoc"));
                    }
                    let tmp_docx = staging.join("pre.docx");
                    pandoc_convert(tools, input, &tmp_docx, &[], cancel, progress)?;
                    lo_convert(
                        tools, &tmp_docx, "pdf", "pdf", None, &staging, &out, cancel, progress,
                    )?;
                }
                _ => {
                    lo_convert(
                        tools, input, "pdf", "pdf", None, &staging, &out, cancel, progress,
                    )?;
                }
            }
            return Ok(JobOutcome {
                output: out.clone(),
                note: None,
            });
        }
        // --- EPUB input: only Pandoc can read it.
        if in_ext == "epub" {
            if tools.pandoc.is_none() {
                return Err(EngineError::tool_missing("Pandoc"));
            }
            let mut extra: Vec<String> = Vec::new();
            if target == "txt" {
                extra = vec![s("-t"), s("plain")];
            }
            if target == "epub" {
                extra = vec![format!("--metadata=title={}", stem_of(input))];
            }
            let tmp = staging.join(format!("out.{target}"));
            pandoc_convert(tools, input, &tmp, &extra, cancel, progress)?;
            move_file(&tmp, &out)?;
            return Ok(JobOutcome {
                output: out.clone(),
                note: None,
            });
        }
        // --- Markdown output strictly needs Pandoc.
        if target == "md" {
            if tools.pandoc.is_none() {
                return Err(EngineError::tool_missing("Pandoc"));
            }
            let tmp = staging.join("out.md");
            pandoc_convert(tools, input, &tmp, &[], cancel, progress)?;
            move_file(&tmp, &out)?;
            return Ok(JobOutcome {
                output: out.clone(),
                note: None,
            });
        }
        // --- html / epub / txt / docx: Pandoc preferred, LibreOffice
        // fallback (its filters cover all four targets).
        if tools.pandoc.is_some() {
            let mut extra: Vec<String> = Vec::new();
            if target == "txt" {
                extra = vec![s("-t"), s("plain")];
            }
            if target == "epub" {
                extra = vec![format!("--metadata=title={}", stem_of(input))];
            }
            let tmp = staging.join(format!("out.{target}"));
            pandoc_convert(tools, input, &tmp, &extra, cancel, progress)?;
            move_file(&tmp, &out)?;
        } else {
            lo_convert(
                tools, input, target, target, None, &staging, &out, cancel, progress,
            )?;
        }
        Ok(JobOutcome {
            output: out.clone(),
            note: None,
        })
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn convert_slides(
    tools: &Tools,
    input: &Path,
    target: &str,
    settings: &OutputSettings,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(f32),
) -> EngineResult<JobOutcome> {
    match target {
        "pdf" => {
            let staging = staging_dir("slides")?;
            let out = output_path(input, "pdf", settings);
            let result = lo_convert(
                tools, input, "pdf", "pdf", None, &staging, &out, cancel, progress,
            )
            .map(|o| JobOutcome {
                output: o,
                note: None,
            });
            let _ = std::fs::remove_dir_all(&staging);
            result
        }
        "jpg" | "png" => {
            // LibreOffice renders the deck to PDF, poppler rasterizes
            // every slide at 300 DPI — the same pipeline as PDF → image.
            let staging = staging_dir("slides")?;
            let result = (|| -> EngineResult<JobOutcome> {
                let pdf = staging.join("deck.pdf");
                lo_convert(
                    tools, input, "pdf", "pdf", None, &staging, &pdf, cancel, progress,
                )?;
                rasterize_pdf(tools, &pdf, input, target, settings, cancel, progress)
            })();
            let _ = std::fs::remove_dir_all(&staging);
            result
        }
        other => Err(EngineError::unsupported(format!(
            "Cannot convert slides to {other}."
        ))),
    }
}

/// LibreOffice filter spec for a spreadsheet target.
fn sheet_lo_spec(target: &str) -> EngineResult<&'static str> {
    match target {
        "xlsx" => Ok("xlsx"),
        "ods" => Ok("ods"),
        "pdf" => Ok("pdf"),
        // Calc's CSV filter: field separator, text delimiter, charset
        // (44 = comma / 9 = tab, 34 = '"', 76 = UTF-8).
        "csv" => Ok("csv:Text - txt - csv (StarCalc):44,34,76"),
        "tsv" => Ok("tsv:Text - txt - csv (StarCalc):9,34,76"),
        other => Err(EngineError::unsupported(format!(
            "Cannot convert a spreadsheet to {other}."
        ))),
    }
}

fn convert_spreadsheet(
    tools: &Tools,
    input: &Path,
    target: &str,
    settings: &OutputSettings,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(f32),
) -> EngineResult<JobOutcome> {
    let spec = sheet_lo_spec(target)?;
    let staging = staging_dir("sheet")?;
    let out = output_path(input, target, settings);
    let note = if target == "csv" || target == "tsv" {
        Some("Only the first sheet is exported to a delimited text file.".to_string())
    } else {
        None
    };
    // Delimited-text inputs must declare their separator on import, or
    // Calc reads every line as a single cell (proven in testing).
    let infilter = match extension_of(input).as_str() {
        "csv" => Some("Text - txt - csv (StarCalc):44,34,76"),
        "tsv" => Some("Text - txt - csv (StarCalc):9,34,76"),
        _ => None,
    };
    let result = lo_convert(
        tools, input, spec, target, infilter, &staging, &out, cancel, progress,
    )
    .map(|o| JobOutcome { output: o, note });
    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn convert_subtitle(
    input: &Path,
    target: &str,
    settings: &OutputSettings,
    progress: &mut dyn FnMut(f32),
) -> EngineResult<JobOutcome> {
    let raw = std::fs::read(input)
        .map_err(|e| EngineError::new("io", format!("Cannot read the subtitle file: {e}")))?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    progress(-1.0);
    let converted = subtitle_convert(&text, target)?;
    let out = output_path(input, target, settings);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| EngineError::new("io", format!("Cannot create output folder: {e}")))?;
    }
    std::fs::write(&out, converted)
        .map_err(|e| EngineError::new("io", format!("Cannot save the subtitle file: {e}")))?;
    progress(100.0);
    Ok(JobOutcome {
        output: out,
        note: None,
    })
}

// ---------------------------------------------------------------------------
// Job execution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobSpec {
    Convert {
        input: String,
        target: String,
        #[serde(default)]
        output_dir: Option<String>,
        #[serde(default = "default_suffix")]
        suffix: String,
        #[serde(default = "default_quality")]
        image_quality: u8,
    },
    CompressImage {
        input: String,
        /// "percent" (reduce by N percent of original size) or "target"
        /// (fit under `value` bytes).
        mode: String,
        value: f64,
        #[serde(default)]
        output_dir: Option<String>,
        #[serde(default = "default_suffix")]
        suffix: String,
    },
    ExtractArchive {
        input: String,
        #[serde(default)]
        output_dir: Option<String>,
    },
    CreateArchive {
        inputs: Vec<String>,
        format: String, // zip | tar | gz
        #[serde(default)]
        output_dir: Option<String>,
        #[serde(default = "default_suffix")]
        suffix: String,
    },
}

fn default_suffix() -> String {
    "converted".into()
}
fn default_quality() -> u8 {
    92
}

pub struct JobOutcome {
    pub output: PathBuf,
    pub note: Option<String>,
}

fn parse_ffmpeg_time(line: &str) -> Option<f64> {
    let idx = line.find("time=")?;
    let rest = &line[idx + 5..];
    let token: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ':' || *c == '.')
        .collect();
    let parts: Vec<&str> = token.split(':').collect();
    if parts.len() == 3 {
        let h: f64 = parts[0].parse().ok()?;
        let m: f64 = parts[1].parse().ok()?;
        let sec: f64 = parts[2].parse().ok()?;
        Some(h * 3600.0 + m * 60.0 + sec)
    } else {
        None
    }
}

fn parse_percent(line: &str) -> Option<f32> {
    // 7z progress lines look like " 1M + data" or " 45% 12 files".
    for tok in line.split_whitespace() {
        if let Some(p) = tok.strip_suffix('%') {
            if let Ok(v) = p.parse::<f32>() {
                return Some(v);
            }
        }
    }
    None
}

/// Spawn a command, stream progress (0..=100, or -1 for indeterminate),
/// honour cancellation, and collect the stderr/stdout tail for errors.
fn run_streaming(
    spec: &CommandSpec,
    duration_hint: Option<f64>,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(f32),
) -> EngineResult<()> {
    run_streaming_in(spec, None, duration_hint, cancel, progress)
}

fn run_streaming_in(
    spec: &CommandSpec,
    working_dir: Option<&Path>,
    duration_hint: Option<f64>,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(f32),
) -> EngineResult<()> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    if let Some(dir) = working_dir {
        command.current_dir(dir);
    }
    let mut child: Child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            EngineError::new(
                "spawn_failed",
                format!("Could not start {}: {e}", spec.tool_name),
            )
        })?;

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut handles = Vec::new();
    if let Some(out) = child.stdout.take() {
        let tx2 = tx.clone();
        handles.push(std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                match line {
                    Ok(l) => {
                        let _ = tx2.send(l);
                    }
                    Err(_) => break,
                }
            }
        }));
    }
    if let Some(err) = child.stderr.take() {
        let tx3 = tx.clone();
        handles.push(std::thread::spawn(move || {
            for line in BufReader::new(err).lines() {
                match line {
                    Ok(l) => {
                        let _ = tx3.send(l);
                    }
                    Err(_) => break,
                }
            }
        }));
    }
    drop(tx);

    let mut tail: Vec<String> = Vec::new();
    let mut duration = duration_hint;
    loop {
        // Drain available lines without blocking the cancel check.
        loop {
            match rx.try_recv() {
                Ok(line) => {
                    if let Some(idx) = line.find("Duration:") {
                        let rest = line[idx + "Duration:".len()..].trim_start();
                        let token: String = rest
                            .chars()
                            .take_while(|c| c.is_ascii_digit() || *c == ':' || *c == '.')
                            .collect();
                        let parts: Vec<&str> = token.split(':').collect();
                        if parts.len() == 3 {
                            if let (Ok(h), Ok(m), Ok(sec)) = (
                                parts[0].parse::<f64>(),
                                parts[1].parse::<f64>(),
                                parts[2].parse::<f64>(),
                            ) {
                                duration = Some(h * 3600.0 + m * 60.0 + sec);
                            }
                        }
                    }
                    if let Some(t) = parse_ffmpeg_time(&line) {
                        if let Some(d) = duration {
                            if d > 0.0 {
                                progress(((t / d) * 100.0).min(99.0) as f32);
                            }
                        }
                    } else if let Some(p) = parse_percent(&line) {
                        progress(p.min(99.0));
                    }
                    tail.push(line);
                    if tail.len() > 24 {
                        tail.remove(0);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(EngineError::cancelled());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // Final drain.
                while let Ok(line) = rx.try_recv() {
                    tail.push(line);
                }
                if status.success() {
                    progress(100.0);
                    return Ok(());
                }
                return Err(EngineError::failed(&spec.tool_name, &tail.join("\n")));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(e) => {
                return Err(EngineError::new(
                    "spawn_failed",
                    format!("{} failed: {e}", spec.tool_name),
                ));
            }
        }
    }
}

/// Run one job end to end.
pub fn run_job(
    tools: &Tools,
    job: &JobSpec,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(f32),
) -> EngineResult<JobOutcome> {
    match job {
        JobSpec::Convert {
            input,
            target,
            output_dir,
            suffix,
            image_quality,
        } => {
            let input_path = PathBuf::from(input);
            let settings = OutputSettings {
                output_dir: output_dir.clone(),
                suffix: suffix.clone(),
            };
            let output = output_path(&input_path, target, &settings);
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    EngineError::new("io", format!("Cannot create output folder: {e}"))
                })?;
            }
            // Media → MP3 needs an audio stream; fail in plain language.
            let family = family_of(&input_path, false);
            let pr = probe(tools, &input_path);
            if target == "mp3" && family == Family::Video && !pr.has_audio {
                return Err(EngineError::new(
                    "no_audio_stream",
                    "This video has no audio track to export as MP3.",
                ));
            }
            // Archive → archive means "repack the contents", never nesting
            // the archive file inside a new one.
            if family == Family::Archive {
                return repack_archive(tools, &input_path, target, &settings, cancel, progress);
            }
            // Stage 2 families have their own multi-step runners.
            match family {
                Family::Pdf => {
                    return convert_pdf(tools, &input_path, target, &settings, cancel, progress);
                }
                Family::Document => {
                    return convert_document(
                        tools,
                        &input_path,
                        target,
                        &settings,
                        cancel,
                        progress,
                    );
                }
                Family::Slides => {
                    return convert_slides(tools, &input_path, target, &settings, cancel, progress);
                }
                Family::Spreadsheet => {
                    return convert_spreadsheet(
                        tools,
                        &input_path,
                        target,
                        &settings,
                        cancel,
                        progress,
                    );
                }
                Family::Subtitle => {
                    return convert_subtitle(&input_path, target, &settings, progress);
                }
                _ => {}
            }
            let spec = build_convert_command(tools, &input_path, &output, target, *image_quality)?;
            progress(-1.0);
            run_streaming(&spec, pr.duration, cancel, progress)?;
            Ok(JobOutcome { output, note: None })
        }
        JobSpec::CompressImage {
            input,
            mode,
            value,
            output_dir,
            suffix,
        } => compress_image(
            tools, input, mode, *value, output_dir, suffix, cancel, progress,
        ),
        JobSpec::ExtractArchive { input, output_dir } => {
            let sevenz = tools
                .sevenz
                .clone()
                .ok_or_else(|| EngineError::tool_missing("7-Zip"))?;
            let input_path = PathBuf::from(input);
            let stem = {
                let name = input_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let ext = extension_of(&input_path);
                if ext.is_empty() {
                    name.clone()
                } else {
                    name[..name.len() - ext.len() - 1].to_string()
                }
            };
            let base_dir = output_dir.as_ref().map(PathBuf::from).unwrap_or_else(|| {
                input_path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."))
            });
            let mut dest = base_dir.join(format!("{stem}.extracted"));
            let mut n = 2;
            while dest.exists() {
                dest = base_dir.join(format!("{stem}.extracted ({n})"));
                n += 1;
            }
            std::fs::create_dir_all(&dest)
                .map_err(|e| EngineError::new("io", format!("Cannot create output folder: {e}")))?;
            let spec = CommandSpec {
                program: sevenz,
                args: vec![
                    s("x"),
                    s(input),
                    format!("-o{}", dest.to_string_lossy()),
                    s("-y"),
                ],
                tool_name: "7-Zip".into(),
            };
            progress(-1.0);
            run_streaming(&spec, None, cancel, progress)?;
            Ok(JobOutcome {
                output: dest,
                note: None,
            })
        }
        JobSpec::CreateArchive {
            inputs,
            format,
            output_dir,
            suffix,
        } => create_archive(tools, inputs, format, output_dir, suffix, cancel, progress),
    }
}

/// Repack an archive's contents into a new archive format: extract to a
/// temporary folder, then create the target archive from what was inside.
fn repack_archive(
    tools: &Tools,
    input_path: &Path,
    target: &str,
    settings: &OutputSettings,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(f32),
) -> EngineResult<JobOutcome> {
    let sevenz = tools
        .sevenz
        .clone()
        .ok_or_else(|| EngineError::tool_missing("7-Zip"))?;
    let tmp = std::env::temp_dir().join(format!("uruk-repack-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)
        .map_err(|e| EngineError::new("io", format!("Cannot create temp folder: {e}")))?;
    let extract = CommandSpec {
        program: sevenz.clone(),
        args: vec![
            s("x"),
            input_path.to_string_lossy().into_owned(),
            format!("-o{}", tmp.to_string_lossy()),
            s("-y"),
        ],
        tool_name: "7-Zip".into(),
    };
    progress(-1.0);
    let result = run_streaming(&extract, None, cancel, progress).and_then(|_| {
        // What was inside becomes the input set of the new archive.
        let mut entries: Vec<String> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&tmp) {
            for e in rd.flatten() {
                entries.push(e.path().to_string_lossy().into_owned());
            }
        }
        if entries.is_empty() {
            return Err(EngineError::new("io", "The archive appears to be empty."));
        }
        // Create from inside the temp dir so paths are stored relative.
        let output = output_path(input_path, target, settings);
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| EngineError::new("io", format!("Cannot create output folder: {e}")))?;
        }
        let (t, ext_note) = match target {
            "zip" => ("zip", None),
            "tar" => ("tar", None),
            "gz" => (
                "tar",
                Some(
                    "GZIP archives hold one stream, so the contents were packed as TAR.GZ."
                        .to_string(),
                ),
            ),
            other => {
                return Err(EngineError::unsupported(format!(
                    "Cannot repack to {other}."
                )))
            }
        };
        let final_output = if target == "gz" {
            output_path(input_path, "tar.gz", settings)
        } else {
            output.clone()
        };
        // Store paths relative to the temp dir: pass bare names and run
        // 7z with its working directory set to the temp folder.
        let rel_args: Vec<String> = entries
            .iter()
            .map(|e| {
                PathBuf::from(e)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| e.clone())
            })
            .collect();
        let mut args2: Vec<String> = vec![
            s("a"),
            format!("-t{t}"),
            final_output.to_string_lossy().into_owned(),
        ];
        args2.extend(rel_args);
        let spec2 = CommandSpec {
            program: sevenz.clone(),
            args: args2,
            tool_name: "7-Zip".into(),
        };
        run_streaming_in(&spec2, Some(&tmp), None, cancel, progress)?;
        Ok(JobOutcome {
            output: final_output,
            note: ext_note,
        })
    });
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

fn create_archive(
    tools: &Tools,
    inputs: &[String],
    format: &str,
    output_dir: &Option<String>,
    suffix: &str,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(f32),
) -> EngineResult<JobOutcome> {
    let sevenz = tools
        .sevenz
        .clone()
        .ok_or_else(|| EngineError::tool_missing("7-Zip"))?;
    if inputs.is_empty() {
        return Err(EngineError::unsupported("No files were given to archive."));
    }
    let first = PathBuf::from(&inputs[0]);
    let any_dir = inputs.iter().any(|i| PathBuf::from(i).is_dir());
    // GZIP holds exactly one stream: a single file goes to .gz; anything
    // else (folders, multiple files) becomes .tar.gz, and we say so.
    let (sevenz_type, out_ext, note) = match format {
        "zip" => ("zip", "zip".to_string(), None),
        "tar" => ("tar", "tar".to_string(), None),
        "gz" => {
            if inputs.len() == 1 && !any_dir {
                ("gzip", "gz".to_string(), None)
            } else {
                (
                    "tar",
                    "tar.gz".to_string(),
                    Some(
                        "GZIP archives hold one file, so the selection was packed as TAR.GZ."
                            .to_string(),
                    ),
                )
            }
        }
        other => {
            return Err(EngineError::unsupported(format!(
                "Unknown archive format {other}."
            )))
        }
    };
    let settings = OutputSettings {
        output_dir: output_dir.clone(),
        suffix: suffix.to_string(),
    };
    let output = output_path(&first, &out_ext, &settings);
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| EngineError::new("io", format!("Cannot create output folder: {e}")))?;
    }
    // Store clean paths inside the archive: when every input shares one
    // parent folder, run 7z there with bare names; otherwise fall back to
    // the given paths (7z strips the filesystem root).
    let parents: Vec<PathBuf> = inputs
        .iter()
        .map(|i| {
            let p = PathBuf::from(i);
            if p.is_dir() {
                p.parent().map(|x| x.to_path_buf()).unwrap_or(p.clone())
            } else {
                p.parent()
                    .map(|x| x.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."))
            }
        })
        .collect();
    let shared_parent = if parents.iter().all(|p| Some(p) == parents.first()) {
        parents.first().cloned()
    } else {
        None
    };
    let mut args: Vec<String> = vec![
        s("a"),
        format!("-t{sevenz_type}"),
        output.to_string_lossy().into_owned(),
    ];
    match &shared_parent {
        Some(_) => {
            for i in inputs {
                let p = PathBuf::from(i);
                args.push(
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| i.clone()),
                );
            }
        }
        None => {
            for i in inputs {
                args.push(i.clone());
            }
        }
    }
    let spec = CommandSpec {
        program: sevenz,
        args,
        tool_name: "7-Zip".into(),
    };
    progress(-1.0);
    run_streaming_in(&spec, shared_parent.as_deref(), None, cancel, progress)?;
    Ok(JobOutcome { output, note })
}

/// Image compression: find the highest quality whose output fits the target
/// size (target-size mode), where percentage mode means "the output should
/// be at most (100 − value)% of the original size".
fn compress_image(
    tools: &Tools,
    input: &str,
    mode: &str,
    value: f64,
    output_dir: &Option<String>,
    suffix: &str,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(f32),
) -> EngineResult<JobOutcome> {
    let (magick, _) = tools
        .magick_argv()
        .ok_or_else(|| EngineError::tool_missing("ImageMagick"))?;
    let input_path = PathBuf::from(input);
    let original = std::fs::metadata(&input_path).map(|m| m.len()).unwrap_or(0) as f64;
    if original <= 0.0 {
        return Err(EngineError::new(
            "io",
            "The source image could not be read.",
        ));
    }
    let target_bytes = match mode {
        "percent" => original * (1.0 - value.clamp(1.0, 95.0) / 100.0),
        "target" => value.max(1024.0),
        other => {
            return Err(EngineError::unsupported(format!(
                "Unknown compress mode {other}."
            )))
        }
    };
    let ext = extension_of(&input_path);
    let out_ext = if ext == "png" {
        "jpg".to_string()
    } else {
        ext.clone()
    };
    // PNG has no quality knob that reliably shrinks photos; re-encode lossy
    // formats to themselves and PNG to JPG, saying so in the result note.
    let note = if ext == "png" {
        Some(
            "PNG is lossless, so the compressed copy was saved as JPG to reach the target size."
                .to_string(),
        )
    } else {
        None
    };
    let settings = OutputSettings {
        output_dir: output_dir.clone(),
        suffix: suffix.to_string(),
    };
    let output = output_path(&input_path, &out_ext, &settings);
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| EngineError::new("io", format!("Cannot create output folder: {e}")))?;
    }
    let temp = output.with_extension("tmp.jpg");
    let qualities = [92u8, 82, 72, 62, 52, 42, 32, 22, 12];
    let total = qualities.len() as f32;
    for (i, q) in qualities.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(EngineError::cancelled());
        }
        let mut args: Vec<String> = Vec::new();
        if !tools.magick_is_convert {
            // standalone magick: no subcommand prefix
        }
        args.push(s(input));
        args.extend(["-quality".into(), q.to_string()]);
        args.push(temp.to_string_lossy().into_owned());
        let spec = CommandSpec {
            program: magick.clone(),
            args,
            tool_name: "ImageMagick".into(),
        };
        run_streaming(&spec, None, cancel, &mut |_| {})?;
        let size = std::fs::metadata(&temp)
            .map(|m| m.len())
            .unwrap_or(u64::MAX) as f64;
        progress(((i as f32 + 1.0) / total) * 100.0);
        if size <= target_bytes || *q == 12 {
            std::fs::rename(&temp, &output).map_err(|e| {
                EngineError::new("io", format!("Cannot save compressed image: {e}"))
            })?;
            return Ok(JobOutcome { output, note });
        }
    }
    unreachable!()
}

// ---------------------------------------------------------------------------
// Registry used by tests and diagnostics
// ---------------------------------------------------------------------------

pub fn tool_report(tools: &Tools) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert(
        "ffmpeg".into(),
        tools
            .ffmpeg
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "MISSING".into()),
    );
    m.insert(
        "ffprobe".into(),
        tools
            .ffprobe
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "MISSING".into()),
    );
    m.insert(
        "magick".into(),
        tools
            .magick
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "MISSING".into()),
    );
    m.insert(
        "sevenz".into(),
        tools
            .sevenz
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "MISSING".into()),
    );
    m.insert(
        "soffice".into(),
        tools
            .soffice
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "MISSING".into()),
    );
    m.insert(
        "pandoc".into(),
        tools
            .pandoc
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "MISSING".into()),
    );
    m.insert(
        "pdftoppm".into(),
        tools
            .pdftoppm
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "MISSING".into()),
    );
    m.insert(
        "pdftotext".into(),
        tools
            .pdftotext
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "MISSING".into()),
    );
    m.insert(
        "pdfimages".into(),
        tools
            .pdfimages
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "MISSING".into()),
    );
    m.insert(
        "pdfinfo".into(),
        tools
            .pdfinfo
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "MISSING".into()),
    );
    m.insert("magick_writable".into(), tools.magick_writable.join(","));
    m.insert("ffmpeg_muxers".into(), tools.ffmpeg_muxers.join(","));
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_classify() {
        assert_eq!(family_of(Path::new("a/b/photo.JPG"), false), Family::Image);
        assert_eq!(family_of(Path::new("song.wma"), false), Family::Audio);
        assert_eq!(family_of(Path::new("clip.gif"), false), Family::Video);
        assert_eq!(family_of(Path::new("pack.tar.gz"), false), Family::Archive);
        assert_eq!(family_of(Path::new("data.bin"), false), Family::Unknown);
        // Stage 2: .txt is a Document now (was Unknown in Stage 1).
        assert_eq!(family_of(Path::new("notes.txt"), false), Family::Document);
        assert_eq!(family_of(Path::new("folder"), true), Family::Archive);
    }

    #[test]
    fn targets_exclude_identity() {
        let t = targets_for(Path::new("photo.png"), false);
        assert!(t.contains(&"jpg".to_string()));
        assert!(!t.contains(&"png".to_string()));
        let t = targets_for(Path::new("photo.jpeg"), false);
        assert!(
            !t.contains(&"jpg".to_string()),
            "jpeg normalizes to jpg identity"
        );
        let t = targets_for(Path::new("movie.mp4"), false);
        assert!(t.contains(&"mp3".to_string()), "video offers MP3 export");
        let t = targets_for(Path::new("mystery.bin"), false);
        assert_eq!(t, vec!["zip", "tar", "gz"], "unknown files can be archived");
    }

    #[test]
    fn output_naming_never_overwrites() {
        // Unique dir per run: the test writes real files, and a fixed dir
        // makes the second run see the first run's outputs as collisions.
        let dir = std::env::temp_dir().join(format!("uruk-engine-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("photo.png");
        std::fs::write(&input, b"x").unwrap();
        let settings = OutputSettings::default();
        let out1 = output_path(&input, "jpg", &settings);
        assert_eq!(
            out1.file_name().unwrap().to_string_lossy(),
            "photo.converted.jpg"
        );
        std::fs::write(&out1, b"x").unwrap();
        let out2 = output_path(&input, "jpg", &settings);
        assert_eq!(
            out2.file_name().unwrap().to_string_lossy(),
            "photo.converted (2).jpg"
        );
    }

    #[test]
    fn tar_gz_extension() {
        assert_eq!(extension_of(Path::new("a/b/backup.TAR.GZ")), "tar.gz");
        assert_eq!(extension_of(Path::new("x.png")), "png");
    }

    #[test]
    fn ffmpeg_time_parse() {
        assert_eq!(
            parse_ffmpeg_time("frame= 1 time=00:00:02.50 bitrate"),
            Some(2.5)
        );
        assert_eq!(parse_ffmpeg_time("no time here"), None);
    }

    #[test]
    fn video_args_for_webm() {
        let args = ffmpeg_video_args("in.mp4", "out.webm", "webm").unwrap();
        assert!(args.iter().any(|a| a == "libvpx-vp9"));
        assert_eq!(args.last().unwrap(), "out.webm");
    }

    #[test]
    fn stage2_families_classify() {
        assert_eq!(family_of(Path::new("doc.pdf"), false), Family::Pdf);
        assert_eq!(family_of(Path::new("doc.docx"), false), Family::Document);
        assert_eq!(family_of(Path::new("doc.odt"), false), Family::Document);
        assert_eq!(family_of(Path::new("doc.rtf"), false), Family::Document);
        assert_eq!(family_of(Path::new("book.epub"), false), Family::Document);
        assert_eq!(family_of(Path::new("deck.pptx"), false), Family::Slides);
        assert_eq!(family_of(Path::new("deck.ppt"), false), Family::Slides);
        assert_eq!(
            family_of(Path::new("sheet.xlsx"), false),
            Family::Spreadsheet
        );
        assert_eq!(
            family_of(Path::new("sheet.csv"), false),
            Family::Spreadsheet
        );
        assert_eq!(
            family_of(Path::new("sheet.xls"), false),
            Family::Spreadsheet
        );
        assert_eq!(family_of(Path::new("subs.srt"), false), Family::Subtitle);
        assert_eq!(family_of(Path::new("subs.vtt"), false), Family::Subtitle);
    }

    #[test]
    fn stage2_targets() {
        let t = targets_for(Path::new("doc.pdf"), false);
        assert!(t.contains(&"docx".to_string()) && t.contains(&"jpg".to_string()));
        assert!(!t.contains(&"pdf".to_string()), "identity excluded");
        assert!(!t.contains(&"pages".to_string()), "Pages is never offered");
        let t = targets_for(Path::new("sheet.xls"), false);
        assert!(t.contains(&"xlsx".to_string()));
        assert!(!t.contains(&"xls".to_string()), "xls is input-only");
        let t = targets_for(Path::new("notes.markdown"), false);
        assert!(
            !t.contains(&"md".to_string()),
            "markdown normalizes to md identity"
        );
        assert!(t.contains(&"pdf".to_string()));
        let t = targets_for(Path::new("subs.srt"), false);
        assert_eq!(t, vec!["vtt".to_string(), "txt".to_string()]);
        let t = targets_for(Path::new("deck.ppt"), false);
        assert_eq!(
            t,
            vec!["pdf".to_string(), "jpg".to_string(), "png".to_string()]
        );
    }

    #[test]
    fn timestamp_parse_and_format() {
        assert_eq!(parse_timestamp("01:02:03,456"), Some(3_723_456));
        assert_eq!(parse_timestamp("01:02:03.456"), Some(3_723_456));
        assert_eq!(
            parse_timestamp("02:03.5"),
            Some(123_500),
            "MM:SS.mmm, .5 = 500ms"
        );
        assert_eq!(parse_timestamp("00:00:01,05"), Some(1_050), ",05 = 50ms");
        assert_eq!(
            parse_timestamp("100:00:00,000"),
            Some(360_000_000),
            "hours overflow"
        );
        assert_eq!(parse_timestamp("garbage"), None);
        assert_eq!(format_timestamp(3_723_456, ','), "01:02:03,456");
        assert_eq!(format_timestamp(360_000_000, '.'), "100:00:00.000");
        assert_eq!(format_timestamp(999, '.'), "00:00:00.999");
    }

    #[test]
    fn srt_to_vtt_roundtrip() {
        let srt = "1\n00:00:01,000 --> 00:00:02,500\nHello world\n\n2\n01:00:00,000 --> 01:00:01,000\nSecond line one\nSecond line two\n";
        let vtt = subtitle_convert(srt, "vtt").unwrap();
        assert!(vtt.starts_with("WEBVTT\n"));
        assert!(vtt.contains("00:00:01.000 --> 00:00:02.500"));
        assert!(vtt.contains("Second line one\nSecond line two"));
        // …and back: comma separators and indices return.
        let back = subtitle_convert(&vtt, "srt").unwrap();
        assert!(back.contains("1\n00:00:01,000 --> 00:00:02,500"));
        assert!(back.contains("2\n01:00:00,000 --> 01:00:01,000"));
    }

    #[test]
    fn vtt_features_are_handled() {
        // Cue identifier, cue settings, a NOTE block, MM:SS.mmm times,
        // and a 100-hour timestamp.
        let vtt = "WEBVTT\n\nNOTE\nThis is a comment block\n\ncue-7\n02:03.500 --> 02:04.000 position:50% align:center\nShort form time\n\n100:00:00.000 --> 100:00:01.500\nFar future\n";
        let cues = parse_cues(vtt);
        assert_eq!(cues.len(), 2, "NOTE block skipped, both cues parsed");
        assert_eq!(cues[0].start_ms, 123_500);
        assert_eq!(cues[0].end_ms, 124_000);
        assert_eq!(cues[1].start_ms, 360_000_000);
        let srt = subtitle_convert(vtt, "srt").unwrap();
        assert!(srt.contains("100:00:00,000 --> 100:00:01,500"));
    }

    #[test]
    fn subtitle_txt_is_dialogue_only() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nAlpha\nBeta\n\n2\n00:00:03,000 --> 00:00:04,000\nGamma\n";
        let txt = subtitle_convert(srt, "txt").unwrap();
        assert_eq!(txt, "Alpha\nBeta\nGamma\n");
    }

    #[test]
    fn subtitle_empty_is_an_honest_error() {
        assert!(subtitle_convert("no cues here", "vtt").is_err());
    }
}
