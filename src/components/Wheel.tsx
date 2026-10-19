import { useEffect, useMemo, useState } from "react";
import type { FileInfo } from "../lib/api";
import { dict, type Lang } from "../lib/i18n";

interface Props {
  x: number;
  y: number;
  info: FileInfo;
  lang: Lang;
  onPick: (target: string) => void;
  onDismissToQueue: () => void;
}

const SIZE = 480;
const C = SIZE / 2;
const R_OUT = 222;
const R_IN = 80;

function polar(cx: number, cy: number, r: number, deg: number): [number, number] {
  const rad = ((deg - 90) * Math.PI) / 180;
  return [cx + r * Math.cos(rad), cy + r * Math.sin(rad)];
}

function sectorPath(rOut: number, rIn: number, a0: number, a1: number): string {
  const [x0, y0] = polar(C, C, rOut, a0);
  const [x1, y1] = polar(C, C, rOut, a1);
  const [x2, y2] = polar(C, C, rIn, a1);
  const [x3, y3] = polar(C, C, rIn, a0);
  const large = a1 - a0 > 180 ? 1 : 0;
  return `M ${x0} ${y0} A ${rOut} ${rOut} 0 ${large} 1 ${x1} ${y1} L ${x2} ${y2} A ${rIn} ${rIn} 0 ${large} 0 ${x3} ${y3} Z`;
}

function starPoints(cx: number, cy: number, rOuter: number, rInner: number, points = 8): string {
  const pts: string[] = [];
  for (let i = 0; i < points * 2; i++) {
    const r = i % 2 === 0 ? rOuter : rInner;
    const [x, y] = polar(cx, cy, r, (i * 180) / points);
    pts.push(`${x.toFixed(1)},${y.toFixed(1)}`);
  }
  return pts.join(" ");
}

/** The signature radial sun-disc menu: valid target formats for the dropped
 *  file as segments of a golden wheel around a lapis center. Transform-only
 *  animations (spring entrance, hover growth) keep it at 60fps. */
export default function Wheel({ x, y, info, lang, onPick, onDismissToQueue }: Props) {
  const t = dict[lang];
  const [hover, setHover] = useState<number | null>(null);
  const targets = info.targets;
  const n = Math.max(targets.length, 1);
  const span = 360 / n;
  const gap = n > 1 ? 2.2 : 0;

  const segments = useMemo(
    () =>
      targets.map((target, i) => {
        const a0 = i * span + gap / 2;
        const a1 = (i + 1) * span - gap / 2;
        const mid = (a0 + a1) / 2;
        const [lx, ly] = polar(C, C, (R_OUT + R_IN) / 2, mid);
        const [tx, ty] = polar(0, 0, 10, mid);
        return { target, path: sectorPath(R_OUT, R_IN, a0, a1), lx, ly, tx, ty };
      }),
    [targets, span, gap],
  );

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onDismissToQueue();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onDismissToQueue]);

  // Keep the wheel on screen.
  const left = Math.min(Math.max(x - SIZE / 2, 8), Math.max(window.innerWidth - SIZE - 8, 8));
  const top = Math.min(Math.max(y - SIZE / 2, 8), Math.max(window.innerHeight - SIZE - 70, 8));

  return (
    <div className="wheel-backdrop" onClick={onDismissToQueue} role="presentation">
      <div
        className="wheel-wrap"
        style={{ left, top }}
        onClick={(e) => e.stopPropagation()}
        role="menu"
        aria-label={info.name}
      >
        <svg width={SIZE} height={SIZE} viewBox={`0 0 ${SIZE} ${SIZE}`}>
          <circle cx={C} cy={C} r={R_OUT + 6} className="wheel-halo" />
          {segments.map((seg, i) => (
            <g
              key={seg.target}
              className={`wheel-seg ${hover === i ? "hover" : ""}`}
              style={{
                transform: hover === i ? `translate(${seg.tx}px, ${seg.ty}px)` : undefined,
                animationDelay: `${i * 24}ms`,
              }}
              onMouseEnter={() => setHover(i)}
              onMouseLeave={() => setHover(null)}
              onClick={() => onPick(seg.target)}
              role="menuitem"
            >
              <path d={seg.path} className={i % 2 === 0 ? "seg-a" : "seg-b"} />
              <text x={seg.lx} y={seg.ly} className="seg-label" dominantBaseline="middle">
                {seg.target.toUpperCase()}
              </text>
            </g>
          ))}
          <circle cx={C} cy={C} r={R_IN - 6} className="wheel-center" />
          <polygon points={starPoints(C, C - 14, 24, 10)} className="wheel-star" />
          <circle cx={C} cy={C - 14} r={6.5} className="wheel-star-core" />
          <text x={C} y={C + 22} className="wheel-file-ext">
            {(info.ext || "•").toUpperCase()}
          </text>
          <text x={C} y={C + 44} className="wheel-file-name">
            {info.name.length > 22 ? `${info.name.slice(0, 21)}…` : info.name}
          </text>
        </svg>
        <button type="button" className="wheel-toqueue" onClick={onDismissToQueue}>
          {t.toQueue} ↓
        </button>
      </div>
    </div>
  );
}
