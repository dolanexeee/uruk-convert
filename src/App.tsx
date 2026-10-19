import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
  analyzePaths,
  cancelJob,
  DEMO,
  getAgentSettings,
  historyList,
  onHistoryUpdated,
  onJobEvent,
  onOrbDrop,
  openPath,
  pickFiles,
  pickFolder,
  pickOutputDir,
  revealInFolder,
  sensorStatus,
  setAgentSettings,
  startJob,
  toolsReport,
  type AgentSettings,
  type FileInfo,
  type HistoryRecord,
  type JobSpec,
  type SensorStatus,
} from "./lib/api";
import { dict, errorLabel, familyLabel, formatBytes, type Lang } from "./lib/i18n";
import Wheel from "./components/Wheel";

// ---------------------------------------------------------------------------
// Settings (persisted in the webview's local storage — the app is local-only)
// ---------------------------------------------------------------------------

interface Settings {
  lang: Lang;
  outputMode: "same" | "custom";
  customDir: string;
  suffix: "converted" | "none";
  imageQuality: number;
}

const DEFAULT_SETTINGS: Settings = {
  lang: "ar",
  outputMode: "same",
  customDir: "",
  suffix: "converted",
  imageQuality: 92,
};

function loadSettings(): Settings {
  try {
    const raw = localStorage.getItem("uruk-convert-settings");
    if (raw) return { ...DEFAULT_SETTINGS, ...(JSON.parse(raw) as Partial<Settings>) };
  } catch {
    /* fresh start */
  }
  return DEFAULT_SETTINGS;
}

// ---------------------------------------------------------------------------
// Queue rows
// ---------------------------------------------------------------------------

type RowStatus = "queued" | "running" | "done" | "error" | "cancelled";
type RowAction = "convert" | "extract" | "compress" | "archive";

interface Row {
  rowId: string;
  info: FileInfo;
  action: RowAction;
  target: string;
  status: RowStatus;
  percent: number;
  output?: string;
  note?: string | null;
  errorCode?: string;
  errorMessage?: string;
}

let seq = 1;
const nextId = () => `job-${Date.now()}-${seq++}`;

function SunDisc({ size = 44 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 48 48" aria-hidden>
      <circle cx="24" cy="24" r="21" fill="none" stroke="#c9a227" strokeWidth="2.4" />
      <polygon
        points="24,9 27.2,19.5 37,14.5 29.5,22.8 39,24 29.5,25.2 37,33.5 27.2,28.5 24,39 20.8,28.5 11,33.5 18.5,25.2 9,24 18.5,22.8 11,14.5 20.8,19.5"
        fill="#c9a227"
      />
      <circle cx="24" cy="24" r="4.6" fill="#0b1e3a" stroke="#c9a227" strokeWidth="1.6" />
    </svg>
  );
}

export default function App() {
  const [settings, setSettings] = useState<Settings>(loadSettings);
  const lang = settings.lang;
  const t = dict[lang];
  const [rows, setRows] = useState<Row[]>([]);
  const [dragging, setDragging] = useState(false);
  const [wheel, setWheel] = useState<{ info: FileInfo; x: number; y: number } | null>(null);
  const [showSettings, setShowSettings] = useState(false);
  const [compressRow, setCompressRow] = useState<Row | null>(null);
  const [compressMode, setCompressMode] = useState<"percent" | "target">("percent");
  const [compressValue, setCompressValue] = useState(50);
  const [tools, setTools] = useState<Record<string, string> | null>(null);
  const rowsRef = useRef<Row[]>([]);
  rowsRef.current = rows;
  // Stage 3: background-agent state (Rust-persisted), conversion history,
  // and the honest per-session sensor/tier status.
  const [agent, setAgent] = useState<AgentSettings | null>(null);
  const [history, setHistory] = useState<HistoryRecord[]>([]);
  const [sensorStat, setSensorStat] = useState<SensorStatus | null>(null);

  useEffect(() => {
    localStorage.setItem("uruk-convert-settings", JSON.stringify(settings));
    document.documentElement.lang = lang;
    document.documentElement.dir = lang === "ar" ? "rtl" : "ltr";
  }, [settings, lang]);

  useEffect(() => {
    toolsReport().then(setTools).catch(() => setTools(null));
  }, []);

  // -- job events ------------------------------------------------------------
  useEffect(() => {
    return onJobEvent((e) => {
      setRows((prev) =>
        prev.map((r) => {
          if (r.rowId !== e.id) return r;
          if (e.type === "progress") {
            return { ...r, status: "running", percent: e.percent ?? r.percent };
          }
          if (e.type === "done") {
            return { ...r, status: "done", percent: 100, output: e.output, note: e.note ?? null };
          }
          return {
            ...r,
            status: e.code === "cancelled" ? "cancelled" : "error",
            errorCode: e.code,
            errorMessage: e.message,
          };
        }),
      );
    });
  }, []);

  const outputDirFor = useCallback((): string | null => {
    if (settings.outputMode === "custom" && settings.customDir) return settings.customDir;
    return null;
  }, [settings]);

  // -- Stage 3: agent settings / history / status ---------------------------
  useEffect(() => {
    getAgentSettings().then(setAgent).catch(() => undefined);
    sensorStatus().then(setSensorStat).catch(() => undefined);
    historyList().then(setHistory).catch(() => undefined);
    return onHistoryUpdated((rec) =>
      setHistory((prev) => [rec, ...prev].slice(0, 300)),
    );
  }, []);

  const updateAgent = useCallback((patch: Partial<AgentSettings>) => {
    setAgent((prev) => {
      if (!prev) return prev;
      const next = { ...prev, ...patch };
      setAgentSettings(next);
      return next;
    });
  }, []);

  // Mirror the webview's output preferences into the Rust-side agent
  // settings so overlay (global wheel) conversions land exactly where
  // window jobs do, with the same suffix and quality.
  useEffect(() => {
    if (!agent) return;
    const next: AgentSettings = {
      ...agent,
      lang,
      output_dir: outputDirFor(),
      suffix: settings.suffix === "none" ? "" : "converted",
      image_quality: settings.imageQuality,
    };
    if (JSON.stringify(next) !== JSON.stringify(agent)) {
      setAgent(next);
      setAgentSettings(next);
    }
  }, [agent, lang, settings, outputDirFor]);

  const launchRow = useCallback(
    (row: Row) => {
      const outDir = outputDirFor();
      const suffix = settings.suffix === "none" ? "" : "converted";
      let spec: JobSpec;
      if (row.action === "extract") {
        spec = { kind: "extract_archive", input: row.info.path, output_dir: outDir };
      } else if (row.action === "compress") {
        spec = {
          kind: "compress_image",
          input: row.info.path,
          mode: "percent",
          value: 50,
          output_dir: outDir,
          suffix,
        };
      } else if (row.action === "archive" || row.info.family === "unknown" || row.info.is_dir) {
        spec = {
          kind: "create_archive",
          inputs: [row.info.path],
          format: row.target,
          output_dir: outDir,
          suffix,
        };
      } else {
        spec = {
          kind: "convert",
          input: row.info.path,
          target: row.target,
          output_dir: outDir,
          suffix,
          image_quality: settings.imageQuality,
        };
      }
      setRows((prev) =>
        prev.map((r) => (r.rowId === row.rowId ? { ...r, status: "running", percent: -1 } : r)),
      );
      startJob(row.rowId, spec);
    },
    [outputDirFor, settings],
  );

  const addRows = useCallback((infos: FileInfo[]): Row[] => {
    const newRows: Row[] = infos.map((info) => ({
      rowId: nextId(),
      info,
      action: "convert",
      target: info.targets[0] ?? "zip",
      status: "queued",
      percent: 0,
    }));
    setRows((prev) => [...prev, ...newRows]);
    return newRows;
  }, []);

  const handlePaths = useCallback(
    async (paths: string[], x?: number, y?: number) => {
      if (paths.length === 0) return;
      const infos = await analyzePaths(paths);
      // A single convertible file dropped on its own → the wheel.
      if (infos.length === 1 && infos[0].targets.length > 0 && !infos[0].is_dir) {
        setWheel({
          info: infos[0],
          x: x ?? window.innerWidth / 2,
          y: y ?? window.innerHeight / 2,
        });
        return;
      }
      addRows(infos);
    },
    [addRows],
  );

  // -- OS drag & drop --------------------------------------------------------
  useEffect(() => {
    if (DEMO) return;
    let unlisten: (() => void) | undefined;
    let webview;
    try {
      webview = getCurrentWebview();
    } catch {
      return; // not running inside Tauri (plain-browser preview)
    }
    webview
      .onDragDropEvent((event) => {
        const p = event.payload;
        if (p.type === "enter" || p.type === "over") {
          setDragging(true);
        } else if (p.type === "leave") {
          setDragging(false);
        } else if (p.type === "drop") {
          setDragging(false);
          const scale = window.devicePixelRatio || 1;
          void handlePaths(p.paths, p.position.x / scale, p.position.y / scale);
        }
      })
      .then((u) => {
        unlisten = u;
      })
      .catch(() => undefined);
    return () => unlisten?.();
  }, [handlePaths]);

  // Wayland orb tier: drops on the standing orb open this window with the
  // files routed through the normal pipeline.
  useEffect(() => onOrbDrop((paths) => void handlePaths(paths)), [handlePaths]);

  // Demo mode: auto-open the wheel for screenshots/design review.
  useEffect(() => {
    if (DEMO && window.location.hash.includes("demo-wheel")) {
      void analyzePaths(["/demo/holiday-photo.png"]).then((infos) => {
        if (infos[0]) {
          setWheel({ info: infos[0], x: window.innerWidth / 2, y: window.innerHeight / 2 });
        }
      });
    }
  }, []);

  const onBrowse = async () => {
    const paths = await pickFiles();
    void handlePaths(paths);
  };
  const onBrowseFolder = async () => {
    const dir = await pickFolder();
    if (dir) void handlePaths([dir]);
  };

  const pickFromWheel = (target: string) => {
    if (!wheel) return;
    const [row] = addRows([wheel.info]);
    setWheel(null);
    const chosen = { ...row, target };
    // launchRow reads from state map by rowId; give it the updated target.
    setRows((prev) => prev.map((r) => (r.rowId === row.rowId ? chosen : r)));
    setTimeout(() => launchRow(chosen), 0);
  };

  const startCompress = () => {
    if (!compressRow) return;
    const outDir = outputDirFor();
    const suffix = settings.suffix === "none" ? "" : "converted";
    const value =
      compressMode === "percent" ? compressValue : Math.max(1, compressValue) * 1024;
    setRows((prev) =>
      prev.map((r) =>
        r.rowId === compressRow.rowId ? { ...r, status: "running", percent: -1, action: "compress" } : r,
      ),
    );
    startJob(compressRow.rowId, {
      kind: "compress_image",
      input: compressRow.info.path,
      mode: compressMode,
      value,
      output_dir: outDir,
      suffix,
    });
    setCompressRow(null);
  };

  const queuedCount = useMemo(() => rows.filter((r) => r.status === "queued").length, [rows]);

  const statusText = (r: Row): string => {
    switch (r.status) {
      case "queued": return t.statusQueued;
      case "running": return r.percent >= 0 ? `${t.statusRunning} ${Math.round(r.percent)}%` : t.statusRunning;
      case "done": return t.statusDone;
      case "cancelled": return t.statusCancelled;
      default: return t.statusError;
    }
  };

  return (
    <div className={`app ${dragging ? "dragging" : ""}`}>
      <header className="topbar">
        <div className="brand">
          <SunDisc />
          <div>
            <div className="brand-name">{t.appName}</div>
            <div className="brand-sub">{t.appNameEn}</div>
          </div>
        </div>
        <div className="topbar-actions">
          <button
            type="button"
            className="chip"
            onClick={() => setSettings((s) => ({ ...s, lang: s.lang === "ar" ? "en" : "ar" }))}
          >
            {lang === "ar" ? "English" : "العربية"}
          </button>
          <button type="button" className="chip" onClick={() => setShowSettings((v) => !v)}>
            ⚙ {t.settings}
          </button>
        </div>
      </header>

      {showSettings && (
        <section className="settings-panel" aria-label={t.settings}>
          <h3>{t.settings}</h3>
          <div className="setting-row">
            <label>{t.outputFolder}</label>
            <div className="setting-controls">
              <label className="radio">
                <input
                  type="radio"
                  checked={settings.outputMode === "same"}
                  onChange={() => setSettings((s) => ({ ...s, outputMode: "same" }))}
                />
                {t.sameAsSource}
              </label>
              <label className="radio">
                <input
                  type="radio"
                  checked={settings.outputMode === "custom"}
                  onChange={() => setSettings((s) => ({ ...s, outputMode: "custom" }))}
                />
                {t.customFolder}
              </label>
              {settings.outputMode === "custom" && (
                <button
                  type="button"
                  className="chip"
                  onClick={async () => {
                    const d = await pickOutputDir();
                    if (d) setSettings((s) => ({ ...s, customDir: d }));
                  }}
                >
                  {settings.customDir || t.choose}
                </button>
              )}
            </div>
          </div>
          <div className="setting-row">
            <label>{t.fileSuffix}</label>
            <div className="setting-controls">
              <label className="radio">
                <input
                  type="radio"
                  checked={settings.suffix === "converted"}
                  onChange={() => setSettings((s) => ({ ...s, suffix: "converted" }))}
                />
                {t.suffixConverted}
              </label>
              <label className="radio">
                <input
                  type="radio"
                  checked={settings.suffix === "none"}
                  onChange={() => setSettings((s) => ({ ...s, suffix: "none" }))}
                />
                {t.suffixNone}
              </label>
            </div>
          </div>
          <div className="setting-row">
            <label>
              {t.imageQuality}: {settings.imageQuality}
            </label>
            <input
              type="range"
              min={40}
              max={100}
              value={settings.imageQuality}
              onChange={(e) => setSettings((s) => ({ ...s, imageQuality: Number(e.target.value) }))}
            />
          </div>
          <div className="setting-row">
            <label>{t.wheelBehavior}</label>
            <p className="setting-note">{t.wheelBehaviorNote}</p>
          </div>
          <div className="setting-row">
            <label>{t.agentTitle}</label>
            <p className="setting-note">{t.agentNote}</p>
          </div>
          <div className="setting-row">
            <label>{t.summonKey}</label>
            <div className="setting-controls">
              <select
                className="target-select"
                value={agent?.modifier ?? "shift"}
                onChange={(e) => updateAgent({ modifier: e.target.value })}
                aria-label={t.summonKey}
              >
                <option value="shift">{t.keyShift}</option>
                <option value="control">{t.keyControl}</option>
                <option value="alt">{t.keyAlt}</option>
              </select>
            </div>
          </div>
          <div className="setting-row">
            <label>{t.toolsKey}</label>
            <div className="setting-controls">
              <select
                className="target-select"
                value={agent?.tools_modifier ?? "alt"}
                onChange={(e) => updateAgent({ tools_modifier: e.target.value })}
                aria-label={t.toolsKey}
              >
                <option value="alt">{t.keyAlt}</option>
                <option value="control">{t.keyControl}</option>
                <option value="shift">{t.keyShift}</option>
              </select>
            </div>
          </div>
          <div className="setting-row">
            <label>{t.actionsKey}</label>
            <div className="setting-controls">
              <select
                className="target-select"
                value={agent?.actions_modifier ?? "control"}
                onChange={(e) => updateAgent({ actions_modifier: e.target.value })}
                aria-label={t.actionsKey}
              >
                <option value="control">{t.keyControl}</option>
                <option value="alt">{t.keyAlt}</option>
                <option value="shift">{t.keyShift}</option>
              </select>
            </div>
          </div>
          <div className="setting-row">
            <label>{t.autostart}</label>
            <input
              type="checkbox"
              checked={agent?.autostart ?? false}
              onChange={(e) => updateAgent({ autostart: e.target.checked })}
            />
          </div>
          <div className="setting-row">
            <label>{t.sensing}</label>
            <input
              type="checkbox"
              checked={agent?.sensing ?? true}
              onChange={(e) => updateAgent({ sensing: e.target.checked })}
            />
          </div>
          {sensorStat && (
            <div className="setting-row">
              <label>{t.sessionTier}</label>
              <p className="setting-note">
                {sensorStat.platform}
                {sensorStat.note ? ` — ${sensorStat.note}` : ""}
              </p>
            </div>
          )}
          <div className="setting-row">
            <label>{t.toolsTitle}</label>
            <p className="setting-note">
              {t.toolsNote}{" "}
              {tools &&
                (["ffmpeg", "magick", "sevenz", "soffice"] as const)
                  .map((k) => `${k === "sevenz" ? "7-Zip" : k === "soffice" ? "LibreOffice" : k}: ${tools[k] && tools[k] !== "MISSING" ? "✓" : "✗"}`)
                  .join(" · ")}
              <br />
              {t.libreOfficeNote}
            </p>
          </div>
        </section>
      )}

      <main>
        <section className={`hero ${rows.length > 0 ? "compact" : ""}`}>
          <div className="hero-disc">
            <SunDisc size={rows.length > 0 ? 54 : 92} />
          </div>
          <h1>{t.dropHere}</h1>
          <p>{t.tagline}</p>
          <p className="hero-hint">{t.dropHint}</p>
          <div className="hero-actions">
            <button type="button" className="primary" onClick={onBrowse}>
              {t.browse}
            </button>
            <button type="button" className="ghost" onClick={onBrowseFolder}>
              {t.browseFolder}
            </button>
          </div>
        </section>

        {rows.length > 0 && (
          <section className="queue">
            <div className="queue-head">
              <h2>{t.queue}</h2>
              {queuedCount > 0 && (
                <button
                  type="button"
                  className="primary small"
                  onClick={() => rowsRef.current.filter((r) => r.status === "queued").forEach(launchRow)}
                >
                  {t.convertAll} ({queuedCount})
                </button>
              )}
            </div>
            <ul className="rows">
              {rows.map((r) => (
                <li key={r.rowId} className={`row status-${r.status}`}>
                  <div className="row-main">
                    <div className="row-name" title={r.info.path}>
                      {r.info.name}
                    </div>
                    <div className="row-meta">
                      <span className="badge">{familyLabel(lang, r.info.family)}</span>
                      <span>{formatBytes(r.info.size, lang)}</span>
                      {r.info.dimensions && <span>{r.info.dimensions}</span>}
                      {r.info.duration ? (
                        <span>
                          {Math.floor(r.info.duration / 60)}:{String(Math.floor(r.info.duration % 60)).padStart(2, "0")}
                        </span>
                      ) : null}
                      <span className={`status-text st-${r.status}`}>{statusText(r)}</span>
                    </div>
                    {r.status === "error" && (
                      <div className="row-error">{errorLabel(lang, r.errorCode ?? "", r.errorMessage ?? "")}</div>
                    )}
                    {r.status === "done" && r.output && (
                      <div className="row-output" title={r.output}>
                        → {r.output.split("/").pop()}
                        {r.note ? <span className="row-note"> · {r.note}</span> : null}
                      </div>
                    )}
                    {r.status === "running" && (
                      <div className="progress">
                        <div
                          className={`progress-bar ${r.percent < 0 ? "indeterminate" : ""}`}
                          style={r.percent >= 0 ? { width: `${Math.max(2, r.percent)}%` } : undefined}
                        />
                      </div>
                    )}
                  </div>
                  <div className="row-actions">
                    {(r.status === "queued" || r.status === "error" || r.status === "cancelled") && (
                      <>
                        <select
                          className="target-select"
                          value={r.target}
                          onChange={(e) =>
                            setRows((prev) =>
                              prev.map((x) => (x.rowId === r.rowId ? { ...x, target: e.target.value } : x)),
                            )
                          }
                          aria-label="target format"
                        >
                          {r.info.targets.map((tg) => (
                            <option key={tg} value={tg}>
                              {tg.toUpperCase()}
                            </option>
                          ))}
                        </select>
                        <button type="button" className="primary small" onClick={() => launchRow(r)}>
                          {t.convert}
                        </button>
                      </>
                    )}
                    {r.info.family === "archive" && !r.info.is_dir && r.status !== "running" && (
                      <button
                        type="button"
                        className="ghost small"
                        onClick={() => {
                          const extractRow = { ...r, action: "extract" as RowAction };
                          setRows((prev) => prev.map((x) => (x.rowId === r.rowId ? extractRow : x)));
                          setTimeout(() => launchRow(extractRow), 0);
                        }}
                      >
                        {t.extract}
                      </button>
                    )}
                    {r.info.family === "image" && r.status !== "running" && (
                      <button
                        type="button"
                        className="ghost small"
                        onClick={() => {
                          setCompressRow(r);
                          setCompressMode("percent");
                          setCompressValue(50);
                        }}
                      >
                        {t.compress}
                      </button>
                    )}
                    {r.status === "running" && (
                      <button type="button" className="ghost small danger" onClick={() => cancelJob(r.rowId)}>
                        {t.cancel}
                      </button>
                    )}
                    {r.status === "done" && r.output && (
                      <>
                        <button type="button" className="ghost small" onClick={() => revealInFolder(r.output!)}>
                          {t.reveal}
                        </button>
                        <button type="button" className="ghost small" onClick={() => openPath(r.output!)}>
                          {t.open}
                        </button>
                      </>
                    )}
                    <button
                      type="button"
                      className="ghost small"
                      onClick={() => setRows((prev) => prev.filter((x) => x.rowId !== r.rowId))}
                    >
                      {t.remove}
                    </button>
                  </div>
                </li>
              ))}
            </ul>
          </section>
        )}

        <section className="queue history">
          <div className="queue-head">
            <h2>{t.historyTitle}</h2>
          </div>
          {history.length === 0 ? (
            <p className="setting-note">{t.historyEmpty}</p>
          ) : (
            <ul className="rows">
              {history.slice(0, 40).map((h, i) => (
                <li
                  key={`${h.ts}-${h.input_path}-${i}`}
                  className={`row status-${h.status === "done" ? "done" : "error"}`}
                >
                  <div className="row-main">
                    <div className="row-name" title={h.input_path}>
                      {h.input_name}{" "}
                      <span className="hist-arrow">→ {h.target.toUpperCase()}</span>
                    </div>
                    <div className="row-meta">
                      <span className="badge">
                        {h.origin === "overlay" ? t.historyOverlay : t.historyWindow}
                      </span>
                      <span>
                        {new Date(h.ts * 1000).toLocaleString(lang === "ar" ? "ar" : "en")}
                      </span>
                      <span
                        className={`status-text st-${h.status === "done" ? "done" : "error"}`}
                      >
                        {h.status === "done" ? t.statusDone : t.statusError}
                      </span>
                    </div>
                    {h.status === "error" && h.error && (
                      <div className="row-error">{h.error}</div>
                    )}
                    {h.status === "done" && h.output_path && (
                      <div className="row-output" title={h.output_path}>
                        → {h.output_path.split("/").pop()}
                      </div>
                    )}
                  </div>
                  {h.status === "done" && h.output_path && (
                    <div className="row-actions">
                      <button
                        type="button"
                        className="ghost small"
                        onClick={() => revealInFolder(h.output_path!)}
                      >
                        {t.reveal}
                      </button>
                    </div>
                  )}
                </li>
              ))}
            </ul>
          )}
        </section>
      </main>

      <footer className="footer">
        <span>{t.localOnly}</span>
        <span className="footer-sep">·</span>
        <span>{t.aboutLine}</span>
      </footer>

      {wheel && (
        <Wheel
          x={wheel.x}
          y={wheel.y}
          info={wheel.info}
          lang={lang}
          onPick={pickFromWheel}
          onDismissToQueue={() => {
            addRows([wheel.info]);
            setWheel(null);
          }}
        />
      )}

      {compressRow && (
        <div className="modal-backdrop" onClick={() => setCompressRow(null)} role="presentation">
          <div className="modal" onClick={(e) => e.stopPropagation()} role="dialog" aria-label={t.compressTitle}>
            <h3>{t.compressTitle}</h3>
            <div className="modal-file">{compressRow.info.name}</div>
            <div className="modal-meta">
              {t.originalSize}: {formatBytes(compressRow.info.size, lang)}
            </div>
            <label className="modal-label">{t.mode}</label>
            <select
              className="target-select"
              value={compressMode}
              onChange={(e) => setCompressMode(e.target.value as "percent" | "target")}
            >
              <option value="percent">{t.percentMode}</option>
              <option value="target">{t.targetMode}</option>
            </select>
            {compressMode === "percent" ? (
              <>
                <label className="modal-label">
                  {t.reduceBy}: {compressValue}%
                </label>
                <input
                  type="range"
                  min={5}
                  max={95}
                  value={compressValue}
                  onChange={(e) => setCompressValue(Number(e.target.value))}
                />
                <div className="modal-meta">
                  {t.outputAtMost}{" "}
                  {formatBytes(Math.max(0, compressRow.info.size * (1 - compressValue / 100)), lang)}
                </div>
              </>
            ) : (
              <>
                <label className="modal-label">{t.targetKb}</label>
                <input
                  type="number"
                  min={4}
                  value={compressValue}
                  onChange={(e) => setCompressValue(Number(e.target.value))}
                />
                <div className="modal-meta">
                  {t.outputAtMost} {formatBytes(compressValue * 1024, lang)}
                </div>
              </>
            )}
            <div className="modal-actions">
              <button type="button" className="ghost" onClick={() => setCompressRow(null)}>
                {t.close}
              </button>
              <button type="button" className="primary" onClick={startCompress}>
                {t.startCompress}
              </button>
            </div>
          </div>
        </div>
      )}

      {dragging && <div className="drag-veil">{t.dropHere}</div>}

      {agent && !agent.onboarding_done && (
        <div className="modal-backdrop" role="presentation">
          <div className="modal onboarding" role="dialog" aria-label={t.onboardingTitle}>
            <div className="onboarding-disc">
              <SunDisc size={64} />
            </div>
            <h3>{t.onboardingTitle}</h3>
            <p className="onboarding-body">{t.onboardingBody}</p>
            {sensorStat?.platform === "macos" && (
              <p className="setting-note">{t.onboardingPermMac}</p>
            )}
            {sensorStat?.platform.startsWith("wayland") && (
              <p className="setting-note">{t.onboardingPermWayland}</p>
            )}
            <div className="modal-actions">
              <button
                type="button"
                className="primary"
                onClick={() => updateAgent({ onboarding_done: true })}
              >
                {t.onboardingGotIt}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
