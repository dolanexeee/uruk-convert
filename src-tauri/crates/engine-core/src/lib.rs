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
        EngineError { code: code.into(), message: message.into() }
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
        let tail = if tail.len() > 400 { &tail[tail.len() - 400..] } else { tail };
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
    Unknown,
}

pub const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "heic", "heif", "tiff", "tif", "avif", "bmp", "svg",
];
pub const AUDIO_EXTS: &[&str] = &["mp3", "m4a", "wav", "flac", "ogg", "opus", "aif", "aiff", "wma"];
pub const VIDEO_EXTS: &[&str] = &["mp4", "mov", "mkv", "webm", "avi", "wmv", "gif"];
pub const ARCHIVE_EXTS: &[&str] = &["zip", "tar", "gz", "tgz", "tar.gz", "rar"];

/// Target formats offered for an image input (Stage 1). `svg` is input-only.
pub const IMAGE_TARGETS: &[&str] = &["jpg", "png", "webp", "heic", "tiff", "avif", "bmp", "pdf"];
/// Target formats offered for an audio input (Stage 1).
pub const AUDIO_TARGETS: &[&str] = &["mp3", "m4a", "wav", "flac", "ogg", "opus", "aiff", "wma"];
/// Target formats offered for a video input (Stage 1), incl. audio export.
pub const VIDEO_TARGETS: &[&str] = &["mp4", "mov", "mkv", "webm", "avi", "wmv", "gif", "mp3"];
/// Archive re-pack targets (Stage 1: create ZIP / TAR / GZIP).
pub const ARCHIVE_TARGETS: &[&str] = &["zip", "tar", "gz"];

/// Lowercased extension of a path, treating `.tar.gz` as one extension.
pub fn extension_of(path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
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
            other => other.into(),
        }
    };
    let mine = norm(&ext);
    let pool: &[&str] = match family {
        Family::Image => IMAGE_TARGETS,
        Family::Audio => AUDIO_TARGETS,
        Family::Video => VIDEO_TARGETS,
        Family::Archive => ARCHIVE_TARGETS,
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
            find_in_dirs(&dirs, &sidecar_candidates(base))
                .or_else(|| find_on_path(path_names))
        };

        let ffmpeg = find("uruk-ffmpeg", &["ffmpeg", "ffmpeg.exe"]);
        let ffprobe = find("uruk-ffprobe", &["ffprobe", "ffprobe.exe"]);
        let sevenz = find("uruk-7z", &["7z", "7zz", "7za", "7z.exe"]);

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
                            self.magick_writable.push(fmt.trim_end_matches('*').to_uppercase());
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
                    "-v", "error",
                    "-show_entries", "stream=codec_type,width,height",
                    "-show_entries", "format=duration,format_name",
                    "-of", "json",
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
                            d.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| d.as_f64())
                        });
                        pr.format_name =
                            fmt.get("format_name").and_then(|f| f.as_str()).map(|s| s.to_string());
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
    let pr = if is_dir { Probe::default() } else { probe(tools, path) };
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
        OutputSettings { output_dir: None, suffix: "converted".into() }
    }
}

pub fn output_path(input: &Path, target_ext: &str, settings: &OutputSettings) -> PathBuf {
    let dir = settings
        .output_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            input.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."))
        });
    let stem = {
        let name = input.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
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
            a.extend(["-c:v", "libx264", "-preset", "medium", "-crf", "20", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "160k"].map(s));
            if target == "mp4" {
                a.extend(["-movflags", "+faststart"].map(s));
            }
        }
        "webm" => {
            a.extend(["-c:v", "libvpx-vp9", "-b:v", "0", "-crf", "30", "-deadline", "good", "-cpu-used", "2", "-c:a", "libopus", "-b:a", "128k"].map(s));
        }
        "avi" => {
            a.extend(["-c:v", "mpeg4", "-q:v", "4", "-c:a", "libmp3lame", "-q:a", "4"].map(s));
        }
        "wmv" => {
            a.extend(["-c:v", "wmv2", "-b:v", "1400k", "-c:a", "wmav2", "-b:a", "128k"].map(s));
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
        _ => return Err(EngineError::unsupported(format!("Cannot convert video to {target}."))),
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
        _ => return Err(EngineError::unsupported(format!("Cannot convert audio to {target}."))),
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
            let ffmpeg = tools.ffmpeg.clone().ok_or_else(|| EngineError::tool_missing("FFmpeg"))?;
            let args = ffmpeg_video_args(&input_s, &output_s, target)?;
            Ok(CommandSpec { program: ffmpeg, args, tool_name: "FFmpeg".into() })
        }
        Family::Audio => {
            let ffmpeg = tools.ffmpeg.clone().ok_or_else(|| EngineError::tool_missing("FFmpeg"))?;
            let args = ffmpeg_audio_args(&input_s, &output_s, target)?;
            Ok(CommandSpec { program: ffmpeg, args, tool_name: "FFmpeg".into() })
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
        let ffmpeg = tools.ffmpeg.clone().ok_or_else(|| EngineError::tool_missing("FFmpeg"))?;
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
        return Ok(CommandSpec { program: ffmpeg, args, tool_name: "FFmpeg".into() });
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
    Ok(CommandSpec { program: magick, args, tool_name: "ImageMagick".into() })
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
    let token: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == ':' || *c == '.').collect();
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
        .map_err(|e| EngineError::new("spawn_failed", format!("Could not start {}: {e}", spec.tool_name)))?;

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
                        let token: String =
                            rest.chars().take_while(|c| c.is_ascii_digit() || *c == ':' || *c == '.').collect();
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
                return Err(EngineError::new("spawn_failed", format!("{} failed: {e}", spec.tool_name)));
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
        JobSpec::Convert { input, target, output_dir, suffix, image_quality } => {
            let input_path = PathBuf::from(input);
            let settings = OutputSettings { output_dir: output_dir.clone(), suffix: suffix.clone() };
            let output = output_path(&input_path, target, &settings);
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| EngineError::new("io", format!("Cannot create output folder: {e}")))?;
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
                return repack_archive(
                    tools, &input_path, target, &settings, cancel, progress,
                );
            }
            let spec = build_convert_command(tools, &input_path, &output, target, *image_quality)?;
            progress(-1.0);
            run_streaming(&spec, pr.duration, cancel, progress)?;
            Ok(JobOutcome { output, note: None })
        }
        JobSpec::CompressImage { input, mode, value, output_dir, suffix } => {
            compress_image(tools, input, mode, *value, output_dir, suffix, cancel, progress)
        }
        JobSpec::ExtractArchive { input, output_dir } => {
            let sevenz = tools.sevenz.clone().ok_or_else(|| EngineError::tool_missing("7-Zip"))?;
            let input_path = PathBuf::from(input);
            let stem = {
                let name = input_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let ext = extension_of(&input_path);
                if ext.is_empty() { name.clone() } else { name[..name.len() - ext.len() - 1].to_string() }
            };
            let base_dir = output_dir
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| input_path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from(".")));
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
            Ok(JobOutcome { output: dest, note: None })
        }
        JobSpec::CreateArchive { inputs, format, output_dir, suffix } => {
            create_archive(tools, inputs, format, output_dir, suffix, cancel, progress)
        }
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
    let sevenz = tools.sevenz.clone().ok_or_else(|| EngineError::tool_missing("7-Zip"))?;
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
            "gz" => ("tar", Some("GZIP archives hold one stream, so the contents were packed as TAR.GZ.".to_string())),
            other => return Err(EngineError::unsupported(format!("Cannot repack to {other}."))),
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
        let mut args2: Vec<String> = vec![s("a"), format!("-t{t}"), final_output.to_string_lossy().into_owned()];
        args2.extend(rel_args);
        let spec2 = CommandSpec { program: sevenz.clone(), args: args2, tool_name: "7-Zip".into() };
        run_streaming_in(&spec2, Some(&tmp), None, cancel, progress)?;
        Ok(JobOutcome { output: final_output, note: ext_note })
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
    let sevenz = tools.sevenz.clone().ok_or_else(|| EngineError::tool_missing("7-Zip"))?;
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
                ("tar", "tar.gz".to_string(), Some("GZIP archives hold one file, so the selection was packed as TAR.GZ.".to_string()))
            }
        }
        other => return Err(EngineError::unsupported(format!("Unknown archive format {other}."))),
    };
    let settings = OutputSettings { output_dir: output_dir.clone(), suffix: suffix.to_string() };
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
                p.parent().map(|x| x.to_path_buf()).unwrap_or_else(|| PathBuf::from("."))
            }
        })
        .collect();
    let shared_parent = if parents.iter().all(|p| Some(p) == parents.first()) {
        parents.first().cloned()
    } else {
        None
    };
    let mut args: Vec<String> = vec![s("a"), format!("-t{sevenz_type}"), output.to_string_lossy().into_owned()];
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
    let spec = CommandSpec { program: sevenz, args, tool_name: "7-Zip".into() };
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
        return Err(EngineError::new("io", "The source image could not be read."));
    }
    let target_bytes = match mode {
        "percent" => original * (1.0 - value.clamp(1.0, 95.0) / 100.0),
        "target" => value.max(1024.0),
        other => return Err(EngineError::unsupported(format!("Unknown compress mode {other}."))),
    };
    let ext = extension_of(&input_path);
    let out_ext = if ext == "png" { "jpg".to_string() } else { ext.clone() };
    // PNG has no quality knob that reliably shrinks photos; re-encode lossy
    // formats to themselves and PNG to JPG, saying so in the result note.
    let note = if ext == "png" {
        Some("PNG is lossless, so the compressed copy was saved as JPG to reach the target size.".to_string())
    } else {
        None
    };
    let settings = OutputSettings { output_dir: output_dir.clone(), suffix: suffix.to_string() };
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
        let spec = CommandSpec { program: magick.clone(), args, tool_name: "ImageMagick".into() };
        run_streaming(&spec, None, cancel, &mut |_| {})?;
        let size = std::fs::metadata(&temp).map(|m| m.len()).unwrap_or(u64::MAX) as f64;
        progress(((i as f32 + 1.0) / total) * 100.0);
        if size <= target_bytes || *q == 12 {
            std::fs::rename(&temp, &output)
                .map_err(|e| EngineError::new("io", format!("Cannot save compressed image: {e}")))?;
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
    m.insert("ffmpeg".into(), tools.ffmpeg.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "MISSING".into()));
    m.insert("ffprobe".into(), tools.ffprobe.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "MISSING".into()));
    m.insert(
        "magick".into(),
        tools.magick.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "MISSING".into()),
    );
    m.insert("sevenz".into(), tools.sevenz.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "MISSING".into()));
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
        assert_eq!(family_of(Path::new("notes.txt"), false), Family::Unknown);
        assert_eq!(family_of(Path::new("folder"), true), Family::Archive);
    }

    #[test]
    fn targets_exclude_identity() {
        let t = targets_for(Path::new("photo.png"), false);
        assert!(t.contains(&"jpg".to_string()));
        assert!(!t.contains(&"png".to_string()));
        let t = targets_for(Path::new("photo.jpeg"), false);
        assert!(!t.contains(&"jpg".to_string()), "jpeg normalizes to jpg identity");
        let t = targets_for(Path::new("movie.mp4"), false);
        assert!(t.contains(&"mp3".to_string()), "video offers MP3 export");
        let t = targets_for(Path::new("mystery.bin"), false);
        assert_eq!(t, vec!["zip", "tar", "gz"], "unknown files can be archived");
    }

    #[test]
    fn output_naming_never_overwrites() {
        let dir = std::env::temp_dir().join("uruk-engine-test");
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("photo.png");
        std::fs::write(&input, b"x").unwrap();
        let settings = OutputSettings::default();
        let out1 = output_path(&input, "jpg", &settings);
        assert_eq!(out1.file_name().unwrap().to_string_lossy(), "photo.converted.jpg");
        std::fs::write(&out1, b"x").unwrap();
        let out2 = output_path(&input, "jpg", &settings);
        assert_eq!(out2.file_name().unwrap().to_string_lossy(), "photo.converted (2).jpg");
    }

    #[test]
    fn tar_gz_extension() {
        assert_eq!(extension_of(Path::new("a/b/backup.TAR.GZ")), "tar.gz");
        assert_eq!(extension_of(Path::new("x.png")), "png");
    }

    #[test]
    fn ffmpeg_time_parse() {
        assert_eq!(parse_ffmpeg_time("frame= 1 time=00:00:02.50 bitrate"), Some(2.5));
        assert_eq!(parse_ffmpeg_time("no time here"), None);
    }

    #[test]
    fn video_args_for_webm() {
        let args = ffmpeg_video_args("in.mp4", "out.webm", "webm").unwrap();
        assert!(args.iter().any(|a| a == "libvpx-vp9"));
        assert_eq!(args.last().unwrap(), "out.webm");
    }
}
