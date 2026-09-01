import { useEffect, useMemo, useState } from "react";
import { BarChart3 } from "lucide-react";
import { channelApi, apiKeyApi, statsApi } from "../lib/api";
import { formatDuration, formatNumber } from "../lib/constants";
import { MultiLineChart, seriesColor } from "../components/MultiLineChart";
import type {
  ApiKey,
  ApiKeyStats,
  Channel,
  ChannelStats,
  ModelStats,
  TokenTrendPoint,
} from "../types";

type TimeRange = 24 | 168 | 720;
type MetricKey = "total_tokens" | "input_tokens" | "output_tokens" | "request_count" | "success_rate" | "avg_duration" | "avg_ttft" | "cache_read";
type Dimension = "model" | "channel" | "key";

const timeRanges: Array<{ value: TimeRange; label: string }> = [
  { value: 24, label: "24 小时" },
  { value: 168, label: "7 天" },
  { value: 720, label: "30 天" },
];

const metrics: Array<{ value: MetricKey; label: string; unit: "count" | "percent" | "ms" }> = [
  { value: "total_tokens", label: "Token 总量", unit: "count" },
  { value: "input_tokens", label: "输入 Token", unit: "count" },
  { value: "output_tokens", label: "输出 Token", unit: "count" },
  { value: "request_count", label: "请求量", unit: "count" },
  { value: "success_rate", label: "成功率", unit: "percent" },
  { value: "avg_duration", label: "平均延迟", unit: "ms" },
  { value: "avg_ttft", label: "首字延迟", unit: "ms" },
  { value: "cache_read", label: "缓存命中", unit: "count" },
];

const dimensions: Array<{ value: Dimension; label: string }> = [
  { value: "model", label: "模型" },
  { value: "channel", label: "渠道" },
  { value: "key", label: "密钥" },
];

function metricOf(point: TokenTrendPoint, metric: MetricKey): number | null {
  switch (metric) {
    case "total_tokens": return point.total_tokens;
    case "input_tokens": return point.input_tokens;
    case "output_tokens": return point.output_tokens;
    case "request_count": return point.request_count;
    case "success_rate":
      return point.request_count > 0 ? (point.success_count / point.request_count) * 100 : null;
    case "avg_duration": return point.avg_duration_ms;
    case "avg_ttft": return point.avg_ttft_ms;
    case "cache_read": return point.cache_read_tokens;
  }
}

function formatHourLabel(h: string): string {
  const d = new Date(h);
  return `${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")} ${String(d.getHours()).padStart(2, "0")}:00`;
}

export function StatsPage() {
  const [hours, setHours] = useState<TimeRange>(24);
  const [metric, setMetric] = useState<MetricKey>("total_tokens");
  const [dimension, setDimension] = useState<Dimension>("model");

  const [trend, setTrend] = useState<TokenTrendPoint[]>([]);
  const [modelStats, setModelStats] = useState<ModelStats[]>([]);
  const [channelStats, setChannelStats] = useState<ChannelStats[]>([]);
  const [apiKeyStats, setApiKeyStats] = useState<ApiKeyStats[]>([]);
  const [channels, setChannels] = useState<Channel[]>([]);
  const [apiKeys, setApiKeys] = useState<ApiKey[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;
    const doLoad = () => {
      Promise.all([
        statsApi.getTokenTrend(hours).catch(() => []),
        statsApi.getModelStats(hours).catch(() => []),
        statsApi.getChannelStats(hours).catch(() => []),
        statsApi.getApiKeyStats(hours).catch(() => []),
      ]).then(([t, m, c, k]) => {
        if (cancelled) return;
        setTrend(t);
        setModelStats(m);
        setChannelStats(c);
        setApiKeyStats(k);
        setLoading(false);
      });
    };
    setLoading(true);
    doLoad();
    const interval = setInterval(doLoad, 30000);
    return () => {
      cancelled = true;
      clearInterval(interval);
    };
  }, [hours]);

  // 名称映射（渠道/密钥表展示用）
  useEffect(() => {
    channelApi.getAll().then(setChannels).catch(() => {});
    apiKeyApi.getAll().then(setApiKeys).catch(() => {});
  }, []);

  const metricConf = metrics.find((m) => m.value === metric)!;
  const valueFormatter = (v: number) =>
    metricConf.unit === "percent"
      ? `${v.toFixed(0)}%`
      : metricConf.unit === "ms"
        ? formatDuration(Math.round(v))
        : formatNumber(Math.round(v));

  // 折线序列：按模型分解，取 Token 总量 top 8 的模型避免图表过密
  const chart = useMemo(() => {
    const hourSet = Array.from(new Set(trend.map((p) => p.hour))).sort();
    const totalsByModel = new Map<string, number>();
    for (const p of trend) {
      totalsByModel.set(p.model, (totalsByModel.get(p.model) ?? 0) + p.total_tokens);
    }
    const topModels = Array.from(totalsByModel.entries())
      .sort((a, b) => b[1] - a[1])
      .slice(0, 8)
      .map(([m]) => m);
    const byModelHour = new Map<string, Map<string, TokenTrendPoint>>();
    for (const p of trend) {
      if (!byModelHour.has(p.model)) byModelHour.set(p.model, new Map());
      byModelHour.get(p.model)!.set(p.hour, p);
    }
    const series = topModels.map((model, idx) => ({
      name: model,
      color: seriesColor(idx),
      values: hourSet.map((h) => {
        const point = byModelHour.get(model)?.get(h);
        return point ? metricOf(point, metric) : null;
      }),
    }));
    return { labels: hourSet, series };
  }, [trend, metric]);

  const channelName = (id: string) => channels.find((c) => c.id === id)?.name ?? id;
  const keyName = (id: string) => apiKeys.find((k) => k.id === id)?.name ?? id;

  return (
    <div className="page-shell space-y-5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="page-title flex items-center gap-2"><BarChart3 size={20} className="text-blue-600" /> 统计</h1>
          <p className="mt-1 text-sm text-slate-500">多维用量、成功率、延迟与缓存命中（30 秒自动刷新）</p>
        </div>
        <div className="flex rounded-xl border border-slate-200 bg-white p-1">
          {timeRanges.map(({ value, label }) => (
            <button
              key={value}
              onClick={() => setHours(value)}
              className={`rounded-lg px-3 py-1.5 text-xs font-medium transition-colors ${
                hours === value ? "bg-blue-600 text-white" : "text-slate-600 hover:bg-slate-50"
              }`}
            >
              {label}
            </button>
          ))}
        </div>
      </div>

      {/* 折线图：指标切换 */}
      <section className="surface p-5">
        <div className="mb-4 flex flex-wrap gap-1.5">
          {metrics.map(({ value, label }) => (
            <button
              key={value}
              onClick={() => setMetric(value)}
              className={`rounded-full border px-3 py-1 text-xs font-medium transition-colors ${
                metric === value
                  ? "border-blue-200 bg-blue-50 text-blue-700"
                  : "border-slate-200 bg-white text-slate-600 hover:bg-slate-50"
              }`}
            >
              {label}
            </button>
          ))}
        </div>
        {loading ? (
          <div className="flex h-60 items-center justify-center text-sm text-slate-400">加载中...</div>
        ) : chart.series.length === 0 ? (
          <div className="flex h-60 items-center justify-center text-sm text-slate-400">所选时间范围内暂无请求数据</div>
        ) : (
          <MultiLineChart
            labels={chart.labels}
            series={chart.series}
            valueFormatter={valueFormatter}
            labelFormatter={formatHourLabel}
          />
        )}
      </section>

      {/* 维度明细表 */}
      <section className="surface p-5">
        <div className="mb-4 flex items-center justify-between">
          <div className="flex rounded-xl border border-slate-200 bg-white p-1">
            {dimensions.map(({ value, label }) => (
              <button
                key={value}
                onClick={() => setDimension(value)}
                className={`rounded-lg px-3 py-1.5 text-xs font-medium transition-colors ${
                  dimension === value ? "bg-slate-900 text-white" : "text-slate-600 hover:bg-slate-50"
                }`}
              >
                {label}
              </button>
            ))}
          </div>
          <span className="text-xs text-slate-400">统计范围与上方时间选择一致</span>
        </div>

        {dimension === "model" && (
          <StatsTable
            head={["模型", "请求", "成功率", "输入", "输出", "Token", "平均延迟"]}
            rows={modelStats.map((s) => [
              s.model,
              formatNumber(s.request_count),
              `${(s.success_rate * 100).toFixed(1)}%`,
              formatNumber(s.input_tokens),
              formatNumber(s.output_tokens),
              formatNumber(s.total_tokens),
              formatDuration(Math.round(s.avg_latency_ms)),
            ])}
          />
        )}
        {dimension === "channel" && (
          <StatsTable
            head={["渠道", "调用", "成功", "失败", "成功率", "Token", "平均延迟"]}
            rows={channelStats.map((s) => [
              channelName(s.channel_id),
              formatNumber(s.total_calls),
              formatNumber(s.success_calls),
              formatNumber(s.failed_calls),
              s.total_calls > 0 ? `${((s.success_calls / s.total_calls) * 100).toFixed(1)}%` : "—",
              formatNumber(s.total_tokens),
              formatDuration(Math.round(s.avg_latency_ms)),
            ])}
          />
        )}
        {dimension === "key" && (
          <StatsTable
            head={["密钥", "调用", "成功", "失败", "成功率", "Token", "平均延迟"]}
            rows={apiKeyStats.map((s) => [
              keyName(s.api_key_id),
              formatNumber(s.total_calls),
              formatNumber(s.success_calls),
              formatNumber(s.failed_calls),
              s.total_calls > 0 ? `${((s.success_calls / s.total_calls) * 100).toFixed(1)}%` : "—",
              formatNumber(s.total_tokens),
              formatDuration(Math.round(s.avg_latency_ms)),
            ])}
          />
        )}
      </section>
    </div>
  );
}

function StatsTable({ head, rows }: { head: string[]; rows: string[][] }) {
  if (rows.length === 0) {
    return <div className="empty-state py-10 text-sm">所选时间范围内暂无数据</div>;
  }
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-sm">
        <thead>
          <tr className="border-b border-slate-200 text-left text-xs text-slate-500">
            {head.map((h, i) => (
              <th key={h} className={`pb-2 pr-4 font-medium ${i === 0 ? "" : "text-right"}`}>{h}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row, ri) => (
            <tr key={ri} className="border-b border-slate-100 last:border-0 hover:bg-slate-50/60">
              {row.map((cell, ci) => (
                <td key={ci} className={`py-2.5 pr-4 ${ci === 0 ? "font-medium text-slate-800" : "text-right text-slate-600"}`}>
                  {cell}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
