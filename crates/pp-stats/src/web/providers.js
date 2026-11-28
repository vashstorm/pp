/**
 * Providers page: per-provider aggregates, expandable token breakdown, stacked burn chart and local
 * hourly peaks.
 */

import { averageSpeed, fillBuckets } from "./analytics-shared.js";
import {
  pivotSeries,
  resetToolCharts,
  TOOL_OTHER_COLOR,
  toolCard,
  toolChart,
  toolLegend,
  toolSeriesColors,
  toolStat,
} from "./charts.js";
import {
  $,
  errorRateText,
  estimatedCost,
  h,
  money,
  number,
  pad2,
  rangeWindows,
  ratioPercent,
  short,
  sum,
} from "./format.js";
import { app, hooks } from "./state.js";
import { renderSortHeader, renderTableMore, sortRows, xEmpty, xMeter } from "./table.js";

/** @typedef {import("../../web-types/api.d.ts").AnalyticsResponse} AnalyticsResponse */
/** @typedef {import("../../web-types/api.d.ts").ProviderAggregate} ProviderAggregate */
/** @typedef {import("../../web-types/api.d.ts").SortState} SortState */

const PROVIDER_TOP = 6;
const PROVIDER_LIMIT = 12;

/** @type {Array<[keyof ProviderAggregate, string, string]>} */
const PROVIDER_MIX = [
  ["uncached_tokens", "Uncached input", "#5b8cff"],
  ["cached_tokens", "Cache read", "var(--chart-primary)"],
  ["cache_write_tokens", "Cache write", "#f5b54a"],
  ["output_tokens", "Output", "var(--chart-secondary)"],
];

/** @type {Array<[keyof ProviderAggregate, string, string]>} */
const PROVIDER_MIX_MODERN = [
  ["uncached_tokens", "Uncached input", "var(--chart-primary)"],
  ["cached_tokens", "Cache read", "var(--ok)"],
  ["cache_write_tokens", "Cache write", "var(--chart-secondary)"],
  ["output_tokens", "Output", "var(--pink)"],
];

/** @type {"tokens" | "cost" | "requests"} */
let providerMetric = "tokens";
let providerPeak = "";
/** @type {SortState} */
let providerSort = { key: "tokens", dir: "desc" };
/** @type {string | null} */
let providerSelected = null;
let providerShown = PROVIDER_LIMIT;

export function resetProviders() {
  providerPeak = "";
  providerMetric = "tokens";
  providerSort = { key: "tokens", dir: "desc" };
  providerSelected = null;
  providerShown = PROVIDER_LIMIT;
}

const providerMixSeries = () => (app.style === "modern" ? PROVIDER_MIX_MODERN : PROVIDER_MIX);
const providerBucketMs = () =>
  app.range === "1h" ? 300000 : app.range === "24h" ? 3600000 : 86400000;
const providerBucketUnit = () =>
  app.range === "1h" ? "5 minutes" : app.range === "24h" ? "hour" : "day";

/**
 * Colors are assigned by descending total tokens, so sorting and filtering never change the color
 * band.
 * @param {ProviderAggregate[]} groups
 * @returns {Map<string, string>}
 */
function providerColors(groups) {
  const palette = toolSeriesColors();
  return new Map(
    [...groups]
      .sort((a, b) => (b.tokens ?? 0) - (a.tokens ?? 0) || a.provider.localeCompare(b.provider))
      .map((group, index) => [group.provider, palette[index % palette.length]]),
  );
}

/** @param {"requests" | "tokens" | "cost"} field */
function providerSpark(field) {
  const payload = /** @type {AnalyticsResponse} */ (app.data);
  return fillBuckets(payload.buckets).map((bucket) => Number(bucket[field] ?? 0));
}

/** @param {ProviderAggregate[]} groups */
function providerStatGrid(groups) {
  const payload = /** @type {AnalyticsResponse} */ (app.data);
  const s = payload.summary;
  const unpriced = s.unpriced ?? 0;
  /** @type {ProviderAggregate | null} */
  const top = groups.reduce(
    (/** @type {ProviderAggregate | null} */ best, group) =>
      best === null || (group.tokens ?? 0) > (best.tokens ?? 0) ? group : best,
    null,
  );
  return `<div class="x-stat-grid x-rise" style="--stat-min:190px">${toolStat(
    "Providers",
    number(groups.length),
    top ? `Most tokens: ${top.provider}` : undefined,
  )}${toolStat("Requests", number(s.requests), `${number(s.failed)} failed`, {
    spark: providerSpark("requests"),
  })}${toolStat("Tokens", short(s.tokens), undefined, {
    title: "Uncached input + cache reads + cache writes + output",
    spark: providerSpark("tokens"),
  })}${toolStat(
    "API-equivalent cost",
    estimatedCost(s.cost, unpriced),
    unpriced > 0 ? `${number(unpriced)} unpriced` : undefined,
    {
      title: "What this usage would cost at public API rates",
      spark: providerSpark("cost"),
      sparkColor: "var(--chart-secondary)",
    },
  )}${toolStat(
    "Error rate",
    errorRateText(s.requests ? s.failed / s.requests : 0),
    `${number(s.requests - s.failed)} succeeded`,
  )}</div>`;
}

/** @param {ProviderAggregate} group */
function providerErrorCell(group) {
  const rate = group.requests > 0 ? (group.failed ?? 0) / group.requests : 0;
  const tone =
    (group.failed ?? 0) > 0 ? (rate >= 0.05 ? "bad" : rate >= 0.01 ? "warn" : "ok") : "neutral";
  return `<span class="x-row" style="gap:8px;justify-content:flex-end"><span class="x-num x-dim">${number(group.failed)}</span><span class="x-badge" data-tone="${tone}" data-mono="true">${errorRateText(rate)}</span></span>`;
}

/**
 * @param {ProviderAggregate[]} groups
 * @param {Map<string, string>} colors
 * @returns {import("./table.js").TableColumn<ProviderAggregate>[]}
 */
function providerColumns(groups, colors) {
  const maxRequests = Math.max(0, ...groups.map((group) => group.requests ?? 0));
  const maxTokens = Math.max(0, ...groups.map((group) => group.tokens ?? 0));
  const totalRequests = sum(groups.map((group) => group.requests)) ?? 0;
  const totalTokens = sum(groups.map((group) => group.tokens)) ?? 0;
  return [
    {
      key: "provider",
      header: "Provider",
      sort: (group) => group.provider,
      render: (group) =>
        `<span class="x-row" style="gap:8px"><span class="x-swatch" style="background:${colors.get(group.provider) ?? TOOL_OTHER_COLOR}"></span><span class="x-mono x-cell-primary">${h(group.provider)}</span></span>`,
    },
    {
      key: "requests",
      header: "Requests",
      align: "right",
      sort: (group) => group.requests ?? 0,
      render: (group) =>
        `<span title="${ratioPercent(totalRequests > 0 ? (group.requests ?? 0) / totalRequests : 0)} of all requests">${xMeter(group.requests ?? 0, maxRequests, number(group.requests))}</span>`,
    },
    {
      key: "errors",
      header: "Error rate",
      align: "right",
      sort: (group) => (group.requests > 0 ? (group.failed ?? 0) / group.requests : 0),
      render: (group) => providerErrorCell(group),
    },
    {
      key: "models",
      header: "Models",
      title: "Distinct models used through this provider",
      align: "right",
      sort: (group) => group.models ?? 0,
      render: (group) => `<span class="x-num">${number(group.models)}</span>`,
    },
    {
      key: "tokens",
      header: "Tokens",
      title: "Uncached input + cache reads + cache writes + output",
      align: "right",
      sort: (group) => group.tokens ?? 0,
      render: (group) =>
        xMeter(
          group.tokens ?? 0,
          maxTokens,
          short(group.tokens),
          colors.get(group.provider) ?? TOOL_OTHER_COLOR,
        ),
    },
    {
      key: "share",
      header: "Share",
      title: "Fraction of all tokens in range",
      align: "right",
      sort: (group) => group.tokens ?? 0,
      render: (group) =>
        `<span class="x-num x-muted">${ratioPercent(totalTokens > 0 ? (group.tokens ?? 0) / totalTokens : 0)}</span>`,
    },
    {
      key: "cost",
      header: "Cost",
      title: "API-equivalent estimate",
      align: "right",
      sort: (group) => group.cost ?? -1,
      render: (group) =>
        `<span class="x-num"${(group.unpriced ?? 0) > 0 ? ` title="${number(group.unpriced)} unpriced"` : ""}>${estimatedCost(group.cost, group.unpriced ?? 0)}</span>`,
    },
    {
      key: "tps",
      header: "Tokens/s",
      title: "Average output throughput",
      align: "right",
      sort: (group) => group.tokens_per_second ?? -1,
      render: (group) => `<span class="x-num">${averageSpeed(group.tokens_per_second)}</span>`,
    },
    {
      // pp does not collect subscription multiplier units, so the missing value is not zero-filled.
      key: "premium",
      header: "Premium",
      title: "Premium requests (subscription multiplier units)",
      align: "right",
      sort: () => 0,
      render: () =>
        '<span class="x-num x-dim" title="Premium request units are not recorded by pp">—</span>',
    },
  ];
}

/** @param {ProviderAggregate} group */
function providerMix(group) {
  const mix = providerMixSeries();
  const known = sum(mix.map(([key]) => Number(group[key] ?? 0))) ?? 0;
  const segments = mix
    .filter(([key]) => Number(group[key] ?? 0) > 0)
    .map(
      ([key, label, color]) =>
        `<div class="x-share-bar-seg" style="flex-grow:${known > 0 ? Number(group[key] ?? 0) / known : 0};background:${color}" title="${h(label)}: ${known > 0 ? ((100 * Number(group[key] ?? 0)) / known).toFixed(1) : "0.0"}%"></div>`,
    )
    .join("");
  const items = mix
    .map(
      ([key, label, color]) =>
        `<span class="x-legend-item" title="${h(label)}"><span class="x-swatch" style="background:${color}"></span><span class="x-truncate">${h(label)}</span><span class="x-legend-value">${short(Number(group[key] ?? 0))} · ${(group.tokens ?? 0) > 0 ? ratioPercent(Number(group[key] ?? 0) / /** @type {number} */ (group.tokens), 0) : "–"}</span></span>`,
    )
    .join("");
  return `<div class="x-providers-mix"><div class="x-share-bar" style="height:8px" role="img">${segments}</div><div class="x-legend">${items}</div></div>`;
}

/**
 * @param {ProviderAggregate[]} groups
 * @param {Map<string, string>} colors
 */
function providerTable(groups, colors) {
  if (!groups.length) return xEmpty("No requests in this range");
  const columns = providerColumns(groups, colors);
  const rows = sortRows(groups, columns, providerSort);
  const shown = rows.slice(0, providerShown);
  const head = columns
    .map((column) =>
      renderSortHeader(column, providerSort, {
        sortAttr: "data-provider-sort",
        alignAttr: "data-provider-align",
        focusAttr: "data-provider-focus",
      }),
    )
    .join("");
  const body = shown
    .map((group) => {
      const open = group.provider === providerSelected;
      const cells = columns
        .map(
          (column) =>
            `<td data-align="${column.align || "left"}" data-wrap="false">${column.render(group)}</td>`,
        )
        .join("");
      const detail = open
        ? `<tr><td colspan="${columns.length}" data-wrap="true" style="padding:0">${providerMix(group)}</td></tr>`
        : "";
      return `<tr data-clickable="true" data-selected="${open}" data-provider-row="${h(group.provider)}" data-provider-focus="row-${h(group.provider)}" tabindex="0">${cells}</tr>${detail}`;
    })
    .join("");
  return `<div class="x-table-wrap"><table class="x-table" data-dense="false"><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>${renderTableMore(
    {
      total: rows.length,
      shown: shown.length,
      step: PROVIDER_LIMIT,
      moreAttr: 'data-providerpage="more"',
      lessAttr: 'data-providerpage="less"',
      focusAttr: "data-provider-focus",
      focusMore: "more",
      focusLess: "less",
    },
  )}`;
}

function providerMetricSegmented() {
  return `<div class="x-segmented" data-size="sm" role="radiogroup" aria-label="Provider burn metric"><span class="x-segmented-thumb"></span>${[
    ["tokens", "Tokens"],
    ["cost", "Cost", "API-equivalent estimate"],
    ["requests", "Requests"],
  ]
    .map(
      ([value, label, title]) =>
        `<button type="button" class="x-segmented-option" role="radio" aria-checked="${providerMetric === value}" data-active="${providerMetric === value}"${title ? ` title="${h(title)}"` : ""} data-provider-metric="${value}" data-provider-focus="metric-${value}">${h(label)}</button>`,
    )
    .join("")}</div>`;
}

/**
 * @param {ProviderAggregate[]} groups
 * @param {Map<string, string>} colors
 */
function providerBurn(groups, colors) {
  const payload = /** @type {AnalyticsResponse} */ (app.data);
  const buckets = fillBuckets(payload.buckets).map((bucket) => bucket.timestamp);
  const points = groups.flatMap((group) =>
    group.buckets.map((bucket) => ({
      timestamp: bucket.timestamp,
      provider: group.provider,
      value: Number(bucket[providerMetric] ?? 0),
    })),
  );
  const palette = toolSeriesColors();
  const series = pivotSeries(points, buckets, PROVIDER_TOP, {
    keyOf: (point) => point.provider,
    timestampOf: (point) => point.timestamp,
    valueOf: (point) => point.value,
    colorOf: (key, rank) => colors.get(key) ?? palette[rank % palette.length],
  }).map((item) => ({ ...item, values: item.values.map((value) => value || null) }));
  return { buckets, series };
}

/**
 * @param {ProviderAggregate[]} groups
 * @param {Map<string, string>} colors
 */
function providerBurnCard(groups, colors) {
  const payload = /** @type {AnalyticsResponse} */ (app.data);
  const burn = providerBurn(groups, colors);
  const cost = providerMetric === "cost";
  const unpriced = payload.summary.unpriced ?? 0;
  const format = cost ? money : short;
  const description =
    cost && unpriced > 0
      ? `API-equivalent estimate per ${providerBucketUnit()}; excludes unpriced subscription requests`
      : `Per ${providerBucketUnit()}, top ${PROVIDER_TOP} providers stacked`;
  const chart = toolChart(
    { buckets: burn.buckets, bucketMs: providerBucketMs() },
    burn.series,
    "No provider activity in this range",
    { format, formatTooltip: cost ? money : number },
  );
  return toolCard(
    2,
    "Burn by provider",
    description,
    providerMetricSegmented(),
    `<div class="x-stack" style="gap:12px">${chart}${toolLegend(burn.series, format)}</div>`,
  );
}

/** @param {ProviderAggregate[]} groups */
function providerPeakHours(groups) {
  const selected = groups.some((group) => group.provider === providerPeak) ? providerPeak : "";
  const tokens = Array.from({ length: 24 }, () => 0);
  const output = Array.from({ length: 24 }, () => 0);
  const requests = Array.from({ length: 24 }, () => 0);
  for (const group of groups) {
    if (selected && group.provider !== selected) continue;
    for (const bucket of group.hours ?? []) {
      const hour = bucket.hour;
      if (!(hour >= 0 && hour < 24)) continue;
      tokens[hour] += bucket.tokens ?? 0;
      output[hour] += bucket.output_tokens ?? 0;
      requests[hour] += bucket.requests ?? 0;
    }
  }
  let peak = 0;
  for (let hour = 1; hour < 24; hour++) if (tokens[hour] > tokens[peak]) peak = hour;
  return { selected, tokens, output, requests, peak, hasData: tokens[peak] > 0 };
}

/** @param {ProviderAggregate[]} groups */
function providerPeakCard(groups) {
  const peak = providerPeakHours(groups);
  const series = [
    {
      key: "tokens",
      label: "Tokens",
      color: "var(--chart-primary)",
      values: peak.tokens.map((value, hour) => (peak.hasData && hour === peak.peak ? null : value)),
    },
    {
      key: "peak",
      label: "Tokens (peak hour)",
      color: "var(--chart-secondary)",
      values: peak.tokens.map((value, hour) => (peak.hasData && hour === peak.peak ? value : null)),
    },
  ];
  const chart = toolChart(
    { buckets: Array.from({ length: 24 }, (_, hour) => hour) },
    series,
    "No activity in this range",
    {
      format: short,
      formatTooltip: number,
      showTotal: false,
      tickLabel: (hour) => pad2(hour),
      tooltipTitle: (hour) => `${pad2(hour)}:00–${pad2((hour + 1) % 24)}:00`,
      extraRows: (hour) =>
        `<div class="x-chart-tooltip-row"><span class="x-chart-tooltip-label">Output</span><span class="x-chart-tooltip-value">${number(peak.output[hour])}</span></div><div class="x-chart-tooltip-row"><span class="x-chart-tooltip-label">Requests</span><span class="x-chart-tooltip-value">${number(peak.requests[hour])}</span></div>`,
    },
  );
  const select = `<select class="x-input x-providers-select" id="provider-peak" aria-label="Provider"><option value="">All providers</option>${groups.map((group) => `<option value="${h(group.provider)}"${group.provider === peak.selected ? " selected" : ""}>${h(group.provider)}</option>`).join("")}</select>`;
  return toolCard(
    3,
    "Peak burn hours",
    peak.hasData
      ? `Tokens by local hour of day; peak at ${pad2(peak.peak)}:00`
      : "Tokens by local hour of day",
    select,
    chart,
  );
}

/**
 * Renders the Providers page.
 * @returns {string}
 */
export function providersPage() {
  const payload = /** @type {AnalyticsResponse | null} */ (app.data);
  if (!payload) return '<div class="loading">Loading statistics…</div>';
  resetToolCharts();
  const groups = [...payload.providers].sort(
    (a, b) => (b.tokens ?? 0) - (a.tokens ?? 0) || a.provider.localeCompare(b.provider),
  );
  const colors = providerColors(groups);
  const unpriced = payload.summary.unpriced ?? 0;
  const description =
    unpriced > 0
      ? `Cost is an API-equivalent estimate and excludes ${number(unpriced)} unpriced subscription request${unpriced === 1 ? "" : "s"}. Select a row for its token mix.`
      : "Cost is an API-equivalent estimate. Select a row for its token mix.";
  return `<div class="x-page"><header class="x-page-header x-rise"><div><h1 class="x-page-title">Providers</h1><p class="x-page-description">Burn, reliability and subscription headroom per provider over ${h(rangeWindows[app.range])}.</p></div></header>${providerStatGrid(groups)}${toolCard(1, "Provider totals", description, "", providerTable(groups, colors), true)}<div class="x-grid x-grid-main-side">${providerBurnCard(groups, colors)}${providerPeakCard(groups)}</div></div>`;
}

/**
 * Handles interaction clicks on the Providers page.
 * @param {MouseEvent} e
 * @returns {boolean}
 */
export function providersClick(e) {
  if (!(e.target instanceof Element)) return false;
  const metricEl = e.target.closest("[data-provider-metric]");
  if (metricEl instanceof HTMLElement) {
    providerMetric = /** @type {"tokens" | "cost" | "requests"} */ (
      metricEl.dataset.providerMetric || "tokens"
    );
    hooks.render();
    return true;
  }
  const header = e.target.closest("[data-provider-sort]");
  if (header instanceof HTMLElement) {
    const key = header.dataset.providerSort ?? "";
    providerSort = {
      key,
      dir:
        providerSort.key === key
          ? providerSort.dir === "asc"
            ? "desc"
            : "asc"
          : header.dataset.providerAlign === "right"
            ? "desc"
            : "asc",
    };
    hooks.render();
    return true;
  }
  const row = e.target.closest("[data-provider-row]");
  if (row instanceof HTMLElement) {
    const key = row.dataset.providerRow ?? null;
    providerSelected = providerSelected === key ? null : key;
    hooks.render();
    return true;
  }
  const pageButton = e.target.closest("[data-providerpage]");
  if (pageButton instanceof HTMLElement) {
    providerShown =
      pageButton.dataset.providerpage === "more"
        ? providerShown + PROVIDER_LIMIT * 4
        : PROVIDER_LIMIT;
    hooks.render();
    return true;
  }
  return false;
}

/**
 * Handles changes of the Providers peak-hour filter dropdown.
 * @param {Event} e
 * @returns {boolean}
 */
export function providersChange(e) {
  if (!(e.target instanceof HTMLSelectElement) || e.target.id !== "provider-peak") return false;
  providerPeak = e.target.value;
  hooks.render();
  $("provider-peak")?.focus({ preventScroll: true });
  return true;
}
