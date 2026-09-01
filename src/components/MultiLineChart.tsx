import { useEffect, useMemo, useRef, useState } from "react";

/**
 * 通用零依赖 SVG 多序列折线图（issue #51 统计页）。
 * 与仪表盘 TokenTrendChart 同一套手写折线手法（平滑路径 + 悬浮提示），
 * 但接收任意 `series`，供多指标/多维度复用。
 */

export interface LineSeries {
  name: string;
  color: string;
  /** 与 labels 等长的数值序列；null 表示该点无数据（断开）。 */
  values: Array<number | null>;
}

const CHART_COLORS = [
  "#3b82f6", "#10b981", "#8b5cf6", "#f59e0b", "#ef4444",
  "#06b6d4", "#6366f1", "#14b8a6", "#d946ef", "#f97316",
];

export function seriesColor(index: number): string {
  return CHART_COLORS[index % CHART_COLORS.length];
}

function smoothPath(points: { x: number; y: number }[]): string {
  if (points.length <= 2) return points.map((p, i) => `${i === 0 ? "M" : "L"}${p.x},${p.y}`).join(" ");
  let d = `M${points[0].x},${points[0].y}`;
  for (let i = 0; i < points.length - 1; i++) {
    const p0 = points[Math.max(0, i - 1)];
    const p1 = points[i];
    const p2 = points[i + 1];
    const p3 = points[Math.min(points.length - 1, i + 2)];
    const cp1x = p1.x + (p2.x - p0.x) / 6;
    const cp1y = p1.y + (p2.y - p0.y) / 6;
    const cp2x = p2.x - (p3.x - p1.x) / 6;
    const cp2y = p2.y - (p3.y - p1.y) / 6;
    d += ` C${cp1x},${cp1y} ${cp2x},${cp2y} ${p2.x},${p2.y}`;
  }
  return d;
}

export function MultiLineChart({
  labels,
  series,
  height = 240,
  valueFormatter,
  labelFormatter,
}: {
  labels: string[];
  series: LineSeries[];
  height?: number;
  valueFormatter?: (value: number) => string;
  labelFormatter?: (label: string) => string;
}) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [containerW, setContainerW] = useState(800);
  const [hoverIndex, setHoverIndex] = useState<number | null>(null);

  useEffect(() => {
    const update = () => setContainerW(containerRef.current?.clientWidth ?? 800);
    update();
    window.addEventListener("resize", update);
    return () => window.removeEventListener("resize", update);
  }, []);

  const fmt = valueFormatter ?? ((v: number) => String(v));
  const fmtLabel = labelFormatter ?? ((l: string) => l);

  const padding = { top: 14, right: 16, bottom: 26, left: 52 };
  const chartH = height - padding.top - padding.bottom;
  const chartW = Math.max(containerW - padding.left - padding.right, 100);
  const stepX = labels.length > 1 ? chartW / (labels.length - 1) : chartW;

  const maxValue = useMemo(() => {
    let max = 0;
    for (const s of series) {
      for (const v of s.values) {
        if (v != null && v > max) max = v;
      }
    }
    return max > 0 ? max : 1;
  }, [series]);

  const yTicks = 4;
  const xOf = (i: number) => padding.left + i * stepX;

  const niceMax = useMemo(() => {
    const magnitude = Math.pow(10, Math.floor(Math.log10(maxValue)));
    return Math.ceil(maxValue / magnitude) * magnitude;
  }, [maxValue]);

  const xTickEvery = Math.max(1, Math.ceil(labels.length / 8));

  return (
    <div ref={containerRef} className="relative w-full">
      <svg width="100%" height={height} className="block">
        {/* 网格与 y 轴 */}
        {Array.from({ length: yTicks + 1 }, (_, i) => {
          const v = (niceMax / yTicks) * i;
          const y = padding.top + chartH - (v / niceMax) * chartH;
          return (
            <g key={i}>
              <line x1={padding.left} x2={padding.left + chartW} y1={y} y2={y} stroke="#e2e8f0" strokeWidth="1" strokeDasharray={i === 0 ? undefined : "3 3"} />
              <text x={padding.left - 8} y={y + 3.5} textAnchor="end" fontSize="10" fill="#94a3b8">
                {fmt(v)}
              </text>
            </g>
          );
        })}
        {/* x 轴标签 */}
        {labels.map((label, i) =>
          i % xTickEvery === 0 ? (
            <text key={label + i} x={xOf(i)} y={height - 8} textAnchor="middle" fontSize="10" fill="#94a3b8">
              {fmtLabel(label)}
            </text>
          ) : null
        )}
        {/* 折线 */}
        {series.map((s) => {
          const points = s.values
            .map((v, i) => (v == null ? null : { x: xOf(i), y: padding.top + chartH - (v / niceMax) * chartH }))
            .filter((p): p is { x: number; y: number } => p != null);
          if (points.length === 0) return null;
          return (
            <path
              key={s.name}
              d={smoothPath(points)}
              fill="none"
              stroke={s.color}
              strokeWidth="2"
              strokeLinecap="round"
            />
          );
        })}
        {/* hover 指示线 */}
        {hoverIndex != null && (
          <line
            x1={xOf(hoverIndex)}
            x2={xOf(hoverIndex)}
            y1={padding.top}
            y2={padding.top + chartH}
            stroke="#94a3b8"
            strokeWidth="1"
            strokeDasharray="3 3"
          />
        )}
        {/* hover 捕获区 */}
        {labels.map((_, i) => (
          <rect
            key={`hit-${i}`}
            x={xOf(i) - stepX / 2}
            y={padding.top}
            width={stepX}
            height={chartH}
            fill="transparent"
            onMouseEnter={() => setHoverIndex(i)}
            onMouseLeave={() => setHoverIndex((cur) => (cur === i ? null : cur))}
          />
        ))}
      </svg>
      {/* 悬浮提示 */}
      {hoverIndex != null && (
        <div
          className="pointer-events-none absolute z-10 rounded-lg border border-slate-200 bg-white/95 px-3 py-2 text-xs shadow-lg"
          style={{
            left: Math.min(Math.max(xOf(hoverIndex) - 80, 0), Math.max(containerW - 190, 0)),
            top: 8,
          }}
        >
          <div className="mb-1 font-medium text-slate-700">{fmtLabel(labels[hoverIndex])}</div>
          {series.map((s) => (
            <div key={s.name} className="flex items-center gap-2 text-slate-600">
              <span className="h-1.5 w-1.5 shrink-0 rounded-full" style={{ background: s.color }} />
              <span className="min-w-0 flex-1 truncate">{s.name}</span>
              <span className="font-mono font-medium text-slate-800">
                {s.values[hoverIndex] == null ? "—" : fmt(s.values[hoverIndex]!)}
              </span>
            </div>
          ))}
        </div>
      )}
      {/* 图例 */}
      <div className="mt-1 flex flex-wrap gap-x-4 gap-y-1 px-2 text-xs text-slate-500">
        {series.map((s) => (
          <span key={s.name} className="flex items-center gap-1.5">
            <span className="h-1.5 w-4 rounded-full" style={{ background: s.color }} />
            <span className="max-w-40 truncate">{s.name}</span>
          </span>
        ))}
      </div>
    </div>
  );
}
