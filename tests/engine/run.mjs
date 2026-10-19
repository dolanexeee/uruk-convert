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
const PANDOC = join(SIDECARS, `uruk-pandoc-${TRIPLE}`);
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

// ---------------------------------------------------------------------------
// Stage 2 fixtures: documents, PDF, slides, spreadsheets, subtitles.
// Baked with the system tools (pandoc sidecar + LibreOffice + python3) —
// the engine's own conversions are then re-probed independently below.
// ---------------------------------------------------------------------------
console.log("· generating stage-2 fixtures…");
const LO_PROFILE = `file://${join(WORK, "lo-bake-profile")}`;
function loBake(convertTo, input, outDir = FIX) {
  must("soffice", [
    "--headless", "--norestore", `-env:UserInstallation=${LO_PROFILE}`,
    "--convert-to", convertTo, "--outdir", outDir, input,
  ]);
}
// A Markdown source with a probe marker, long enough to paginate to ≥2
// pages once LibreOffice renders it (feeds the multi-page PDF proofs).
{
  let md = "# URUKDOC Fixture Title\n\nThis paragraph carries the marker URUKDOC inside a real sentence for probing.\n\n- first bullet\n- second bullet\n\n";
  for (let i = 1; i <= 110; i++) {
    md += `Filler line ${i} with enough words to gradually fill pages of rendered output for pagination.\n\n`;
  }
  writeFileSync(join(FIX, "doc.md"), md);
}
must(PANDOC, [join(FIX, "doc.md"), "-o", join(FIX, "doc.docx")]);
must(PANDOC, [join(FIX, "doc.md"), "--metadata", "title=URUKDOC", "-o", join(FIX, "doc.epub")]);
loBake("odt", join(FIX, "doc.docx"));
loBake("rtf", join(FIX, "doc.docx"));
loBake("pdf", join(FIX, "doc.docx")); // → doc.pdf (the PDF-family fixture)
writeFileSync(join(FIX, "notes.txt"), "URUKTXT plain text fixture line\nsecond line of plain text\n");
writeFileSync(join(FIX, "page.html"), "<!doctype html><html><body><h1>URUKHTML heading</h1><p>URUKHTML body paragraph.</p></body></html>\n");
// Minimal ODP baked by a small python script (zip of XML parts), then
// turned into PPTX / PPT fixtures by LibreOffice.
{
  const py = [
    "import zipfile, sys",
    "content = '''<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
    "<office:document-content xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" xmlns:draw=\"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0\" xmlns:presentation=\"urn:oasis:names:tc:opendocument:xmlns:presentation:1.0\" xmlns:svg=\"urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0\" office:version=\"1.2\">",
    "<office:body><office:presentation>",
    "<draw:page draw:name=\"page1\"><draw:frame draw:name=\"t1\" svg:x=\"1cm\" svg:y=\"1cm\" svg:width=\"20cm\" svg:height=\"3cm\"><draw:text-box><text:p>URUKSLIDE deck marker title</text:p></draw:text-box></draw:frame></draw:page>",
    "<draw:page draw:name=\"page2\"><draw:frame draw:name=\"t2\" svg:x=\"1cm\" svg:y=\"1cm\" svg:width=\"20cm\" svg:height=\"3cm\"><draw:text-box><text:p>Second slide body URUKSLIDE</text:p></draw:text-box></draw:frame></draw:page>",
    "</office:presentation></office:body></office:document-content>'''",
    "manifest = '''<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
    "<manifest:manifest xmlns:manifest=\"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0\">",
    "<manifest:file-entry manifest:full-path=\"/\" manifest:media-type=\"application/vnd.oasis.opendocument.presentation\"/>",
    "<manifest:file-entry manifest:full-path=\"content.xml\" manifest:media-type=\"text/xml\"/>",
    "</manifest:manifest>'''",
    "z = zipfile.ZipFile(sys.argv[1], 'w')",
    "z.writestr(zipfile.ZipInfo('mimetype'), 'application/vnd.oasis.opendocument.presentation', compress_type=zipfile.ZIP_STORED)",
    "z.writestr('META-INF/manifest.xml', manifest)",
    "z.writestr('content.xml', content)",
    "z.close()",
  ].join("\n");
  const script = join(WORK, "bake_odp.py");
  writeFileSync(script, py);
  must("python3", [script, join(FIX, "deck.odp")]);
}
loBake("pptx", join(FIX, "deck.odp"));
loBake("ppt:MS PowerPoint 97", join(FIX, "deck.odp"));
// Spreadsheets: CSV literal → XLSX / ODS / XLS bakes via LibreOffice.
writeFileSync(join(FIX, "cells.csv"), "name,amount\napples,42\nURUKCELL,7\n");
writeFileSync(join(FIX, "cells.tsv"), "name\tamount\napples\t42\nURUKCELL\t7\n");
loBake("xlsx", join(FIX, "cells.csv"));
loBake("ods", join(FIX, "cells.xlsx"));
loBake("xls:MS Excel 97", join(FIX, "cells.xlsx"));
// Subtitles with the nasty cases baked in.
writeFileSync(
  join(FIX, "subs.srt"),
  "1\n00:00:01,000 --> 00:00:02,500\nHello URUK subs\n\n2\n01:00:00,000 --> 01:00:01,250\nSecond cue line\n",
);
writeFileSync(
  join(FIX, "subs.vtt"),
  "WEBVTT\n\nNOTE\nA comment block that must not become a cue\n\ncue-1\n02:03.500 --> 02:04.000 position:50% align:center\nShort time cue\n\n100:00:00.000 --> 100:00:01.500\nFar future cue\n",
);

// ---------------------------------------------------------------------------
// Stage 4 fixtures: tools.
// ---------------------------------------------------------------------------
console.log("· generating stage-4 fixtures…");
// Transparency fixture: red disc on a fully transparent canvas.
must("convert", ["-size", "200x160", "xc:none", "-fill", "#d23c28", "-draw", "circle 100,80 100,40", join(FIX, "alpha.png")]);
// Metadata fixture: a JPEG with a planted EXIF APP1 (Make/Model) spliced
// in by python (ImageMagick's -set exif:* does not write EXIF into JPEG
// on this build — the byte-level APP1 is the honest plant) plus an IM
// comment. The strip test then proves both are really gone.
must("convert", ["-size", "240x180", "plasma:fractal", "-quality", "92", join(FIX, "meta-base.jpg")]);
must("convert", [join(FIX, "meta-base.jpg"), "-set", "comment", "PLANTED-COMMENT-42", join(FIX, "meta-commented.jpg")]);
{
  const py = [
    "import struct, sys",
    "def build_exif(make, model):",
    "    entries, data = [], b''",
    "    base = 8 + 2 + 2*12 + 4",
    "    for tag, s in ((0x010F, make), (0x0110, model)):",
    "        b = s.encode() + b'\\x00'",
    "        if len(b) <= 4:",
    "            entries.append((tag, 2, len(b), b.ljust(4, b'\\x00')))",
    "        else:",
    "            entries.append((tag, 2, len(b), struct.pack('<I', base + len(data))))",
    "            data += b",
    "    tiff = b'II*\\x00' + struct.pack('<I', 8)",
    "    ifd = struct.pack('<H', len(entries))",
    "    for tag, typ, cnt, val in sorted(entries):",
    "        ifd += struct.pack('<HHI', tag, typ, cnt) + val",
    "    ifd += struct.pack('<I', 0)",
    "    payload = b'Exif\\x00\\x00' + tiff + ifd + data",
    "    return b'\\xff\\xe1' + struct.pack('>H', len(payload) + 2) + payload",
    "src = open(sys.argv[1], 'rb').read()",
    "assert src[:2] == b'\\xff\\xd8'",
    "open(sys.argv[2], 'wb').write(src[:2] + build_exif('UrukTestCam', 'Probe-9000') + src[2:])",
  ].join("\n");
  const script = join(WORK, "plant_exif.py");
  writeFileSync(script, py);
  must("python3", [script, join(FIX, "meta-commented.jpg"), join(FIX, "meta.jpg")]);
}
// Three-page PDF with one distinct marker per page (split/extract/
// rotate/redact proofs): p1 SECRETREDACT, p2 KEEPTEXT, p3 THIRDPAGE.
for (const [name, marker] of [["pg1", "SECRETREDACT"], ["pg2", "KEEPTEXT"], ["pg3", "THIRDPAGE"]]) {
  writeFileSync(join(FIX, `${name}.txt`), `${marker} marker line on this page\n\nBody text follows the marker so the page is a real document page.\n`);
  loBake("pdf", join(FIX, `${name}.txt`));
}
must("pdfunite", [join(FIX, "pg1.pdf"), join(FIX, "pg2.pdf"), join(FIX, "pg3.pdf"), join(FIX, "photo.pdf")]);
// Heavy PDF for the compress proof: a big noisy image as the only page.
must("convert", ["-size", "1600x1200", "plasma:fractal", join(FIX, "big.png")]);
must("convert", [join(FIX, "big.png"), join(FIX, "big.pdf")]);

function engineMetadata(path) {
  const r = run(ENGINE, ["metadata", path]);
  try {
    return JSON.parse(r.stdout);
  } catch {
    return null;
  }
}
function pdfWordBox(path, page, word) {
  const out = must("pdftotext", ["-f", String(page), "-l", String(page), "-bbox", path, "-"]);
  const m = out.match(new RegExp(`<word xMin="([\\d.]+)" yMin="([\\d.]+)" xMax="([\\d.]+)" yMax="([\\d.]+)">${word}</word>`));
  if (!m) return null;
  const [x1, y1, x2, y2] = m.slice(1).map(Number);
  return { page, x: x1 - 2, y: y1 - 2, w: x2 - x1 + 4, h: y2 - y1 + 4 };
}
const tool = (input, t) => ({ kind: "tool", input, tool: t, suffix: "converted" });

// Re-probe helpers for the zip-based office formats and PDFs.
const PY_ZIP_TEXT = [
  "import sys, zipfile, re",
  "path, prefix = sys.argv[1], sys.argv[2]",
  "try:",
  "    z = zipfile.ZipFile(path)",
  "except Exception:",
  "    sys.exit(0)",
  "out = []",
  "for n in z.namelist():",
  "    if n.startswith(prefix) and (n.endswith('.xml') or n.endswith('.xhtml') or n.endswith('.html')):",
  "        out.append(re.sub(r'<[^>]+>', ' ', z.read(n).decode('utf-8', 'replace')))",
  "print(' '.join(out))",
].join("\n");
function zipText(path, prefix) {
  const r = run("python3", ["-c", PY_ZIP_TEXT, path, prefix]);
  return r.stdout ?? "";
}
function pdfPagesOf(path) {
  const out = must("pdfinfo", [path]);
  const m = out.match(/Pages:\s+(\d+)/);
  return m ? Number(m[1]) : 0;
}
function pdfTextOf(path) {
  return must("pdftotext", [path, "-"]);
}
function identifyArg(path, fmt) {
  let r = run("identify", ["-format", fmt, path]);
  if (r.status !== 0) r = run("magick", ["identify", "-format", fmt, path]);
  return (r.stdout ?? "").trim();
}

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

// ---------------------------------------------------------------------------
console.log("· documents (stage 2)");
{
  const docx = join(FIX, "doc.docx");
  const toPdf = engineJob(conv(docx, "pdf"));
  check(
    "docx → pdf (LibreOffice, marker + ≥2 pages)",
    toPdf.ok && pdfPagesOf(toPdf.output) >= 2 && pdfTextOf(toPdf.output).includes("URUKDOC"),
    toPdf.error ?? "",
  );
  const toTxt = engineJob(conv(docx, "txt"));
  check("docx → txt", toTxt.ok && readFileSync(toTxt.output, "utf8").includes("URUKDOC"), toTxt.error ?? "");
  const toMd = engineJob(conv(docx, "md"));
  check(
    "docx → md (pandoc, heading survives)",
    toMd.ok && readFileSync(toMd.output, "utf8").includes("URUKDOC") && readFileSync(toMd.output, "utf8").includes("#"),
    toMd.error ?? "",
  );
  const toHtml = engineJob(conv(docx, "html"));
  check("docx → html", toHtml.ok && readFileSync(toHtml.output, "utf8").includes("URUKDOC"), toHtml.error ?? "");
  const toEpub = engineJob(conv(docx, "epub"));
  check("docx → epub", toEpub.ok && zipText(toEpub.output, "").includes("URUKDOC"), toEpub.error ?? "");

  const md2docx = engineJob(conv(join(FIX, "doc.md"), "docx"));
  check("md → docx", md2docx.ok && zipText(md2docx.output, "word/").includes("URUKDOC"), md2docx.error ?? "");
  const md2pdf = engineJob(conv(join(FIX, "doc.md"), "pdf"));
  check(
    "md → pdf (pandoc → docx → LibreOffice chain)",
    md2pdf.ok && pdfTextOf(md2pdf.output).includes("URUKDOC"),
    md2pdf.error ?? "",
  );
  const txt2docx = engineJob(conv(join(FIX, "notes.txt"), "docx"));
  check("txt → docx", txt2docx.ok && zipText(txt2docx.output, "word/").includes("URUKTXT"), txt2docx.error ?? "");
  const html2pdf = engineJob(conv(join(FIX, "page.html"), "pdf"));
  check("html → pdf", html2pdf.ok && pdfTextOf(html2pdf.output).includes("URUKHTML"), html2pdf.error ?? "");
  const rtf2docx = engineJob(conv(join(FIX, "doc.rtf"), "docx"));
  check("rtf → docx", rtf2docx.ok && zipText(rtf2docx.output, "word/").includes("URUKDOC"), rtf2docx.error ?? "");
  const odt2pdf = engineJob(conv(join(FIX, "doc.odt"), "pdf"));
  check("odt → pdf", odt2pdf.ok && pdfTextOf(odt2pdf.output).includes("URUKDOC"), odt2pdf.error ?? "");

  const epub = join(FIX, "doc.epub");
  const epub2txt = engineJob(conv(epub, "txt"));
  check("epub → txt (pandoc)", epub2txt.ok && readFileSync(epub2txt.output, "utf8").includes("URUKDOC"), epub2txt.error ?? "");
  const epub2md = engineJob(conv(epub, "md"));
  check("epub → md (pandoc)", epub2md.ok && readFileSync(epub2md.output, "utf8").includes("URUKDOC"), epub2md.error ?? "");
  const epub2pdf = engineJob(conv(epub, "pdf"));
  check("epub → pdf (pandoc → docx → LibreOffice chain)", epub2pdf.ok && pdfTextOf(epub2pdf.output).includes("URUKDOC"), epub2pdf.error ?? "");
}

console.log("· pdf (stage 2)");
{
  const pdf = join(FIX, "doc.pdf");
  const pages = pdfPagesOf(pdf);
  check("fixture pdf has ≥2 pages", pages >= 2, `pages=${pages}`);

  const toJpg = engineJob(conv(pdf, "jpg"));
  const jpg1 = join(FIX, "doc.converted-page-001.jpg");
  const jpg2 = join(FIX, "doc.converted-page-002.jpg");
  check(
    "pdf → jpg (all pages, page-### names)",
    toJpg.ok && existsSync(jpg1) && existsSync(jpg2) && Boolean(toJpg.note?.includes("pages")),
    toJpg.error ?? toJpg.note ?? "",
  );
  check(
    "pdf → jpg is 300 DPI (Letter width = 2550px)",
    existsSync(jpg1) && identifyFormat(jpg1) === "JPEG" && identifyArg(jpg1, "%w") === "2550",
    existsSync(jpg1) ? `fmt=${identifyFormat(jpg1)} w=${identifyArg(jpg1, "%w")}` : "missing",
  );
  const toPng = engineJob(conv(pdf, "png"));
  const png1 = join(FIX, "doc.converted-page-001.png");
  check("pdf → png (all pages)", toPng.ok && existsSync(png1) && identifyFormat(png1) === "PNG", toPng.error ?? "");
  const toTxt = engineJob(conv(pdf, "txt"));
  check("pdf → txt", toTxt.ok && readFileSync(toTxt.output, "utf8").includes("URUKDOC"), toTxt.error ?? "");
  const toMd = engineJob(conv(pdf, "md"));
  check("pdf → md (extracted text)", toMd.ok && readFileSync(toMd.output, "utf8").includes("URUKDOC"), toMd.error ?? "");
  const toDocx = engineJob(conv(pdf, "docx"));
  check(
    "pdf → docx (text-faithful, labelled)",
    toDocx.ok && zipText(toDocx.output, "word/").includes("URUKDOC") && Boolean(toDocx.note?.includes("Text-faithful")),
    toDocx.error ?? toDocx.note ?? "",
  );
  const toPptx = engineJob(conv(pdf, "pptx"));
  check(
    "pdf → pptx (one slide per page, labelled)",
    toPptx.ok && zipText(toPptx.output, "ppt/slides/").includes("URUKDOC") && Boolean(toPptx.note?.includes("Text-faithful")),
    toPptx.error ?? toPptx.note ?? "",
  );
  const toEpub = engineJob(conv(pdf, "epub"));
  check("pdf → epub (text-only, labelled)", toEpub.ok && zipText(toEpub.output, "").includes("URUKDOC"), toEpub.error ?? "");
  const toXlsx = engineJob(conv(pdf, "xlsx"));
  check(
    "pdf → xlsx refuses honestly",
    !toXlsx.ok && (toXlsx.error ?? "").startsWith("pdf_to_spreadsheet_unavailable"),
    toXlsx.error ?? "produced output?",
  );
}

console.log("· slides (stage 2)");
{
  const pptx = join(FIX, "deck.pptx");
  const toPdf = engineJob(conv(pptx, "pdf"));
  check(
    "pptx → pdf (2 slides, marker)",
    toPdf.ok && pdfPagesOf(toPdf.output) === 2 && pdfTextOf(toPdf.output).includes("URUKSLIDE"),
    toPdf.error ?? "",
  );
  const toJpg = engineJob(conv(pptx, "jpg"));
  check(
    "pptx → jpg (every slide, page-### names)",
    toJpg.ok && existsSync(join(FIX, "deck.converted-page-001.jpg")) && existsSync(join(FIX, "deck.converted-page-002.jpg")),
    toJpg.error ?? "",
  );
  const toPng = engineJob(conv(pptx, "png"));
  check(
    "pptx → png",
    toPng.ok && existsSync(join(FIX, "deck.converted-page-001.png")) && identifyFormat(join(FIX, "deck.converted-page-001.png")) === "PNG",
    toPng.error ?? "",
  );
  const ppt2pdf = engineJob(conv(join(FIX, "deck.ppt"), "pdf"));
  check("ppt (legacy input) → pdf", ppt2pdf.ok && pdfTextOf(ppt2pdf.output).includes("URUKSLIDE"), ppt2pdf.error ?? "");
  const odp2pdf = engineJob(conv(join(FIX, "deck.odp"), "pdf"));
  check("odp (extra input) → pdf", odp2pdf.ok && pdfTextOf(odp2pdf.output).includes("URUKSLIDE"), odp2pdf.error ?? "");
}

console.log("· spreadsheets (stage 2)");
{
  const xlsx = join(FIX, "cells.xlsx");
  const toCsv = engineJob(conv(xlsx, "csv"));
  check(
    "xlsx → csv (comma-separated values)",
    toCsv.ok && readFileSync(toCsv.output, "utf8").includes("URUKCELL") && readFileSync(toCsv.output, "utf8").includes("name,amount"),
    toCsv.error ?? "",
  );
  const toTsv = engineJob(conv(xlsx, "tsv"));
  check(
    "xlsx → tsv (real tabs)",
    toTsv.ok && readFileSync(toTsv.output, "utf8").includes("name\tamount") && readFileSync(toTsv.output, "utf8").includes("URUKCELL\t7"),
    toTsv.error ?? "",
  );
  const toOds = engineJob(conv(xlsx, "ods"));
  check("xlsx → ods", toOds.ok && zipText(toOds.output, "content.xml").includes("URUKCELL"), toOds.error ?? "");
  const toPdf = engineJob(conv(xlsx, "pdf"));
  check("xlsx → pdf", toPdf.ok && pdfTextOf(toPdf.output).includes("URUKCELL"), toPdf.error ?? "");
  const csv2xlsx = engineJob(conv(join(FIX, "cells.csv"), "xlsx"));
  check("csv → xlsx", csv2xlsx.ok && zipText(csv2xlsx.output, "xl/").includes("URUKCELL"), csv2xlsx.error ?? "");
  const ods2xlsx = engineJob(conv(join(FIX, "cells.ods"), "xlsx"));
  check("ods → xlsx", ods2xlsx.ok && zipText(ods2xlsx.output, "xl/").includes("URUKCELL"), ods2xlsx.error ?? "");
  const csv2tsv = engineJob(conv(join(FIX, "cells.csv"), "tsv"));
  check("csv → tsv", csv2tsv.ok && readFileSync(csv2tsv.output, "utf8").includes("name\tamount"), csv2tsv.error ?? "");
  const tsv2csv = engineJob(conv(join(FIX, "cells.tsv"), "csv"));
  check("tsv → csv", tsv2csv.ok && readFileSync(tsv2csv.output, "utf8").includes("name,amount"), tsv2csv.error ?? "");
  const xls2xlsx = engineJob(conv(join(FIX, "cells.xls"), "xlsx"));
  check("xls (legacy input) → xlsx", xls2xlsx.ok && zipText(xls2xlsx.output, "xl/").includes("URUKCELL"), xls2xlsx.error ?? "");
}

console.log("· subtitles (stage 2)");
{
  const srt = join(FIX, "subs.srt");
  const toVtt = engineJob(conv(srt, "vtt"));
  const vttOut = toVtt.ok ? readFileSync(toVtt.output, "utf8") : "";
  check(
    "srt → vtt (header + dot milliseconds)",
    toVtt.ok && vttOut.startsWith("WEBVTT") && vttOut.includes("00:00:01.000 --> 00:00:02.500") && vttOut.includes("Hello URUK subs"),
    toVtt.error ?? vttOut.slice(0, 120),
  );
  const toTxt = engineJob(conv(srt, "txt"));
  check(
    "srt → txt (dialogue only)",
    toTxt.ok && readFileSync(toTxt.output, "utf8") === "Hello URUK subs\nSecond cue line\n",
    toTxt.error ?? "",
  );
  const vtt = join(FIX, "subs.vtt");
  const toSrt = engineJob(conv(vtt, "srt"));
  const srtOut = toSrt.ok ? readFileSync(toSrt.output, "utf8") : "";
  check(
    "vtt → srt (settings dropped, NOTE skipped, indices added)",
    toSrt.ok && srtOut.includes("1\n00:02:03,500 --> 00:02:04,000") && srtOut.includes("Short time cue") && !srtOut.includes("comment block"),
    toSrt.error ?? srtOut.slice(0, 160),
  );
  check(
    "vtt → srt preserves 100-hour timestamps",
    toSrt.ok && srtOut.includes("100:00:00,000 --> 100:00:01,500"),
    srtOut.slice(0, 160),
  );
  const vtt2txt = engineJob(conv(vtt, "txt"));
  check(
    "vtt → txt (dialogue only, no NOTE)",
    vtt2txt.ok && readFileSync(vtt2txt.output, "utf8") === "Short time cue\nFar future cue\n",
    vtt2txt.error ?? "",
  );
}

// ---------------------------------------------------------------------------
console.log("· image tools (stage 4)");
{
  const png = join(FIX, "img.png"); // 320x240
  const cropped = engineJob(tool(png, { tool: "image_crop", x: 10, y: 10, w: 100, h: 80 }));
  check(
    "tool crop 320x240 → 100x80",
    cropped.ok && identifyArg(cropped.output, "%wx%h") === "100x80",
    cropped.error ?? "",
  );
  const resized = engineJob(tool(png, { tool: "image_resize", w: 160 }));
  check(
    "tool resize w=160 keeps ratio (160x120)",
    resized.ok && identifyArg(resized.output, "%wx%h") === "160x120",
    resized.error ?? "",
  );
  const rotated = engineJob(tool(png, { tool: "image_transform", rotate: 90, flip_h: false, flip_v: false }));
  check(
    "tool transform rotate 90 swaps dims (240x320)",
    rotated.ok && identifyArg(rotated.output, "%wx%h") === "240x320",
    rotated.error ?? "",
  );
  const flipped = engineJob(tool(png, { tool: "image_transform", rotate: 0, flip_h: true, flip_v: true }));
  check(
    "tool transform flips keep dims (320x240)",
    flipped.ok && identifyArg(flipped.output, "%wx%h") === "320x240",
    flipped.error ?? "",
  );
  const alpha = join(FIX, "alpha.png");
  check(
    "fixture alpha.png really has transparency",
    identifyArg(alpha, "%[opaque]").toLowerCase() === "false",
    identifyArg(alpha, "%[opaque]"),
  );
  const bg = engineJob(tool(alpha, { tool: "image_background", color: "#FFFFFF" }));
  check(
    "tool add-background flattens transparency (opaque)",
    bg.ok && identifyArg(bg.output, "%[opaque]").toLowerCase() === "true",
    bg.error ?? identifyArg(bg.output ?? alpha, "%[opaque]"),
  );
  const annotated = engineJob(
    tool(png, {
      tool: "image_annotate",
      shapes: [
        { shape: "rect", x: 20, y: 20, w: 120, h: 70 },
        { shape: "arrow", x1: 200, y1: 180, x2: 290, y2: 60 },
        { shape: "text", x: 26, y: 130, text: "URUKNOTE", size: 30 },
      ],
    }),
  );
  check(
    "tool annotate draws rect+arrow+text (output differs, same dims)",
    annotated.ok &&
      identifyArg(annotated.output, "%wx%h") === "320x240" &&
      statSync(annotated.output).size !== statSync(png).size,
    annotated.error ?? "",
  );
}

console.log("· metadata view + strip (stage 4)");
{
  const meta = join(FIX, "meta.jpg");
  const entries = engineMetadata(meta);
  const asMap = new Map(entries ?? []);
  check(
    "metadata view reads planted EXIF Make/Model + comment",
    asMap.get("exif:Make") === "UrukTestCam" &&
      asMap.get("exif:Model") === "Probe-9000" &&
      String(asMap.get("comment") ?? asMap.get("Comment") ?? "").includes("PLANTED-COMMENT-42"),
    JSON.stringify(entries)?.slice(0, 200) ?? "no entries",
  );
  const stripped = engineJob(tool(meta, { tool: "image_strip_metadata" }));
  const verbose = stripped.ok ? must("identify", ["-verbose", stripped.output]) : "";
  check(
    "tool strip removes planted EXIF + comment",
    stripped.ok && !verbose.includes("exif:") && !verbose.includes("PLANTED-COMMENT-42"),
    stripped.error ?? "planted metadata survived the strip",
  );
  const pdfMeta = engineMetadata(join(FIX, "photo.pdf"));
  check(
    "metadata view reads PDF info (Pages=3)",
    (pdfMeta ?? []).some(([k, v]) => k === "Pages" && v === "3"),
    JSON.stringify(pdfMeta)?.slice(0, 160) ?? "",
  );
  const mediaMeta = engineMetadata(join(FIX, "clip.mp4"));
  check(
    "metadata view reads media format + streams",
    (mediaMeta ?? []).some(([k]) => k === "Format") && (mediaMeta ?? []).some(([k]) => k === "Video"),
    JSON.stringify(mediaMeta)?.slice(0, 160) ?? "",
  );
}

console.log("· trim (stage 4)");
{
  const probeDuration = (path) => {
    const r = run(FFPROBE, ["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", path]);
    return Number((r.stdout ?? "").trim()) || 0;
  };
  const clip = join(FIX, "clip.mp4"); // 1.5 s
  const tail = engineJob(tool(clip, { tool: "media_trim", start: 0, end: 0.8 }));
  const tailDur = tail.ok ? probeDuration(tail.output) : 0;
  check(
    "tool trim tail 0→0.8s (stream copy)",
    tail.ok && tailDur > 0.45 && tailDur < 1.15,
    tail.error ?? `duration=${tailDur}`,
  );
  const seek = engineJob(tool(clip, { tool: "media_trim", start: 0.5, end: 1.2 }));
  const seekDur = seek.ok ? probeDuration(seek.output) : 0;
  check(
    "tool trim seek 0.5→1.2s (re-encode, accurate cut)",
    seek.ok && seekDur > 0.4 && seekDur < 1.05,
    seek.error ?? `duration=${seekDur}`,
  );
  const tone = join(FIX, "tone.wav"); // 1.0 s
  const at = engineJob(tool(tone, { tool: "media_trim", start: 0.3, end: 0.9 }));
  const atDur = at.ok ? probeDuration(at.output) : 0;
  check(
    "tool trim audio 0.3→0.9s",
    at.ok && atDur > 0.35 && atDur < 0.9,
    at.error ?? `duration=${atDur}`,
  );
}

console.log("· pdf tools (stage 4)");
{
  const pdf = join(FIX, "photo.pdf"); // 3 pages: SECRETREDACT / KEEPTEXT / THIRDPAGE
  check("fixture photo.pdf has 3 pages", pdfPagesOf(pdf) === 3, `pages=${pdfPagesOf(pdf)}`);

  const split = engineJob(tool(pdf, { tool: "pdf_split", ranges: [[1, 2], [3, 3]] }));
  const part2 = (split.output ?? "").replace("-p1-2.pdf", "-p3-3.pdf");
  check(
    "tool pdf split → p1-2 (2 pages) + p3-3 (1 page)",
    split.ok &&
      (split.output ?? "").endsWith("-p1-2.pdf") &&
      pdfPagesOf(split.output) === 2 &&
      existsSync(part2) &&
      pdfPagesOf(part2) === 1,
    split.error ?? split.output ?? "",
  );

  const extract = engineJob(tool(pdf, { tool: "pdf_extract", ranges: [[1, 1], [3, 3]] }));
  const extractText = extract.ok ? pdfTextOf(extract.output) : "";
  check(
    "tool pdf extract pages 1+3 (2 pages, page 2 absent)",
    extract.ok &&
      pdfPagesOf(extract.output) === 2 &&
      extractText.includes("SECRETREDACT") &&
      extractText.includes("THIRDPAGE") &&
      !extractText.includes("KEEPTEXT"),
    extract.error ?? extractText.slice(0, 120),
  );

  const docPdf = join(FIX, "doc.pdf");
  const docPages = pdfPagesOf(docPdf);
  const merged = engineJob(tool(pdf, { tool: "pdf_merge", inputs: [pdf, docPdf] }));
  check(
    "tool pdf merge (3 + doc pages)",
    merged.ok && pdfPagesOf(merged.output) === 3 + docPages,
    merged.error ?? `pages=${merged.ok ? pdfPagesOf(merged.output) : "?"} want ${3 + docPages}`,
  );

  const rot = engineJob(tool(pdf, { tool: "pdf_rotate", angle: 90 }));
  const rotInfo = rot.ok ? must("pdfinfo", ["-f", "1", "-l", "3", rot.output]) : "";
  const rotCount = (rotInfo.match(/rot:\s+90/g) ?? []).length;
  check(
    "tool pdf rotate sets /Rotate 90 on all pages, text intact",
    rot.ok && rotCount === 3 && pdfTextOf(rot.output).includes("KEEPTEXT"),
    rot.error ?? rotInfo.slice(0, 160),
  );

  const big = join(FIX, "big.pdf");
  const comp = engineJob(tool(big, { tool: "pdf_compress", tier: "screen" }));
  check(
    "tool pdf compress (screen tier) shrinks the heavy PDF",
    comp.ok && statSync(comp.output).size < statSync(big).size * 0.8,
    comp.error ?? `${statSync(big).size} → ${comp.ok ? statSync(comp.output).size : "?"}`,
  );

  const box = pdfWordBox(pdf, 1, "SECRETREDACT");
  check("redact fixture: SECRETREDACT word box found on page 1", Boolean(box), "pdftotext -bbox");
  const red = engineJob(tool(pdf, { tool: "pdf_redact", regions: [box] }));
  const redText = red.ok ? pdfTextOf(red.output) : "";
  check(
    "tool pdf redact REMOVES the text (not paint-over)",
    red.ok &&
      !redText.includes("SECRETREDACT") &&
      redText.includes("KEEPTEXT") &&
      redText.includes("THIRDPAGE") &&
      pdfPagesOf(red.output) === 3,
    red.error ?? redText.slice(0, 160),
  );
  check(
    "redact note discloses the image-rebuild of redacted pages",
    (red.note ?? "").includes("rebuilt as 300-DPI images"),
    red.note ?? "no note",
  );
}

console.log(`\n══ engine tests: ${passed} passed, ${failed} failed, ${gaps} known gaps ══`);
if (failures.length) {
  console.log("failures:\n - " + failures.join("\n - "));
  process.exit(1);
}
