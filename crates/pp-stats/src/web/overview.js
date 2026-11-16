/**
 * Overview 页面渲染与活动柱图。
 */

import {
  cacheRate,
  costNote,
  fillBuckets,
  metric,
  overviewTable,
  pricingNotice,
} from "./analytics-shared.js";
import {
  chartHit,
  descriptions,
  duration,
  h,
  icon,
  localTime,
  money,
  number,
  percent,
  short,
  sum,
} from "./format.js";
import { app } from "./state.js";

/** @typedef {import("../../web-types/api.d.ts").AnalyticsResponse} AnalyticsResponse */

/**
 * 渲染 Overview 活动柱图 SVG。
 * @param {Array<Partial<import("../../web-types/api.d.ts").AggregateBucket> & { timestamp: number }>} buckets
 * @param {"requests" | "tokens" | "cost" | "tokens_per_second" | "avg_ttft_ms"} [mode]
 * @returns {string}
 */
export function chart(buckets, mode = app.activity) {
  /** @type {Record<string, string>} */
  const labels = { tokens_per_second: "Tokens/s", avg_ttft_ms: "TTFT" };
  const label = labels[mode] || mode;
  /** @param {number} value */
  const formatted = (value) =>
    mode === "cost" ? money(value) : mode === "avg_ttft_ms" ? duration(value) : number(value);
  if (!buckets.some((b) => Number(b[mode] ?? 0) > 0)) {
    return '<div class="empty">No data in this range</div>';
  }
  const width = 900;
  const height = 270;
  const left = 42;
  const right = 7;
  const top = 16;
  const bottom = 34;
  const plotW = width - left - right;
  const plotH = height - top - bottom;
  let max = Math.max(...buckets.map((b) => Number(b[mode] ?? 0)), 0.000001);
  const magnitude = 10 ** Math.floor(Math.log10(max));
  max = Math.ceil(max / magnitude) * magnitude;
  const step = plotW / buckets.length;
  const bar = Math.min(60, step * 0.65);
  const labelEvery = Math.max(1, Math.ceil(buckets.length / 12));
  return `<svg viewBox="0 0 ${width} ${height}" preserveAspectRatio="none" role="img" aria-label="${h(label)} over time">${Array.from(
    { length: 5 },
    (_, i) => {
      const y = top + plotH - (i * plotH) / 4;
      return `<line x1="${left}" y1="${y}" x2="${width - right}" y2="${y}"/><text x="${left - 8}" y="${y + 4}" text-anchor="end">${mode === "cost" ? money((max * i) / 4) : mode === "avg_ttft_ms" ? duration((max * i) / 4) : short((max * i) / 4)}</text>`;
    },
  ).join("")}${buckets
    .map((b, i) => {
      const raw = b[mode];
      const value = Number(raw ?? 0);
      const failed = Number(b.failed ?? 0);
      const bh = (value / max) * plotH;
      const x = left + step * (i + 0.5) - bar / 2;
      const y = top + plotH - bh;
      const bucketLabel = new Date(b.timestamp).toLocaleString(
        "en-US",
        app.range === "1h" || app.range === "24h"
          ? { hour: "2-digit", minute: "2-digit", hour12: false }
          : { month: "short", day: "numeric" },
      );
      const detail = `${localTime(b.timestamp)}\n${label}: ${raw == null ? "Not reported" : formatted(value)}${mode === "requests" ? `\nFailed: ${number(failed)}` : ""}`;
      return `<rect x="${x}" y="${y}" width="${bar}" height="${bh}" fill="var(--accent)"/>${mode === "requests" && failed ? `<rect x="${x}" y="${y}" width="${bar}" height="${(failed / max) * plotH}" fill="var(--red)"/>` : ""}${chartHit(left + step * i, top, step, plotH, detail)}${i % labelEvery === 0 ? `<text x="${x + bar / 2}" y="${height - 12}" text-anchor="middle">${h(bucketLabel)}</text>` : ""}`;
    })
    .join("")}</svg>`;
}

/**
 * 渲染 Overview 页面。
 * @returns {string}
 */
export function overview() {
  const payload = /** @type {AnalyticsResponse | null} */ (app.data);
  if (!payload) return '<div class="loading">Loading statistics…</div>';
  const s = payload.summary;
  const buckets = fillBuckets(payload.buckets);
  const tokenTotal =
    s.input_tokens == null || s.output_tokens == null ? null : s.input_tokens + s.output_tokens;
  /** @type {Array<[string, number | null, string]>} */
  const mix = [
    ["Uncached input", s.uncached_tokens, "var(--blue)"],
    ["Cache read", s.cached_tokens, "var(--accent)"],
    ["Cache write", s.cache_write_tokens, "var(--yellow)"],
    ["Output", s.output_tokens, "var(--pink)"],
  ];
  const mixTotal = sum(mix.map((m) => m[1]));
  return `<h1>Overview</h1><p class="subtitle">Everything pp did across your sessions in the ${descriptions[app.range]}.</p><div class="panel metrics">${metric(
    "API-equivalent cost",
    money(s.cost),
    costNote(s),
    buckets.map((b) => Number(b.cost ?? 0)),
    "var(--pink)",
  )}${metric(
    "Requests",
    number(s.requests),
    `${number(s.failed)} failed`,
    buckets.map((b) => Number(b.requests ?? 0)),
  )}${metric(
    "Conversation tokens",
    short(tokenTotal),
    `${short(s.output_tokens)} output`,
    buckets.map((b) => Number(b.tokens ?? 0)),
  )}${metric("Cache rate", cacheRate(s), "— saved")}${metric(
    "Error rate",
    s.requests ? percent(s.failed, s.requests) : "0.0%",
    `${number(s.ok)} succeeded`,
    buckets.map((b) => (b.requests ? Number(b.failed ?? 0) / Number(b.requests) : 0)),
    "var(--red)",
  )}</div><div class="panel metrics small-metrics">${[
    ["Uncached input", short(s.uncached_tokens)],
    ["Cache read", short(s.cached_tokens)],
    ["Cache write", short(s.cache_write_tokens)],
    ["Output", short(s.output_tokens)],
    ["Premium requests", "—"],
    ["Tokens/s", s.tokens_per_second == null ? "—" : s.tokens_per_second.toFixed(1)],
    ["Avg latency", duration(s.avg_duration_ms)],
    ["Avg TTFT", duration(s.avg_ttft_ms)],
  ]
    .map(([label, value]) => metric(label, value))
    .join(
      "",
    )}</div><div class="graphs"><section class="panel graph"><div class="panel-heading"><div><h2>Activity</h2><p class="panel-description">${app.range === "1h" ? "Per 5 minutes" : app.range === "24h" ? "Per hour" : "Per day"}</p></div><div class="segmented" role="radiogroup" aria-label="Activity metric">${["requests", "tokens", "cost"].map((v) => `<button role="radio" aria-checked="${app.activity === v}" data-activity="${v}">${v[0].toUpperCase() + v.slice(1)}</button>`).join("")}</div></div><div class="chart">${chart(buckets)}</div></section><section class="panel graph"><h2>Token mix</h2><p class="panel-description">Where conversation tokens went</p><div class="token-bar">${mix.map((m) => `<span style="width:${mixTotal ? (100 * (m[1] ?? 0)) / mixTotal : 0}%;background:${m[2]}" title="${h(m[0])}: ${number(m[1])}"></span>`).join("")}</div><div class="legend">${mix.map((m) => `<span><i class="dot" style="background:${m[2]}"></i>${m[0]} ${m[1] == null ? "—" : mixTotal ? `${Math.round((100 * m[1]) / mixTotal)}%` : "—"}</span>`).join("")}</div>${s.usage_known < s.requests || s.cache_known < s.requests || s.cache_write_known < s.requests ? '<div class="unavailable">Some requests did not report token or cache usage.</div>' : ""}<div class="agent-label">By agent</div><div class="token-bar"><span style="width:${s.requests ? 100 : 0}%;background:var(--accent)"></span></div><div class="agent-row"><i class="dot" style="background:var(--accent)"></i>Main agent <span class="muted num">${number(s.requests)} req</span><span class="num">${short(tokenTotal)}</span><span class="percentage">${s.requests ? "100%" : "—"}</span></div><div class="agent-row"><i class="dot" style="background:var(--pink)"></i>Subagents <span class="muted num">0 req</span><span class="num">0</span><span class="percentage">${s.requests ? "0%" : "—"}</span></div></section></div><section class="panel"><div class="requests-heading panel-heading"><div><h2><i class="live-dot"></i> &nbsp;Latest requests</h2><p class="panel-description">Most recent model calls across every session</p></div><a class="all-requests" href="#/requests?range=${app.range}">All requests ${icon("arrow")}</a></div>${overviewTable(payload.latest || [])}</section>${pricingNotice()}`;
}
