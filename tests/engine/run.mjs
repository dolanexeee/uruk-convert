#!/usr/bin/env node
/**
 * Uruk Convert — engine integration tests.
 *
 * Drives the REAL engine (uruk-engine-core via engine-cli) through every
 * Stage-1 conversion with programmatically generated fixtures, then
 * re-probes every output (ffprobe / ImageMagick identify / 7z list) to
 * prove the output format is genuine — not just a renamed file.
 *
 * Usage: npm run test:engine   (or: node tests/engine/run.mjs)
 * Env:   SKIP_BUILD=1 to reuse a previously built engine-cli.
 */
import { execFileSync, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { homedir } from "node:os";

const CARGO = existsSync(join(homedir(), ".cargo", "bin", "cargo"))
  ? join(homedir(), ".cargo", "bin", "cargo")
  : "cargo";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const WORK = join(ROOT, "tests", "engine", ".work");
const FIX = join(WORK, "fixtures");
const OUT = join(WORK, "out");
const SIDECARS = join(ROOT, "src-tauri", "binaries");
const TRIPLE = "x86_64-unknown-linux-gnu";
const FFMPEG = join(SIDECARS, `uruk-ffmpeg-${TRIPLE}`);
const FFPROBE = join(SIDECARS, `uruk-ffprobe-${TRIPLE}`);
const SEVENZ = join(SIDECARS, `uruk-7z-${TRIPLE}`);
const ENGINE = join(ROOT, "src-tauri", "target", "debug", "engine-cli");

const env = {
  ...process.env,
  URUK_SIDECAR_DIR: SIDECARS,
  PATH: `${SIDECARS}:${process.env.PATH}`,
};

let passed = 0;
let failed = 0;
let gaps = 0;
const failures = [];

function run(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, { encoding: "utf8", env, timeout: 180_000, ...opts });
  return r;
}
function must(cmd, args, opts = {}) {
  const r = run(cmd, args, opts);
  if (r.status !== 0) throw new Error(`${cmd} ${args.join(" ")} failed: ${r.stderr?.slice(-400)}`);
  return r.stdout ?? "";
}

function check(name, ok, detail = "") {
  if (ok) {
    passed++;
    console.log(`  PASS  ${name}`);
  } else {
    failed++;
    failures.push(`${name}: ${detail}`);
    console.log(`  FAIL  ${name} — ${detail}`);
  }
}
function gap(name, why) {
  gaps++;
  console.log(`  GAP   ${name} — ${why}`);
}

// ---------------------------------------------------------------------------
// Setup: build engine CLI + fixtures
// ---------------------------------------------------------------------------
if (!process.env.SKIP_BUILD) {
  console.log("· building engine-cli…");
  must(CARGO, ["build", "-p", "uruk-engine-core", "--bin", "engine-cli"], {
    cwd: join(ROOT, "src-tauri"),
  });
}
rmSync(WORK, { recursive: true, force: true });
mkdirSync(FIX, { recursive: true });
mkdirSync(OUT, { recursive: true });

console.log("· generating fixtures…");
// Images (system ImageMagick is a fixture tool here, not the engine).
must("convert", ["-size", "320x240", "gradient:#123058-#c9a227", join(FIX, "img.png")]);
must("convert", ["-size", "800x600", "plasma:fractal", "-quality", "95", join(FIX, "noise.jpg")]);
writeFileSync(
  join(FIX, "shape.svg"),
  `<svg xmlns="http://www.w3.org/2000/svg" width="200" height="120"><rect width="200" height="120" fill="#0f5e5c"/><circle cx="100" cy="60" r="34" fill="#c9a227"/></svg>`,
);
// Video with audio (lavfi via the bundled static ffmpeg).
must(FFMPEG, [
  "-y", "-v", "error",
  "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=15:duration=1.5",
  "-f", "lavfi", "-i", "sine=frequency=660:duration=1.5",
  "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest",
  join(FIX, "clip.mp4"),
]);
// Audio.
must(FFMPEG, [
  "-y", "-v", "error", "-f", "lavfi", "-i", "sine=frequency=440:duration=1",
  "-c:a", "pcm_s16le", join(FIX, "tone.wav"),
]);
// Folder fixture.
mkdirSync(join(FIX, "folder"), { recursive: true });
writeFileSync(join(FIX, "folder", "a.txt"), "hello uruk\n");
writeFileSync(join(FIX, "folder", "b.txt"), "second file\n");

// Tool capabilities (decides honest-gap expectations).
const tools = JSON.parse(must(ENGINE, ["tools"]));
const magickWritable = (tools.magick_writable ?? "").split(",").map((s) => s.trim());
const heicEncodable =
  magickWritable.includes("HEIC") || (tools.ffmpeg_muxers ?? "").split(",").includes("heif");
console.log(`· heic encodable on this system: ${heicEncodable}`);

// ---------------------------------------------------------------------------
// Helpers to run one engine job
// ---------------------------------------------------------------------------
let jobSeq = 0;
function engineJob(spec) {
  const jobFile = join(WORK, `job-${jobSeq++}.json`);
  writeFileSync(jobFile, JSON.stringify(spec));
  const r = run(ENGINE, ["run", jobFile]);
  const lines = (r.stdout ?? "").trim().split("\n");
  const done = lines.find((l) => l.startsWith("DONE "));
  const err = lines.find((l) => l.startsWith("ERROR "));
  const note = lines.find((l) => l.startsWith("NOTE "));
  return {
    ok: Boolean(done),
    output: done ? done.slice(5).trim() : null,
    error: err ? err.slice(6) : null,
    note: note ? note.slice(5) : null,
    raw: r.stdout,
  };
}

function probeFormat(path) {
  const r = run(FFPROBE, [
    "-v", "error", "-show_entries", "stream=codec_type,codec_name",
    "-show_entries", "format=format_name", "-of", "json", path,
  ]);
  try {
    return JSON.parse(r.stdout);
  } catch {
    return null;
  }
}
function identifyFormat(path) {
  // IM6 ships `identify` as its own binary; `magick identify` for IM7.
  let r = run("identify", ["-format", "%m", path]);
  if (r.status !== 0) r = run("magick", ["identify", "-format", "%m", path]);
  return (r.stdout ?? "").trim();
}
function sevenList(path) {
  const r = run(SEVENZ, ["l", "-slt", path]);
  return r.stdout ?? "";
}

const conv = (input, target) => ({ kind: "convert", input, target, suffix: "converted", image_quality: 92 });

// ---------------------------------------------------------------------------
console.log("· images");
{
  const png = join(FIX, "img.png");
  const jpgId = identifyFormat(join(FIX, "noise.jpg"));
  check("fixture jpg is JPEG", jpgId === "JPEG", jpgId);

  const cases = [
    ["jpg", (p) => identifyFormat(p) === "JPEG"],
    ["webp", (p) => identifyFormat(p) === "WEBP"],
    ["tiff", (p) => identifyFormat(p) === "TIFF"],
    ["bmp", (p) => identifyFormat(p) === "BMP"],
    ["avif", (p) => probeFormat(p)?.streams?.some((s) => s.codec_name === "av1")],
    ["pdf", (p) => readFileSync(p).subarray(0, 4).toString() === "%PDF"],
  ];
  for (const [target, verify] of cases) {
    const job = engineJob(conv(png, target));
    check(`png → ${target}`, job.ok && existsSync(job.output ?? "") && verify(job.output), job.error ?? job.raw ?? "");
  }
  // HEIC target: either genuinely works or the engine must refuse honestly.
  const heic = engineJob(conv(png, "heic"));
  if (heicEncodable) {
    check("png → heic (encodable here)", heic.ok && existsSync(heic.output ?? ""), heic.error ?? "");
  } else if (!heic.ok && (heic.error ?? "").startsWith("heic_encode_unavailable")) {
    gap("png → heic", "no HEIC encoder in this system's tools; engine refuses with heic_encode_unavailable (documented in PARITY.md)");
  } else {
    check("png → heic honest handling", false, heic.error ?? heic.raw ?? "produced output without an encoder?");
  }
  // jpg → png and svg → png
  const j2p = engineJob(conv(join(FIX, "noise.jpg"), "png"));
  check("jpg → png", j2p.ok && identifyFormat(j2p.output) === "PNG", j2p.error ?? "");
  const s2p = engineJob(conv(join(FIX, "shape.svg"), "png"));
  check("svg → png", s2p.ok && identifyFormat(s2p.output) === "PNG", s2p.error ?? "");

  // Compress: percentage + target modes must actually shrink the file.
  const srcSize = statSync(join(FIX, "noise.jpg")).size;
  const cPct = engineJob({ kind: "compress_image", input: join(FIX, "noise.jpg"), mode: "percent", value: 70, suffix: "converted" });
  check(
    "compress percent 70 shrinks",
    cPct.ok && statSync(cPct.output).size < srcSize * 0.6,
    cPct.error ?? `out=${cPct.output ? statSync(cPct.output).size : "?"} src=${srcSize}`,
  );
  const cTgt = engineJob({ kind: "compress_image", input: join(FIX, "noise.jpg"), mode: "target", value: 60_000, suffix: "converted" });
  check(
    "compress target 60KB fits",
    cTgt.ok && statSync(cTgt.output).size <= 60_000,
    cTgt.error ?? `out=${cTgt.output ? statSync(cTgt.output).size : "?"}`,
  );
}

console.log("· video");
{
  const mp4 = join(FIX, "clip.mp4");
  const expect = [
    ["mov", (f) => (f?.format?.format_name ?? "").includes("mov")],
    ["mkv", (f) => (f?.format?.format_name ?? "").includes("matroska")],
    ["webm", (f) => (f?.format?.format_name ?? "").includes("webm") && f.streams.some((s) => s.codec_name === "vp9")],
    ["avi", (f) => (f?.format?.format_name ?? "").includes("avi")],
    ["wmv", (f) => (f?.format?.format_name ?? "").includes("asf") && f.streams.some((s) => s.codec_name === "wmv2")],
    ["gif", (f) => (f?.format?.format_name ?? "").includes("gif")],
    ["mp3", (f) => f.streams.length > 0 && f.streams.every((s) => s.codec_type === "audio") && f.streams.some((s) => s.codec_name === "mp3")],
  ];
  for (const [target, pred] of expect) {
    const job = engineJob(conv(mp4, target));
    const probe = job.ok ? probeFormat(job.output) : null;
    check(`mp4 → ${target}`, Boolean(job.ok && probe && pred(probe)), job.error ?? JSON.stringify(probe));
  }
}

console.log("· audio");
{
  const wav = join(FIX, "tone.wav");
  const expect = [
    ["mp3", "mp3"],
    ["m4a", "aac"],
    ["flac", "flac"],
    ["ogg", "vorbis"],
    ["opus", "opus"],
    ["aiff", "pcm_s16be"],
    ["wma", "wmav2"],
  ];
  for (const [target, codec] of expect) {
    const job = engineJob(conv(wav, target));
    const probe = job.ok ? probeFormat(job.output) : null;
    check(
      `wav → ${target}`,
      Boolean(probe?.streams?.some((s) => s.codec_name === codec)),
      job.error ?? JSON.stringify(probe),
    );
  }
  const back = engineJob({ ...conv(wav, "mp3") });
  if (back.ok) {
    const w = engineJob(conv(back.output, "wav"));
    const probe = w.ok ? probeFormat(w.output) : null;
    check("mp3 → wav", Boolean(probe?.streams?.some((s) => s.codec_name === "pcm_s16le")), w.error ?? "");
  }
}

console.log("· archives");
{
  const file = join(FIX, "noise.jpg");
  const zip = engineJob({ kind: "create_archive", inputs: [file], format: "zip", suffix: "converted" });
  check("create zip from file", zip.ok && sevenList(zip.output).includes("noise.jpg"), zip.error ?? "");
  const tar = engineJob({ kind: "create_archive", inputs: [join(FIX, "folder")], format: "tar", suffix: "converted" });
  check("create tar from folder", tar.ok && sevenList(tar.output).includes("a.txt"), tar.error ?? "");
  const gz = engineJob({ kind: "create_archive", inputs: [file], format: "gz", suffix: "converted" });
  check("create gz from single file", gz.ok && (gz.output ?? "").endsWith(".gz") && !(gz.output ?? "").endsWith(".tar.gz"), gz.error ?? gz.output ?? "");
  const gzf = engineJob({ kind: "create_archive", inputs: [join(FIX, "folder")], format: "gz", suffix: "converted" });
  check(
    "folder → gz becomes tar.gz with note",
    gzf.ok && (gzf.output ?? "").endsWith(".tar.gz") && Boolean(gzf.note),
    gzf.error ?? `${gzf.output} note=${gzf.note}`,
  );
  if (zip.ok) {
    const ex = engineJob({ kind: "extract_archive", input: zip.output });
    check(
      "extract zip",
      ex.ok && existsSync(join(ex.output, "noise.jpg")),
      ex.error ?? ex.output ?? "",
    );
    const repack = engineJob(conv(zip.output, "tar"));
    const listing = repack.ok ? sevenList(repack.output) : "";
    check(
      "repack zip → tar (contents, not nested)",
      repack.ok && listing.includes("noise.jpg") && !listing.includes(".zip"),
      repack.error ?? listing.slice(0, 200),
    );
  }
}

console.log(`\n══ engine tests: ${passed} passed, ${failed} failed, ${gaps} known gaps ══`);
if (failures.length) {
  console.log("failures:\n - " + failures.join("\n - "));
  process.exit(1);
}
