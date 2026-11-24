/**
 * Models 页面：模型请求占比、排序明细表、缓存节省计算与单模型吞吐/TTFT 详情。
 */

import {
  pivotSeries,
  resetToolCharts,
  TOOL_OTHER_COLOR,
  toolBucketAxis,
  toolBucketWord,
  toolCard,
  toolChart,
  toolDensify,
  toolLegend,
  toolRangeMeta,
  toolSeriesColors,
  toolSparkline,
  toolStat,
} from "./charts.js";
import {
  compact,
  errorRateText,
  estimatedCost,
  h,
  icon,
  number,
  rangeWindows,
  ratioPercent,
  relative,
} from "./format.js";
import { app, hooks } from "./state.js";
import { renderSortHeader, renderTableMore, sortRows, xEmpty, xMeter } from "./table.js";

/** @typedef {import("../../web-types/api.d.ts").AnalyticsResponse} AnalyticsResponse */
/** @typedef {import("../../web-types/api.d.ts").ModelAggregate} ModelAggregate */
/** @typedef {import("../../web-types/api.d.ts").SortState} SortState */

const MODEL_SHARE_LIMIT = 6;
const MODEL_LIMIT = 25;

/** @type {SortState} */
let modelSort = { key: "requests", dir: "desc" };
let modelShown = MODEL_LIMIT;
/** @type {string | null} */
let modelSelected = null;
/** @type {Set<string>} */
let modelDetailHidden = new Set();

/** @param {Pick<ModelAggregate, "model" | "provider"> & { key?: string }} group */
export const modelKeyOf = (group) => group.key || `${group.model}::${group.provider}`;

/** @param {Pick<ModelAggregate, "input_tokens" | "cached_tokens">} group */
const modelCacheRate = (group) =>
  (group.input_tokens ?? 0) > 0
    ? (group.cached_tokens ?? 0) / /** @type {number} */ (group.input_tokens)
    : 0;

/** @param {Pick<ModelAggregate, "requests" | "failed">} group */
const modelErrorRate = (group) =>
  (group.requests ?? 0) > 0 ? (group.failed ?? 0) / group.requests : 0;

/** @param {number | null | undefined} value */
const modelSpeed = (value) => (value == null ? "-" : value.toFixed(1));

/** @param {number | null | undefined} ms */
const modelDuration = (ms) =>
  ms == null ? "-" : ms < 1000 ? `${(ms / 1000).toFixed(2)}s` : `${(ms / 1000).toFixed(1)}s`;

/** @param {number} rate */
const modelErrorTone = (rate) => (rate >= 0.05 ? "bad" : rate >= 0.01 ? "warn" : "ok");

/**
 * 缓存节省需要能对账的写入用量与一致费率；任一缺失时返回 null。
 * @param {Pick<ModelAggregate, "rates" | "cost_components" | "requests" | "priced" | "cache_write_known" | "input_tokens">} group
 * @returns {number | null}
 */
export function modelCacheSavings(group) {
  const rates = group.rates;
  const components = group.cost_components;
  if (!rates || !(rates.input > 0) || !components) return null;
  if (!(group.requests > 0) || group.priced !== group.requests) return null;
  if (group.cache_write_known !== group.requests) return null;
  if (!(group.input_tokens && group.input_tokens > 0)) return null;
  const counterfactual = (group.input_tokens * rates.input) / 1e6;
  if (!(counterfactual > 0)) return null;
  const actual =
    (components.input ?? 0) + (components.cache_read ?? 0) + (components.cache_write ?? 0);
  return 1 - actual / counterfactual;
}

export function resetModels() {
  modelSort = { key: "requests", dir: "desc" };
  modelShown = MODEL_LIMIT;
  modelSelected = null;
  modelDetailHidden = new Set();
}

/**
 * 颜色按总请求数降序固定分配，排序与筛选不改变色带。
 * @param {ModelAggregate[]} groups
 * @returns {Map<string, string>}
 */
export function modelColors(groups) {
  const palette = toolSeriesColors();
  return new Map(
    [...groups]
      .sort(
        (a, b) =>
          (b.requests ?? 0) - (a.requests ?? 0) || modelKeyOf(a).localeCompare(modelKeyOf(b)),
      )
      .map((group, index) => [modelKeyOf(group), palette[index % palette.length]]),
  );
}

/**
 * 同一模型被多个 provider 提供时才附加 provider，避免图例重复。
 * @param {ModelAggregate[]} groups
 * @returns {Map<string, string>}
 */
export function modelLabels(groups) {
  /** @type {Map<string, number>} */
  const providersPerModel = new Map();
  /** @type {Map<string, number>} */
  const keysPerModelProvider = new Map();
  for (const group of groups) {
    providersPerModel.set(group.model, (providersPerModel.get(group.model) ?? 0) + 1);
    const pairKey = `${group.model}\0${group.provider}`;
    keysPerModelProvider.set(pairKey, (keysPerModelProvider.get(pairKey) ?? 0) + 1);
  }
  return new Map(
    groups.map((group) => {
      const pairKey = `${group.model}\0${group.provider}`;
      let label = group.model;
      if ((keysPerModelProvider.get(pairKey) ?? 0) > 1 && group.key) {
        label = `${group.model} · ${group.provider} (${group.key})`;
      } else if ((providersPerModel.get(group.model) ?? 0) > 1) {
        label = `${group.model} · ${group.provider}`;
      }
      return [modelKeyOf(group), label];
    }),
  );
}

/**
 * 前 N 个模型单列，其余折叠为 Other；阈值与图例顺序一致。
 * @param {Array<{ timestamp: number, key: string, value: number }>} points
 * @param {number[]} buckets
 * @param {Map<string, string>} colors
 * @param {Map<string, string>} labels
 * @param {number} limit
 */
export function modelPivot(points, buckets, colors, labels, limit) {
  return pivotSeries(points, buckets, limit, {
    keyOf: (point) => point.key,
    timestampOf: (point) => point.timestamp,
    valueOf: (point) => point.value,
    colorOf: (key) => colors.get(key) ?? TOOL_OTHER_COLOR,
    labelOf: (key) => labels.get(key) ?? key,
  });
}

function modelView() {
  const payload = /** @type {AnalyticsResponse} */ (app.data);
  const groups = [...payload.models].sort(
    (a, b) => (b.requests ?? 0) - (a.requests ?? 0) || modelKeyOf(a).localeCompare(modelKeyOf(b)),
  );
  const colors = modelColors(groups);
  const labels = modelLabels(groups);
  const buckets = toolBucketAxis(
    groups.flatMap((group) => (group.buckets || []).map((bucket) => bucket.timestamp)),
  );
  const points = groups.flatMap((group) =>
    (group.buckets || []).map((bucket) => ({
      timestamp: bucket.timestamp,
      key: modelKeyOf(group),
      value: bucket.requests ?? 0,
    })),
  );
  const requestTotals = toolDensify(
    points.map((point) => ({ timestamp: point.timestamp, value: point.value })),
    buckets,
  );
  // 零值转 null，tooltip 只列出该时间桶内实际活跃的模型。
  const countSeries = modelPivot(points, buckets, colors, labels, MODEL_SHARE_LIMIT).map(
    (series) => ({ ...series, values: series.values.map((value) => value || null) }),
  );
  const shareSeries = countSeries.map((series) => ({
    ...series,
    values: series.values.map((value, index) =>
      value === null || !(requestTotals[index] > 0) ? null : value / requestTotals[index],
    ),
  }));
  const trends = new Map(
    groups.map((group) => [
      modelKeyOf(group),
      toolDensify(
        (group.buckets || []).map((bucket) => ({
          timestamp: bucket.timestamp,
          value: bucket.requests ?? 0,
        })),
        buckets,
      ),
    ]),
  );
  /** @type {ModelAggregate | undefined} */
  let top;
  let totalRequests = 0;
  let failedRequests = 0;
  let totalCost = 0;
  let unpricedRequests = 0;
  for (const group of groups) {
    if (!top || (group.requests ?? 0) > (top.requests ?? 0)) top = group;
    totalRequests += group.requests ?? 0;
    failedRequests += group.failed ?? 0;
    totalCost += group.cost ?? 0;
    unpricedRequests += group.unpriced ?? 0;
  }
  return {
    groups,
    colors,
    labels,
    buckets,
    requestTotals,
    countSeries,
    shareSeries,
    trends,
    top,
    totalRequests,
    failedRequests,
    totalCost,
    unpricedRequests,
    maxRequests: top?.requests ?? 0,
    providerCount: new Set(groups.map((group) => group.provider)).size,
  };
}

/** @param {Array<[string, string, string]>} items */
function modelKeyValues(items) {
  return `<dl class="x-kv" style="margin:0">${items
    .map(
      ([key, label, value]) =>
        `<div class="x-kv-item" data-key="${h(key)}"><dt class="x-kv-key">${h(label)}</dt><dd class="x-kv-value" style="margin:0">${value}</dd></div>`,
    )
    .join("")}</dl>`;
}

/** @param {ModelAggregate} group */
function modelErrorsCell(group) {
  if (!(group.failed > 0)) return '<span class="x-num x-dim">0%</span>';
  const rate = modelErrorRate(group);
  return `<span title="${h(`${number(group.failed)} failed`)}"><span class="x-badge" data-tone="${modelErrorTone(rate)}" data-mono="true">${errorRateText(rate)}</span></span>`;
}

/**
 * @param {ReturnType<typeof modelView>} view
 * @returns {import("./table.js").TableColumn<ModelAggregate>[]}
 */
function modelColumns(view) {
  const bucketWord = toolBucketWord(toolRangeMeta().bucketMs);
  return [
    {
      key: "expand",
      header: "",
      width: 28,
      render: (group) =>
        `<span class="x-models-chevron" data-open="${modelKeyOf(group) === modelSelected}">${icon("chevron", 14)}</span>`,
    },
    {
      key: "model",
      header: "Model",
      sort: (group) => group.model,
      render: (group) => {
        const key = modelKeyOf(group);
        return `<div class="x-row" style="gap:10px"><span class="x-swatch" style="background:${view.colors.get(key) || TOOL_OTHER_COLOR}"></span><div class="x-stack" style="gap:0"><span class="x-cell-primary x-truncate"><span class="x-mono">${h(group.model)}</span></span><span class="x-cell-secondary x-truncate">${h(group.provider)}</span></div></div>`;
      },
    },
    {
      key: "requests",
      header: "Requests",
      align: "right",
      sort: (group) => group.requests ?? 0,
      render: (group) =>
        xMeter(
          group.requests ?? 0,
          view.maxRequests,
          number(group.requests),
          view.colors.get(modelKeyOf(group)) || TOOL_OTHER_COLOR,
        ),
    },
    {
      key: "cost",
      header: "Cost",
      title: "API-equivalent estimate at public rates",
      align: "right",
      sort: (group) => group.cost ?? -1,
      render: (group) =>
        `<span class="x-num">${estimatedCost(group.cost ?? 0, group.unpriced ?? 0)}</span>`,
    },
    {
      key: "tokens",
      header: "Tokens",
      title: "Uncached input + cache reads + cache writes + output",
      align: "right",
      sort: (group) => group.tokens ?? 0,
      render: (group) =>
        `<span class="x-num" title="${h(number(group.tokens))}">${compact(group.tokens)}</span>`,
    },
    {
      key: "cache",
      header: "Cache rate",
      title: "Cache reads ÷ (uncached input + cache reads)",
      align: "right",
      sort: (group) => modelCacheRate(group),
      render: (group) => `<span class="x-num">${ratioPercent(modelCacheRate(group))}</span>`,
    },
    {
      key: "errors",
      header: "Errors",
      align: "right",
      sort: (group) => modelErrorRate(group),
      render: (group) => modelErrorsCell(group),
    },
    {
      key: "tps",
      header: "Tokens/s",
      title: "Average output tokens per second",
      align: "right",
      sort: (group) => group.tokens_per_second ?? -1,
      render: (group) => `<span class="x-num">${modelSpeed(group.tokens_per_second)}</span>`,
    },
    {
      key: "ttft",
      header: "TTFT",
      title: "Average time to first token",
      align: "right",
      sort: (group) => group.avg_ttft_ms ?? -1,
      render: (group) => `<span class="x-num">${modelDuration(group.avg_ttft_ms)}</span>`,
    },
    {
      key: "trend",
      header: "Trend",
      title: `Requests per ${bucketWord}`,
      width: 112,
      render: (group) => {
        const key = modelKeyOf(group);
        const values = view.trends.get(key);
        return values
          ? `<span style="display:inline-block;width:96px;height:22px">${toolSparkline(values, 96, 22, view.colors.get(key) || TOOL_OTHER_COLOR)}</span>`
          : '<span class="x-dim">–</span>';
      },
    },
  ];
}

/**
 * @param {ReturnType<typeof modelView>} view
 * @param {ModelAggregate} group
 */
function modelDetail(view, group) {
  const key = modelKeyOf(group);
  const color = view.colors.get(key) || "var(--chart-primary)";
  const points = (group.buckets || []).map((bucket) => ({
    timestamp: bucket.timestamp,
    tps: bucket.tokens_per_second ?? null,
    ttft: bucket.avg_ttft_ms == null ? null : bucket.avg_ttft_ms / 1000,
    requests: bucket.requests ?? 0,
  }));
  const line = points.length > 1 ? "line" : "bars";
  /** @type {import("./charts.js").ChartSeries[]} */
  const series = [
    {
      key: "tps",
      label: "Tokens/s",
      color,
      kind: line,
      values: points.map((point) => point.tps),
    },
    {
      key: "ttft",
      label: "TTFT",
      color: TOOL_OTHER_COLOR,
      kind: line,
      axis: "right",
      dashed: true,
      values: points.map((point) => point.ttft),
    },
  ];
  const errorRate = modelErrorRate(group);
  const savings = modelCacheSavings(group);
  const chart = points.length
    ? toolChart(
        { buckets: points.map((point) => point.timestamp), bucketMs: toolRangeMeta().bucketMs },
        series,
        "No data in this range",
        {
          height: 240,
          stacked: false,
          hidden: modelDetailHidden,
          showTotal: false,
          format: compact,
          formatTooltip: (value) => `${modelSpeed(value)} tok/s`,
          formatRight: (value) => `${Number(value.toFixed(2))}s`,
          extraRows: (index) =>
            `<div class="x-chart-tooltip-row x-chart-tooltip-total"><span class="x-chart-tooltip-label">Requests</span><span class="x-chart-tooltip-value">${number(points[index].requests)}</span></div>`,
        },
      )
    : xEmpty("No performance samples", "Timing is recorded for streamed responses.");
  const firstSeen = group.first_seen == null ? "—" : relative(group.first_seen);
  const lastSeen = group.last_seen == null ? "—" : relative(group.last_seen);
  return `<div class="x-models-detail x-rise"><div class="x-models-detail-facts"><div><div class="x-section-label">Efficiency</div>${modelKeyValues(
    [
      [
        "error",
        "Error rate",
        `<span class="x-tone-${modelErrorTone(errorRate)}">${errorRateText(errorRate)} <span class="x-dim">(${number(group.failed ?? 0)} failed)</span></span>`,
      ],
      ["cache", "Cache rate", ratioPercent(modelCacheRate(group))],
      [
        "savings",
        "Cache savings",
        savings == null
          ? '<span class="x-dim" title="Not recorded: cache savings need reported cache-write usage, consistent recorded rates and priced requests">—</span>'
          : `<span${savings < 0 ? ' style="color:var(--bad)"' : ""}>${ratioPercent(savings)}</span>`,
      ],
      [
        "premium",
        "Premium requests",
        '<span class="x-dim" title="Premium request units are not recorded by pp">—</span>',
      ],
    ],
  )}</div><div><div class="x-section-label">Latency</div>${modelKeyValues([
    ["duration", "Avg duration", modelDuration(group.avg_duration_ms)],
    ["ttft", "Avg TTFT", modelDuration(group.avg_ttft_ms)],
    ["speed", "Tokens/s", modelSpeed(group.tokens_per_second)],
  ])}</div><div><div class="x-section-label">Tokens</div>${modelKeyValues([
    ["uncached", "Uncached input", compact(group.uncached_tokens)],
    ["cache-read", "Cache read", compact(group.cached_tokens)],
    ["cache-write", "Cache write", compact(group.cache_write_tokens)],
    ["output", "Output", compact(group.output_tokens)],
  ])}</div><div class="x-micro x-dim">First seen ${h(firstSeen)} · last seen ${h(lastSeen)}</div></div><div class="x-models-detail-chart"><div class="x-row" style="justify-content:space-between"><div class="x-section-label" style="margin-bottom:0">Performance per active ${h(toolBucketWord(toolRangeMeta().bucketMs))}</div>${toolLegend(series, undefined, { hidden: modelDetailHidden, attribute: "data-model-detail-series", values: false, focusAttribute: "data-models-focus" })}</div>${chart}</div></div>`;
}

/** @param {ReturnType<typeof modelView>} view */
function modelTable(view) {
  const columns = modelColumns(view);
  const rows = sortRows(view.groups, columns, modelSort);
  const visible = rows.slice(0, modelShown);
  const head = columns
    .map((column) =>
      renderSortHeader(column, modelSort, {
        sortAttr: "data-model-sort",
        alignAttr: "data-model-align",
        focusAttr: "data-models-focus",
      }),
    )
    .join("");
  const body = visible
    .map((group) => {
      const key = modelKeyOf(group);
      const encoded = encodeURIComponent(key);
      const open = key === modelSelected;
      const cells = columns
        .map(
          (column) =>
            `<td data-align="${column.align || "left"}" data-wrap="false">${column.render(group)}</td>`,
        )
        .join("");
      return `<tr data-clickable="true" data-selected="${open}" data-model-row="${h(encoded)}" data-models-focus="row-${h(encoded)}" tabindex="0">${cells}</tr>${
        open
          ? `<tr><td colspan="${columns.length}" data-wrap="true" style="padding:0">${modelDetail(view, group)}</td></tr>`
          : ""
      }`;
    })
    .join("");
  return `<div class="x-table-wrap"><table class="x-table" data-dense="false"><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>${renderTableMore(
    {
      total: rows.length,
      shown: visible.length,
      step: MODEL_LIMIT,
      moreAttr: 'data-model-page="more"',
      lessAttr: 'data-model-page="less"',
      focusAttr: "data-models-focus",
      focusMore: "more",
      focusLess: "less",
    },
  )}`;
}

function modelsEmpty() {
  return xEmpty("No model usage in this range", "Pick a wider range in the top bar.");
}

/**
 * 处理 Models 页内图例、排序、展开与分页点击。
 * @param {MouseEvent} e
 * @returns {boolean}
 */
export function modelsClick(e) {
  if (!(e.target instanceof Element)) return false;
  const share = e.target.closest("[data-share]");
  if (share instanceof HTMLElement) {
    app.shareMode = share.dataset.share === "requests" ? "requests" : "share";
    hooks.render();
    return true;
  }
  const header = e.target.closest("[data-model-sort]");
  if (header instanceof HTMLElement) {
    const key = header.dataset.modelSort ?? "";
    modelSort = {
      key,
      dir:
        modelSort.key === key
          ? modelSort.dir === "asc"
            ? "desc"
            : "asc"
          : header.dataset.modelAlign === "right"
            ? "desc"
            : "asc",
    };
    hooks.render();
    return true;
  }
  const series = e.target.closest("[data-model-detail-series]");
  if (series instanceof HTMLElement) {
    const key = series.dataset.modelDetailSeries ?? "";
    if (modelDetailHidden.has(key)) modelDetailHidden.delete(key);
    else modelDetailHidden.add(key);
    hooks.render();
    return true;
  }
  const row = e.target.closest("[data-model-row]");
  if (row instanceof HTMLElement) {
    const key = decodeURIComponent(row.dataset.modelRow ?? "");
    modelSelected = modelSelected === key ? null : key;
    modelDetailHidden = new Set();
    hooks.render();
    return true;
  }
  const pageButton = e.target.closest("[data-model-page]");
  if (pageButton instanceof HTMLElement) {
    modelShown =
      pageButton.dataset.modelPage === "more" ? modelShown + MODEL_LIMIT * 4 : MODEL_LIMIT;
    hooks.render();
    return true;
  }
  return false;
}

/**
 * 渲染 Models 页面。
 * @returns {string}
 */
export function modelsPage() {
  if (!app.data) return '<div class="loading">Loading statistics…</div>';
  resetToolCharts();
  const view = modelView();
  const bucketWord = toolBucketWord(toolRangeMeta().bucketMs);
  const empty = modelsEmpty();
  const share = app.shareMode === "share";
  const totals = Math.max(1, view.totalRequests);
  const statGrid = `<div class="x-stat-grid x-rise" style="--stat-min:200px">${toolStat(
    "Models used",
    number(view.groups.length),
    `across ${number(view.providerCount)} provider${view.providerCount === 1 ? "" : "s"}`,
  )}${toolStat(
    "Most used",
    view.top ? view.top.model : "–",
    view.top
      ? `${ratioPercent(view.top.requests / totals)} of requests · ${view.top.provider}`
      : undefined,
    { title: view.top ? `${view.top.model} (${view.top.provider})` : undefined },
  )}${toolStat("Requests", number(view.totalRequests), `${number(view.failedRequests)} failed`, {
    spark: view.requestTotals,
  })}${toolStat(
    "API-equivalent cost",
    estimatedCost(view.totalCost, view.unpricedRequests),
    view.unpricedRequests > 0 ? `${number(view.unpricedRequests)} unpriced` : undefined,
    { title: "What this usage would cost at public API rates" },
  )}</div>`;
  const actions = `<div class="x-segmented" data-size="sm" role="radiogroup" aria-label="Share chart mode"><span class="x-segmented-thumb"></span>${[
    ["share", "Share"],
    ["requests", "Requests"],
  ]
    .map(
      ([value, label]) =>
        `<button type="button" class="x-segmented-option" role="radio" aria-checked="${app.shareMode === value}" data-active="${app.shareMode === value}" data-share="${value}" data-models-focus="share-${value}">${label}</button>`,
    )
    .join("")}</div>`;
  const chart = toolChart(
    view,
    share ? view.shareSeries : view.countSeries,
    "No model activity in this range",
    {
      height: 260,
      yMax: share ? 1 : undefined,
      format: share ? (value) => ratioPercent(value, 0) : compact,
      formatTooltip: share ? (value) => ratioPercent(value) : number,
      showTotal: !share,
    },
  );
  const shareBody = `<div class="x-stack" style="gap:12px">${chart}${toolLegend(
    view.countSeries,
    (value) => ratioPercent(value / totals),
    { focusAttribute: "data-models-focus" },
  )}</div>`;
  return `<div class="x-page"><header class="x-page-header x-rise"><div><h1 class="x-page-title">Models</h1><p class="x-page-description">Which models did the work in ${h(rangeWindows[app.range])}, and how fast they answered.</p></div></header>${view.groups.length ? statGrid : empty}${toolCard(
    1,
    "Request share",
    share
      ? `Each model's share of requests per ${bucketWord}`
      : `Requests per ${bucketWord}, stacked by model`,
    actions,
    view.groups.length ? shareBody : empty,
  )}${toolCard(2, "All models", "Click a row for latency and throughput over time", "", view.groups.length ? modelTable(view) : empty, true)}</div>`;
}
