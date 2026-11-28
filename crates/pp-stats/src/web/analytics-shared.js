/**
 * Stats bucketing, card, table, cost breakdown and stacked bar primitives shared by Overview / Costs /
 * Projects.
 */

import {
  chartHit,
  descriptions,
  detailed,
  duration,
  h,
  icon,
  localTime,
  money,
  number,
  percent,
  relative,
  short,
  sum,
  total,
} from "./format.js";
import { app } from "./state.js";

/** @typedef {import("../../web-types/api.d.ts").AggregateBucket} AggregateBucket */
/** @typedef {import("../../web-types/api.d.ts").AggregateTotals} AggregateTotals */
/** @typedef {import("../../web-types/api.d.ts").AnalyticsResponse} AnalyticsResponse */
/** @typedef {import("../../web-types/api.d.ts").CostSnapshot} CostSnapshot */
/** @typedef {import("../../web-types/api.d.ts").RequestRow} RequestRow */

export const colors = [
  "var(--accent)",
  "var(--pink)",
  "var(--blue)",
  "var(--yellow)",
  "var(--green)",
  "var(--red)",
  "var(--muted)",
];

/** @type {Array<[keyof CostSnapshot, string, string]>} */
export const componentNames = [
  ["input", "Input", colors[2]],
  ["cache_read", "Cache read", colors[0]],
  ["cache_write", "Cache write", colors[3]],
  ["output", "Output", colors[1]],
];

/**
 * Fills in consecutive empty time buckets for the range.
 * @template {{ timestamp: number }} T
 * @param {T[]} buckets
 * @param {import("../../web-types/api.d.ts").StatsRange} [range]
 * @param {number} [nowMs]
 * @returns {Array<Partial<AggregateBucket> & { timestamp: number }>}
 */
export function fillBuckets(
  buckets,
  range = app.range,
  nowMs = app.data?.updated_at ?? Date.now(),
) {
  if (!buckets.length) return [];
  let start;
  if (range === "1h") start = nowMs - 3600000;
  else if (range === "24h") start = nowMs - 86400000;
  else if (range === "all") start = buckets[0].timestamp;
  else start = nowMs - Number.parseInt(range, 10) * 86400000;
  const fixed = range === "1h" ? 300000 : range === "24h" ? 3600000 : null;
  let t = 0;
  if (fixed) {
    t = Math.floor(start / fixed) * fixed;
  } else {
    const d = new Date(start);
    d.setHours(0, 0, 0, 0);
    t = d.getTime();
  }
  /** @type {Map<number, Partial<AggregateBucket> & { timestamp: number }>} */
  const map = new Map(
    buckets.map((b) => [
      b.timestamp,
      /** @type {Partial<AggregateBucket> & { timestamp: number }} */ (/** @type {unknown} */ (b)),
    ]),
  );
  /** @type {Array<Partial<AggregateBucket> & { timestamp: number }>} */
  const result = [];
  while (t <= nowMs) {
    result.push(map.get(t) || { timestamp: t, requests: 0, tokens: 0, failed: 0 });
    if (fixed) {
      t += fixed;
    } else {
      const d = new Date(t);
      d.setDate(d.getDate() + 1);
      d.setHours(0, 0, 0, 0);
      t = d.getTime();
    }
  }
  return result;
}

/**
 * Renders a trend line (used by the legacy Overview/Requests/Costs/Projects stat cards).
 * @param {number[]} values
 * @param {string} color
 * @param {string} [label]
 * @param {number[]} [timestamps]
 * @returns {string}
 */
export function sparkline(values, color, label = "Requests", timestamps = []) {
  if (!Array.isArray(values)) return "";
  if (!values.length) return '<div class="sparkline"></div>';
  const max = Math.max(...values, 1);
  const points = values
    .map(
      (v, i) =>
        `${values.length === 1 ? 100 : (i * 260) / (values.length - 1)},${35 - (v / max) * 30}`,
    )
    .join(" ");
  return `<svg class="sparkline" viewBox="0 0 260 38" preserveAspectRatio="none"><polygon points="0,38 ${points} 260,38" fill="${color}" opacity=".12"/><polyline points="${points}" fill="none" stroke="${color}" stroke-width="1.6"/>${values
    .map((v, i) =>
      chartHit(
        (i * 260) / values.length,
        0,
        260 / values.length,
        38,
        `${timestamps[i] == null ? `Bucket ${i + 1}` : localTime(timestamps[i])}\n${label}: ${
          label.includes("cost") ? money(v) : label === "Error rate" ? percent(v, 1) : number(v)
        }`,
      ),
    )
    .join("")}</svg>`;
}

/**
 * Renders a legacy stat card.
 * @param {string} label
 * @param {string} value
 * @param {string} [note]
 * @param {number[] | ""} [spark]
 * @param {string} [color]
 * @returns {string}
 */
export function metric(label, value, note = "", spark = "", color = "var(--accent)") {
  const analytics = /** @type {AnalyticsResponse | null} */ (app.data);
  const timestamps = Array.isArray(spark)
    ? fillBuckets(analytics?.buckets || []).map((b) => b.timestamp)
    : [];
  return `<div class="metric"><div class="metric-label">${h(label)}</div><div class="metric-value" title="${h(value)}">${h(value)}</div>${note ? `<div class="metric-note">${h(note)}</div>` : ""}${Array.isArray(spark) ? sparkline(spark, color, label, timestamps) : ""}</div>`;
}

/**
 * Renders the request status badge.
 * @param {Pick<RequestRow, "status" | "stop_reason">} r
 * @param {boolean} [compact]
 * @returns {string}
 */
export function statusBadge(r, compact = false) {
  const label = { ok: "OK", failed: "Failed", aborted: "Aborted" }[r.status] || "Unknown";
  return compact
    ? `<span class="badge ${h(r.status)}">${label}</span>`
    : `<div class="status-text ${h(r.status)}"><i></i><div>${label}<div class="stop">${h(r.stop_reason || "")}</div></div></div>`;
}

/**
 * Renders the recent requests summary table (shared by Overview and the Projects detail).
 * @param {RequestRow[]} rows
 * @returns {string}
 */
export function overviewTable(rows) {
  return `<div class="table-wrap"><table><thead><tr>${["Model", "When", "Tokens", "Cost", "Duration", "Status"].map((s, i) => `<th${i > 1 ? ' class="right"' : ""}>${s}</th>`).join("")}</tr></thead><tbody>${
    rows.length
      ? rows
          .map(
            (r) =>
              `<tr tabindex="0" data-id="${r.id}" aria-label="View ${h(r.model)} request ${r.id}"><td><div class="model-name">${h(r.model)}</div><div class="provider">${h(r.provider)}</div></td><td title="${h(localTime(r.timestamp))}">${relative(r.timestamp)}</td><td class="right num">${number(total(r))}</td><td class="right num">${detailed(r.cost, money(r.cost), " USD")}</td><td class="right num">${detailed(r.duration_ms, duration(r.duration_ms), " ms")}</td><td class="right">${statusBadge(r, true)}</td></tr>`,
          )
          .join("")
      : '<tr><td colspan="6"><div class="empty">No requests in this range</div></td></tr>'
  }</tbody></table></div>`;
}

/**
 * Renders a key/value grid.
 * @param {Array<[string, unknown]>} items
 * @returns {string}
 */
export function detailsGrid(items) {
  return `<dl class="detail-grid">${items.map(([key, value]) => `<div><dt>${h(key)}</dt><dd>${h(value)}</dd></div>`).join("")}</dl>`;
}

/** @param {number | null | undefined} n */
export const averageSpeed = (n) => (n == null ? "—" : n.toFixed(1));

/** @param {Pick<AggregateTotals, "cached_tokens" | "input_tokens">} g */
export const cacheRate = (g) => percent(g.cached_tokens, g.input_tokens);

/** @param {Pick<AggregateTotals, "priced" | "unpriced">} g */
export const costNote = (g) => `${number(g.priced)} priced · ${number(g.unpriced)} unpriced`;

/** @param {Pick<AggregateTotals, "priced" | "cost">} g */
export const costPerRequest = (g) => (g.priced && g.cost != null ? money(g.cost / g.priced) : "—");

/**
 * @param {string} kind
 * @param {string} key
 */
export const groupKey = (kind, key) => encodeURIComponent(`${kind}:${key}`);

export function pricingNotice() {
  return '<div class="pricing-note" role="note">Costs and rates are computed and stored with each request at record time, using the model configuration in effect then. Unpriced requests are excluded; missing prices or usage are never treated as zero or repriced at current rates. Cache writes require reported usage and a configured cacheWrite rate when nonzero. Subscription quotas are not reported by pp.</div>';
}

/**
 * @param {string} key
 * @param {Array<[string, string]>} options
 * @param {string} selected
 * @param {string} label
 */
export function segmentControls(key, options, selected, label) {
  return `<div class="segmented" role="radiogroup" aria-label="${h(label)}">${options.map(([id, name]) => `<button role="radio" aria-checked="${id === selected}" data-${key}="${id}">${h(name)}</button>`).join("")}</div>`;
}

/**
 * @param {string} title
 * @param {string} description
 * @param {string} content
 * @param {string} [controls]
 */
export function chartPanel(title, description, content, controls = "") {
  return `<section class="panel graph analytics-panel"><div class="panel-heading"><div><h2>${h(title)}</h2><p class="panel-description">${h(description)}</p></div>${controls}</div>${content}</section>`;
}

/** @param {Pick<AggregateTotals, "uncached_tokens" | "cached_tokens" | "cache_write_tokens" | "output_tokens" | "usage_known" | "cache_known" | "cache_write_known" | "requests">} g */
export function tokenMix(g) {
  /** @type {Array<[string, number | null, string]>} */
  const parts = [
    ["Uncached input", g.uncached_tokens, colors[2]],
    ["Cache read", g.cached_tokens, colors[0]],
    ["Cache write", g.cache_write_tokens, colors[3]],
    ["Output", g.output_tokens, colors[1]],
  ];
  const known = sum(parts.map((p) => p[1]));
  return `<div class="token-bar">${parts.map(([name, value, color]) => `<span style="width:${known ? (100 * (value ?? 0)) / known : 0}%;background:${color}" title="${h(name)}: ${number(value)}"></span>`).join("")}</div><div class="legend">${parts.map(([name, value, color]) => `<span><i class="dot" style="background:${color}"></i>${h(name)} ${short(value)} · ${percent(value, known)}</span>`).join("")}</div><p class="panel-description">${g.usage_known < g.requests || g.cache_known < g.requests || g.cache_write_known < g.requests ? "Some token or cache usage is unknown. " : ""}</p>`;
}

/** @param {Pick<AggregateTotals, "cost" | "cost_components">} g */
export function splitBar(g) {
  return `<div class="cost-split" title="Input · cache read · cache write · output">${componentNames.map(([key, name, color]) => `<span style="width:${g.cost ? (100 * (g.cost_components?.[key] ?? 0)) / g.cost : 0}%;background:${color}" title="${name}: ${money(g.cost_components?.[key])}"></span>`).join("")}</div>`;
}

/** @param {Pick<AggregateTotals, "cost" | "cost_components" | "priced" | "unpriced">} g */
export function breakdown(g) {
  return `<div class="component-list">${componentNames.map(([key, name, color]) => `<div><span><i class="dot" style="background:${color}"></i>${name}</span><b class="num">${money(g.cost_components?.[key])}</b><span class="num muted">${percent(g.cost_components?.[key], g.cost)}</span></div>`).join("")}<div><span>Total</span><b class="num">${money(g.cost)}</b><span></span></div></div>${splitBar(g)}<p class="panel-description">${costNote(g)}</p>`;
}

/**
 * @typedef {{
 *   key: string,
 *   label?: string,
 *   color?: string,
 *   cost?: number | null,
 *   requests?: number | null,
 *   tokens?: number | null,
 *   buckets: Array<{ timestamp: number } & Partial<AggregateBucket>>,
 *   daily: Array<{ timestamp: number } & Partial<AggregateBucket>>,
 * }} StackedSeriesGroup
 */

/**
 * Renders a stacked bar chart (used by the Costs page).
 * @param {StackedSeriesGroup[]} groups
 * @param {"cost" | "requests" | "tokens"} metricName
 * @param {boolean} [share]
 * @param {boolean} [daily]
 * @returns {string}
 */
export function stackedChart(groups, metricName, share = false, daily = false) {
  if (!groups.length) return '<div class="empty">No data in this range</div>';
  const visibleGroups = groups.filter((g) => !app.hiddenSeries.has(g.key));
  if (!visibleGroups.length) {
    return '<div class="empty">Select a legend item to show its series.</div>';
  }
  const analytics = /** @type {AnalyticsResponse} */ (app.data);
  const timestamps = [
    ...new Set(groups.flatMap((g) => (daily ? g.daily : g.buckets).map((b) => b.timestamp))),
  ].sort((a, b) => a - b);
  /** @type {Array<{ timestamp: number }>} */
  const timeline = [];
  if (daily && timestamps.length) {
    const start =
      app.range === "all"
        ? timestamps[0]
        : Math.floor(
            (analytics.updated_at -
              (app.range === "1h"
                ? 3600000
                : app.range === "24h"
                  ? 86400000
                  : Number.parseInt(app.range, 10) * 86400000)) /
              86400000,
          ) * 86400000;
    for (let timestamp = start; timestamp <= analytics.updated_at; timestamp += 86400000) {
      timeline.push({ timestamp });
    }
  } else if (!daily) {
    timeline.push(...fillBuckets(timestamps.map((timestamp) => ({ timestamp }))));
  }
  const series = visibleGroups.map(
    (g) =>
      new Map(
        (daily ? g.daily : g.buckets).map((b) => [
          b.timestamp,
          /** @type {number | null | undefined} */ (b[metricName]),
        ]),
      ),
  );
  const totals = timeline.map((b) => series.reduce((s, m) => s + (m.get(b.timestamp) ?? 0), 0));
  if (!totals.some((v) => v > 0)) {
    return `<div class="empty">${metricName === "cost" && !analytics.summary.priced && analytics.summary.requests ? "No priced usage in this range" : metricName === "cost" && analytics.summary.priced ? "No estimated cost for selected series" : "No data in this range"}</div>`;
  }
  const width = 900;
  const height = 265;
  const left = 48;
  const top = 16;
  const plotH = 210;
  const plotW = 840;
  const step = plotW / timeline.length;
  const bar = Math.min(60, step * 0.72);
  const max = share ? 100 : Math.max(...totals) * 1.05;
  /** @param {number} v */
  const format = (v) => (share ? `${Math.round(v)}%` : metricName === "cost" ? money(v) : short(v));
  let svg = `<svg viewBox="0 0 ${width} ${height}" preserveAspectRatio="none" role="img" aria-label="${h(metricName)} by ${daily ? "UTC day" : "time bucket"}">`;
  for (let i = 0; i < 5; i++) {
    const y = top + plotH * (1 - i / 4);
    svg += `<line x1="${left}" y1="${y}" x2="890" y2="${y}"/><text x="${left - 7}" y="${y + 4}" text-anchor="end">${h(format((max * i) / 4))}</text>`;
  }
  timeline.forEach((b, i) => {
    let offset = 0;
    visibleGroups.forEach((g, j) => {
      const raw = series[j].get(b.timestamp) ?? 0;
      const value = share ? (totals[i] ? (100 * raw) / totals[i] : 0) : raw;
      const bh = (value / max) * plotH;
      const color = g.color || colors[groups.indexOf(g) % colors.length];
      svg += `<rect x="${left + step * (i + 0.5) - bar / 2}" y="${top + plotH - offset - bh}" width="${bar}" height="${bh}" fill="${color}"><title>${h(g.label ?? g.key)} · ${daily ? new Date(b.timestamp).toISOString().slice(0, 10) : h(localTime(b.timestamp))}: ${h(metricName === "cost" ? money(raw) : number(raw))}${share ? ` · ${format(value)}` : ""}</title></rect>`;
      offset += bh;
    });
    const bucketTime = daily
      ? `${new Date(b.timestamp).toISOString().slice(0, 10)} UTC`
      : localTime(b.timestamp);
    const details = visibleGroups.map((g, j) => {
      const raw = series[j].has(b.timestamp) ? series[j].get(b.timestamp) : 0;
      return `${g.label ?? g.key}: ${raw == null ? "Not reported" : metricName === "cost" ? money(raw) : number(raw)}${share && raw != null ? ` · ${totals[i] ? percent(raw, totals[i]) : "0%"}` : ""}`;
    });
    const partial = series.some((s) => s.has(b.timestamp) && s.get(b.timestamp) == null);
    svg += chartHit(
      left + step * i,
      top,
      step,
      plotH,
      [
        bucketTime,
        ...details,
        `Total${partial ? " (known)" : ""}: ${metricName === "cost" ? money(totals[i]) : number(totals[i])}`,
      ].join("\n"),
    );
    if (i % Math.max(1, Math.ceil(timeline.length / 10)) === 0) {
      const label = new Date(b.timestamp).toLocaleString(
        "en-US",
        daily
          ? { month: "short", day: "numeric", timeZone: "UTC" }
          : app.range === "1h" || app.range === "24h"
            ? { hour: "2-digit", minute: "2-digit", hour12: false }
            : { month: "short", day: "numeric" },
      );
      svg += `<text x="${left + step * (i + 0.5)}" y="${height - 10}" text-anchor="middle">${h(label)}</text>`;
    }
  });
  return `<div class="chart">${svg}</svg></div>`;
}

/**
 * @param {StackedSeriesGroup[]} groups
 * @param {"cost" | "requests" | "tokens"} metricName
 * @param {boolean} [share]
 */
export function seriesLegend(groups, metricName, share = false) {
  const denominator = sum(groups.map((g) => g[metricName]));
  return `<div class="legend series-legend">${groups.map((g, i) => `<button data-series="${h(g.key)}" aria-pressed="${!app.hiddenSeries.has(g.key)}"><i class="dot" style="background:${g.color || colors[i % colors.length]}"></i>${h(g.label ?? g.key)} <span class="num">${share ? percent(g[metricName], denominator) : metricName === "cost" ? money(g[metricName]) : short(g[metricName])}</span></button>`).join("")}</div>`;
}

/**
 * @template {{ key: string, requests?: number | null, tokens?: number | null, cost?: number | null, buckets: Array<{ timestamp: number } & Partial<AggregateBucket>>, daily: Array<{ timestamp: number } & Partial<AggregateBucket>> }} T
 * @param {T[]} groups
 * @param {(g: T) => string} label
 * @returns {StackedSeriesGroup[]}
 */
export function topSeries(groups, label) {
  /** @type {StackedSeriesGroup[]} */
  const result = groups.slice(0, 6).map((g) => ({ ...g, label: label(g) }));
  if (groups.length > 6) {
    const rest = groups.slice(6);
    const buckets = new Map();
    const daily = new Map();
    for (const g of rest) {
      for (const [name, map] of /** @type {const} */ ([
        ["buckets", buckets],
        ["daily", daily],
      ])) {
        for (const b of g[name]) {
          const v = map.get(b.timestamp) || {
            timestamp: b.timestamp,
            requests: 0,
            tokens: 0,
            cost: 0,
          };
          for (const field of /** @type {const} */ (["requests", "tokens", "cost"])) {
            v[field] += Number(b[field] ?? 0);
          }
          map.set(b.timestamp, v);
        }
      }
    }
    result.push({
      key: "other",
      label: `Other (${rest.length})`,
      requests: sum(rest.map((g) => g.requests)),
      tokens: sum(rest.map((g) => g.tokens)),
      cost: sum(rest.map((g) => g.cost)),
      buckets: [...buckets.values()],
      daily: [...daily.values()],
    });
  }
  return result;
}

/**
 * @param {{ key: string, label?: string, description?: string, model?: string, provider?: string }} g
 * @param {string} kind
 * @param {string} cells
 * @param {(key: string) => string} detail
 * @param {number} columns
 * @returns {string}
 */
export function groupRow(g, kind, cells, detail, columns) {
  const key = groupKey(kind, g.key);
  const open = app.expandedGroups.has(key);
  const name = g.label ?? (kind === "provider" ? g.provider : g.model) ?? g.key;
  const description =
    g.description ?? (kind !== "provider" ? `${g.provider ?? ""} · ${g.key}` : "");
  return `<tr class="group-row${open ? " expanded" : ""}" tabindex="0" data-expand="${h(key)}" aria-expanded="${open}" aria-controls="details-${h(key)}" aria-label="${open ? "Collapse" : "Expand"} ${h(name)}"><td><div class="model-name">${icon(open ? "down" : "chevron", 12)} ${h(name)}</div>${description ? `<div class="provider insight-path" title="${h(description)}">${h(description)}</div>` : ""}</td>${cells}</tr><tr class="expanded-detail" id="details-${h(key)}"${open ? "" : " hidden"}><td colspan="${columns}">${open ? detail(key) : ""}</td></tr>`;
}

/**
 * @param {string[]} headers
 * @param {string} rows
 */
export function analyticsTable(headers, rows) {
  return `<div class="table-wrap"><table class="analytics-table"><thead><tr>${headers.map((name, i) => `<th${i ? ' class="right"' : ""}>${h(name)}</th>`).join("")}</tr></thead><tbody>${rows || `<tr><td colspan="${headers.length}"><div class="empty">No requests in this range</div></td></tr>`}</tbody></table></div>`;
}

/** @param {string[]} values */
export const numericCells = (values) =>
  values.map((v) => `<td class="right num">${v}</td>`).join("");

/**
 * Generates the explanatory text for the current range.
 * @returns {string}
 */
export const currentRangeDescription = () => descriptions[app.range];
