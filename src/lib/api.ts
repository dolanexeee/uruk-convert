import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

export type Family = "image" | "audio" | "video" | "archive" | "unknown";

export interface FileInfo {
  path: string;
  name: string;
  ext: string;
  family: Family;
  is_dir: boolean;
  size: number;
  targets: string[];
  duration?: number | null;
  dimensions?: string | null;
}

export type JobSpec =
  | { kind: "convert"; input: string; target: string; output_dir?: string | null; suffix: string; image_quality: number }
  | { kind: "compress_image"; input: string; mode: string; value: number; output_dir?: string | null; suffix: string }
  | { kind: "extract_archive"; input: string; output_dir?: string | null }
  | { kind: "create_archive"; inputs: string[]; format: string; output_dir?: string | null; suffix: string };

export interface JobEvent {
  type: "progress" | "done" | "error";
  id: string;
  percent?: number;
  output?: string;
  note?: string | null;
  code?: string;
  message?: string;
}

type Handler = (e: JobEvent) => void;

export const DEMO = typeof window !== "undefined" && window.location.hash.includes("demo");

const handlers = new Set<Handler>();
let tauriListening = false;

export function onJobEvent(h: Handler): () => void {
  handlers.add(h);
  if (!DEMO && !tauriListening) {
    tauriListening = true;
    listen<{ id: string; percent: number }>("job-progress", (e) =>
      emit({ type: "progress", id: e.payload.id, percent: e.payload.percent }),
    ).catch(() => undefined);
    listen<{ id: string; output: string; note?: string | null }>("job-done", (e) =>
      emit({ type: "done", id: e.payload.id, output: e.payload.output, note: e.payload.note }),
    ).catch(() => undefined);
    listen<{ id: string; code: string; message: string }>("job-error", (e) =>
      emit({ type: "error", id: e.payload.id, code: e.payload.code, message: e.payload.message }),
    ).catch(() => undefined);
  }
  return () => handlers.delete(h);
}

function emit(e: JobEvent) {
  handlers.forEach((h) => h(e));
}

// ---------------------------------------------------------------------------
// Demo mode (QA screenshots / design review in a plain browser): no Tauri
// backend; analysis and jobs are simulated locally.
// ---------------------------------------------------------------------------

const DEMO_TARGETS: Record<string, string[]> = {
  image: ["jpg", "webp", "heic", "tiff", "avif", "bmp", "pdf"],
  audio: ["mp3", "wav", "flac", "ogg", "opus", "aiff", "wma"],
  video: ["mp4", "mov", "mkv", "webm", "avi", "wmv", "gif", "mp3"],
  archive: ["zip", "tar", "gz"],
  unknown: ["zip", "tar", "gz"],
};

function demoAnalyzeOne(path: string): FileInfo {
  const name = path.split("/").pop() ?? path;
  const ext = (name.split(".").pop() ?? "").toLowerCase();
  let family: Family = "unknown";
  if (["jpg", "jpeg", "png", "webp", "heic", "tiff", "avif", "bmp", "svg"].includes(ext)) family = "image";
  else if (["mp3", "m4a", "wav", "flac", "ogg", "opus", "aiff", "wma"].includes(ext)) family = "audio";
  else if (["mp4", "mov", "mkv", "webm", "avi", "wmv", "gif"].includes(ext)) family = "video";
  else if (["zip", "tar", "gz", "rar"].includes(ext)) family = "archive";
  const targets = (DEMO_TARGETS[family] ?? []).filter((t) => t !== ext);
  return {
    path,
    name,
    ext,
    family,
    is_dir: false,
    size: 2_418_553,
    targets,
    duration: family === "video" || family === "audio" ? 95 : null,
    dimensions: family === "image" ? "2400x1600" : family === "video" ? "1920x1080" : null,
  };
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

export async function analyzePaths(paths: string[]): Promise<FileInfo[]> {
  if (DEMO) return paths.map(demoAnalyzeOne);
  return invoke<FileInfo[]>("analyze_paths", { paths });
}

export async function toolsReport(): Promise<Record<string, string>> {
  if (DEMO) {
    return { ffmpeg: "demo", ffprobe: "demo", magick: "demo", sevenz: "demo" };
  }
  return invoke<Record<string, string>>("tools_report");
}

export function startJob(id: string, job: JobSpec): void {
  if (DEMO) {
    let p = 0;
    const timer = setInterval(() => {
      p += 8 + Math.random() * 10;
      if (p >= 100) {
        clearInterval(timer);
        const target =
          job.kind === "convert" ? job.target : job.kind === "create_archive" ? job.format : job.kind === "extract_archive" ? "extracted" : "jpg";
        emit({ type: "done", id, output: `/demo/output.converted.${target}` });
      } else {
        emit({ type: "progress", id, percent: p });
      }
    }, 160);
    demoTimers.set(id, timer);
    return;
  }
  void invoke("start_job", { id, job });
}

const demoTimers = new Map<string, ReturnType<typeof setInterval>>();

export function cancelJob(id: string): void {
  if (DEMO) {
    const t = demoTimers.get(id);
    if (t) clearInterval(t);
    emit({ type: "error", id, code: "cancelled", message: "Conversion cancelled." });
    return;
  }
  void invoke("cancel_job", { id });
}

export async function pickFiles(): Promise<string[]> {
  if (DEMO) return ["/demo/holiday-photo.png"];
  const picked = await open({ multiple: true, directory: false });
  if (!picked) return [];
  return Array.isArray(picked) ? picked : [picked];
}

export async function pickFolder(): Promise<string | null> {
  if (DEMO) return "/demo/Some Folder";
  const picked = await open({ multiple: false, directory: true });
  return typeof picked === "string" ? picked : null;
}

export async function pickOutputDir(): Promise<string | null> {
  return pickFolder();
}

export function revealInFolder(path: string): void {
  if (DEMO) return;
  void invoke("reveal_in_folder", { path });
}

export function openPath(path: string): void {
  if (DEMO) return;
  void invoke("open_path", { path });
}
