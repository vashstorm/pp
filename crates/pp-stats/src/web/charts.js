/**
 * 共享图表、指标卡片、图例、时间轴分桶与 SVG 渲染原语（Tools / Hooks / Models / Providers / Errors 共用）。
 */

import { $, compact, fixedCost, h, number, pad2, TOOL_RANGES } from "./format.js";
import { app } from "./state.js";

/** @typedef {import("../../web-types/api.d.ts").StatsRange} StatsRange */

export const TOOL_SERIES_COLORS = [
  "#5ad8e6",
  "#ed4abf",
  "#9d7bff",
  "#f5b54a",
  "#4ade80",
  "#5b8cff",
  "#ff7a59",
  "#2dd4bf",
  "#c3e94f",
  "#fb7185",
];

export const TOOL_MODERN_DARK_COLORS = [
  "#7aa2f7",
  "#d7af5f",
  "#3dd69b",
  "#2ac3de",
  "#bb9af7",
  "#f7768e",
  "#ff9e64",
  "#73daca",
  "#9ece6a",
  "#7dcfff",
];

export const TOOL_MODERN_LIGHT_COLORS = [
  "#2e6be6",
  "#b88418",
  "#0e885d",
  "#0891b2",
  "#7c3aed",
  "#cf333a",
  "#d96b1f",
  "#0d9488",
  "#65a30d",
  "#0284c7",
];

export const TOOL_OTHER_COLOR = "#6c6c74";

/**
 * 返回当前外观与明暗主题对应的图表面板调色板。
 * @returns {string[]}
 */
export const toolSeriesColors = () =>
  document.documentElement.dataset.style === "modern"
    ? document.documentElement.dataset.theme === "dark"
      ? TOOL_MODERN_DARK_COLORS
      : TOOL_MODERN_LIGHT_COLORS
    : TOOL_SERIES_COLORS;

/**
 * 返回当前选择时间范围对应的分桶与标签元信息。
 * @param {StatsRange} [range]
 * @returns {{ windowLabel: string, bucketMs: number, spanMs: number | null }}
 */
export const toolRangeMeta = (range = app.range) => TOOL_RANGES[range] || TOOL_RANGES["7d"];

/**
 * 计算本地时间当天零点毫秒时间戳。
 * @param {number} timestamp
 * @returns {number}
 */
export function toolStartOfDay(timestamp) {
  const date = new Date(timestamp);
  date.setHours(0, 0, 0, 0);
  return date.getTime();
}

/**
 * 计算本地时间次日零点毫秒时间戳。
 * @param {number} timestamp
 * @returns {number}
 */
export function toolNextDay(timestamp) {
  const date = new Date(timestamp);
  date.setDate(date.getDate() + 1);
  return date.getTime();
}

/**
 * 生成与服务端分桶规则对齐的连续时间桶起点数组。
 * @param {number[]} timestamps
 * @param {StatsRange} [range]
 * @param {number} [nowMs]
 * @returns {number[]}
 */
export function toolBucketAxis(
  timestamps,
  range = app.range,
  nowMs = app.data?.updated_at || Date.now(),
) {
  const meta = toolRangeMeta(range);
  /** @type {number[]} */
  const buckets = [];
  if (meta.bucketMs < 86400000 && meta.spanMs != null) {
    const first = Math.floor((nowMs - meta.spanMs) / meta.bucketMs) * meta.bucketMs;
    const last = Math.floor(nowMs / meta.bucketMs) * meta.bucketMs;
    for (let t = first; t <= last; t += meta.bucketMs) buckets.push(t);
  } else {
    let earliest = nowMs;
    for (const timestamp of timestamps) if (timestamp < earliest) earliest = timestamp;
    const first = toolStartOfDay(meta.spanMs == null ? earliest : nowMs - meta.spanMs);
    const last = toolStartOfDay(nowMs);
    for (let t = first; t <= last; t = toolNextDay(t)) buckets.push(t);
  }
  return buckets.length > 1500 ? buckets.slice(-1500) : buckets;
}

/**
 * 将点集按时间桶对齐、汇总并按总量降序切分 Top-N 与 Other 折叠系列。
 * @template TPoint
 * @param {TPoint[]} points
 * @param {number[]} buckets
 * @param {number} limit
 * @param {{
 *   keyOf: (point: TPoint) => string,
 *   timestampOf: (point: TPoint) => number,
 *   valueOf: (point: TPoint) => number,
 *   colorOf: (key: string, rank: number) => string,
 *   labelOf?: (key: string) => string,
 * }} options
 * @returns {Array<{ key: string, label: string, color: string, values: number[] }>}
 */
export function pivotSeries(points, buckets, limit, options) {
  const index = new Map(buckets.map((timestamp, i) => [timestamp, i]));
  /** @type {Map<string, number[]>} */
  const byKey = new Map();
  for (const point of points) {
    const i = index.get(options.timestampOf(point));
    if (i === undefined) continue;
    const key = options.keyOf(point);
    let values = byKey.get(key);
    if (!values) {
      values = Array.from({ length: buckets.length }, () => 0);
      byKey.set(key, values);
    }
    values[i] += options.valueOf(point);
  }
  const ranked = [...byKey.entries()]
    .map(([key, values]) => ({
      key,
      values,
      total: values.reduce((totalSum, value) => totalSum + value, 0),
    }))
    .filter((entry) => entry.total !== 0)
    .sort((a, b) => b.total - a.total || a.key.localeCompare(b.key));
  const head = ranked.slice(0, limit);
  const tail = ranked.slice(limit);
  const series = head.map((entry, rank) => ({
    key: entry.key,
    label: options.labelOf ? options.labelOf(entry.key) : entry.key,
    color: options.colorOf(entry.key, rank),
    values: entry.values,
  }));
  if (tail.length) {
    const other = Array.from({ length: buckets.length }, () => 0);
    for (const entry of tail) {
      for (let i = 0; i < other.length; i++) other[i] += entry.values[i];
    }
    series.push({
      key: "__other__",
      label: `Other (${tail.length})`,
      color: TOOL_OTHER_COLOR,
      values: other,
    });
  }
  return series;
}

/**
 * 将点集按时间桶聚合为单条稠密序列。
 * @param {Array<{ timestamp: number, value: number }>} points
 * @param {number[]} buckets
 * @returns {number[]}
 */
export function toolDensify(points, buckets) {
  const index = new Map(buckets.map((timestamp, i) => [timestamp, i]));
  const out = Array.from({ length: buckets.length }, () => 0);
  for (const point of points) {
    const i = index.get(point.timestamp);
    if (i !== undefined) out[i] += point.value;
  }
  return out;
}

/**
 * 返回时间桶粒度文案。
 * @param {number} bucketMs
 * @returns {string}
 */
export function toolBucketWord(bucketMs) {
  return bucketMs < 3600000 ? "5 minutes" : bucketMs < 86400000 ? "hour" : "day";
}

/**
 * 格式化时间轴刻度文本。
 * @param {number} timestamp
 * @param {number} bucketMs
 * @returns {string}
 */
export function toolTickLabel(timestamp, bucketMs) {
  const date = new Date(timestamp);
  if (bucketMs < 86400000) return `${pad2(date.getHours())}:${pad2(date.getMinutes())}`;
  return date.toLocaleString("en-US", { month: "short", day: "numeric" });
}

/**
 * 格式化图例与悬停提示中的桶时间标签。
 * @param {number} timestamp
 * @param {number} bucketMs
 * @returns {string}
 */
export function toolBucketLabel(timestamp, bucketMs) {
  const date = new Date(timestamp);
  if (bucketMs < 3600000) return `${pad2(date.getHours())}:${pad2(date.getMinutes())}`;
  if (bucketMs < 86400000) {
    return `${date.toLocaleString("en-US", { month: "short", day: "numeric" })} ${pad2(date.getHours())}:${pad2(date.getMinutes())}`;
  }
  return date.toLocaleString("en-US", { month: "short", day: "numeric" });
}

/**
 * 计算整洁的坐标轴边界与刻度数组。
 * @param {number} max
 * @param {number} [min]
 * @returns {{ min: number, max: number, ticks: number[] }}
 */
export function toolNiceScale(max, min = 0) {
  const hi = Math.max(0, max);
  const lo = Math.min(0, min);
  if (!(hi - lo > 0)) return { min: 0, max: 1, ticks: [0] };
  const rough = (hi - lo) / 4;
  const magnitude = 10 ** Math.floor(Math.log10(rough));
  const step =
    [1, 2, 2.5, 5, 10]
      .map((multiple) => multiple * magnitude)
      .find((candidate) => candidate >= rough) ?? 10 * magnitude;
  const top = Math.ceil(hi / step) * step;
  const bottom = Math.floor(lo / step) * step;
  const ticks = [];
  for (let tick = bottom; tick <= top + step / 2; tick += step) {
    ticks.push(Number(tick.toPrecision(12)));
  }
  return { min: bottom, max: top > bottom ? top : bottom + step, ticks };
}

/**
 * 渲染无坐标轴迷你趋势线（用于指标卡片与表格趋势列）。
 * @param {Array<number | null>} values
 * @param {number | null} width
 * @param {number} height
 * @param {string} color
 * @returns {string}
 */
export function toolSparkline(values, width, height, color) {
  const actual = width ?? 260;
  const numeric = values.map((v) => v ?? 0);
  const max = Math.max(0, ...numeric);
  const min = Math.min(0, ...numeric);
  const count = numeric.length;
  let line = "";
  if (count > 0) {
    const step = count > 1 ? actual / (count - 1) : 0;
    for (let i = 0; i < count; i++) {
      const x = count > 1 ? i * step : actual / 2;
      const y =
        max > min ? height - 2 - ((numeric[i] - min) / (max - min)) * (height - 4) : height - 2;
      line += `${i === 0 ? "M" : "L"}${x.toFixed(1)},${y.toFixed(1)}`;
    }
  }
  const attrs = width
    ? `width="${width}" height="${height}"`
    : `viewBox="0 0 260 ${height}" width="100%" height="${height}" preserveAspectRatio="none"`;
  return `<svg class="x-sparkline" ${attrs} aria-hidden="true"><path class="area" d="${line}L${actual},${height}L0,${height}Z" fill="${color}"/><path class="x-chart-line" d="${line}" stroke="${color}"/></svg>`;
}

/**
 * 渲染 `x-stat` 指标卡。
 * @param {string} label
 * @param {string} value
 * @param {string | undefined} [hint]
 * @param {{ size?: "sm" | "md", title?: string, spark?: Array<number | null>, sparkColor?: string }} [options]
 * @returns {string}
 */
export function toolStat(label, value, hint, options = {}) {
  const size = options.size ?? "md";
  return `<div class="x-stat" data-size="${size}"${options.title ? ` title="${h(options.title)}"` : ""}><div class="x-stat-label">${h(label)}</div><div class="x-stat-value">${h(value)}</div>${hint !== undefined ? `<div class="x-stat-foot"><span class="x-truncate">${h(hint)}</span></div>` : ""}${options.spark ? `<div class="x-stat-spark">${toolSparkline(options.spark, null, 28, options.sparkColor || "var(--chart-primary)")}</div>` : ""}</div>`;
}

/** @typedef {{
 *   key: string,
 *   label: string,
 *   color: string,
 *   values: Array<number | null>,
 *   kind?: "bars" | "line",
 *   axis?: "left" | "right",
 *   dashed?: boolean,
 * }} ChartSeries */

/** @typedef {{
 *   series: ChartSeries[],
 *   buckets: number[],
 *   bucketMs: number,
 *   height: number,
 *   emptyLabel?: string,
 *   stacked?: boolean,
 *   yMax?: number,
 *   yMaxRight?: number,
 *   hidden?: Set<string>,
 *   format?: (value: number) => string,
 *   formatRight?: (value: number) => string,
 *   formatTooltip?: (value: number) => string,
 *   showTotal?: boolean,
 *   tickLabel?: (index: number) => string,
 *   tooltipTitle?: (index: number) => string,
 *   extraRows?: (index: number) => string,
 * }} ChartModel */

/** @type {Map<string, ChartModel>} */
let toolChartModels = new Map();
let toolChartSeq = 0;

/**
 * 在每次进入新页面渲染前重置页面内图表注册表。
 */
export function resetToolCharts() {
  toolChartModels = new Map();
  toolChartSeq = 0;
}

/**
 * 注册图表模型并返回占位容器 HTML。
 * @param {{ buckets: number[], bucketMs?: number }} view
 * @param {ChartSeries[]} series
 * @param {string | undefined} emptyLabel
 * @param {Partial<ChartModel>} [options]
 * @returns {string}
 */
export function toolChart(view, series, emptyLabel, options = {}) {
  const id = `tool-chart-${++toolChartSeq}`;
  /** @type {ChartModel} */
  const model = {
    series,
    buckets: view.buckets,
    bucketMs: view.bucketMs ?? toolRangeMeta().bucketMs,
    height: 260,
    emptyLabel,
    ...options,
  };
  toolChartModels.set(id, model);
  return `<div class="x-chart" data-chart-id="${id}" style="height:${model.height}px"></div>`;
}

/**
 * 渲染可点击切换显示的图例。
 * @param {ChartSeries[]} series
 * @param {(value: number) => string} [format]
 * @param {{ hidden?: Set<string>, attribute?: string, focusAttribute?: string, values?: boolean }} [options]
 * @returns {string}
 */
export function toolLegend(series, format = compact, options = {}) {
  const hidden = options.hidden || app.hiddenSeries;
  const attribute = options.attribute || "data-series";
  const focus = options.focusAttribute || "data-tools-focus";
  const values = options.values !== false;
  return `<div class="x-legend">${series
    .map(
      (item) =>
        `<button type="button" class="x-legend-item" ${attribute}="${h(item.key)}" data-off="${hidden.has(item.key)}" aria-pressed="${!hidden.has(item.key)}" ${focus}="legend-${encodeURIComponent(item.key)}" title="${h(item.label)}"><span class="x-swatch" style="background:${item.color}"></span><span class="x-truncate">${h(item.label)}</span>${values ? `<span class="x-legend-value">${format(item.values.reduce((acc, value) => (acc ?? 0) + (value ?? 0), 0) ?? 0)}</span>` : ""}</button>`,
    )
    .join("")}</div>`;
}

/**
 * 渲染 `x-card` 面板容器。
 * @param {number} index
 * @param {string} title
 * @param {string} description
 * @param {string} actions
 * @param {string} body
 * @param {boolean} [flush]
 * @param {string} [footer]
 * @returns {string}
 */
export function toolCard(index, title, description, actions, body, flush = false, footer = "") {
  return `<section class="x-card x-rise" style="--i:${index}"><header class="x-card-header"><div class="x-card-titles"><h2 class="x-card-title">${h(title)}</h2><p class="x-card-description">${h(description)}</p></div>${actions ? `<div class="x-card-actions">${actions}</div>` : ""}</header><div class="x-card-body"${flush ? ' data-flush="true"' : ""}>${body}</div>${footer ? `<footer class="x-card-footer">${footer}</footer>` : ""}</section>`;
}

/**
 * 根据模型与容器宽度生成双轴/堆叠/折线 SVG 图表与几何元数据。
 * @param {ChartModel} model
 * @param {number} width
 */
export function toolChartSvg(model, width) {
  const hidden = model.hidden || app.hiddenSeries;
  const visible = model.series.filter((series) => !hidden.has(series.key));
  const slots = model.buckets.length;
  const height = model.height;
  const padTop = 10;
  const padBottom = 24;
  const plotH = height - padTop - padBottom;
  const stacked = model.stacked !== false;
  /** @param {ChartSeries} series */
  const kind = (series) => series.kind || "bars";
  const left = visible.filter((series) => series.axis !== "right");
  const right = visible.filter((series) => series.axis === "right");
  /** @param {ChartSeries} series */
  const stackable = (series) => stacked && kind(series) === "bars";
  const bars = visible.filter((series) => kind(series) === "bars");
  const grouped = bars.filter((series) => !stackable(series));
  /** @type {Map<string, number[]>} */
  const bases = new Map();
  const tops = Array.from({ length: slots }, () => 0);
  const bottoms = Array.from({ length: slots }, () => 0);
  let leftMax = 0;
  let leftMin = 0;
  for (const series of left) {
    if (stackable(series)) {
      const base = Array.from({ length: slots }, () => 0);
      for (let i = 0; i < slots; i++) {
        const value = series.values[i] ?? 0;
        if (value < 0) {
          base[i] = bottoms[i];
          bottoms[i] += value;
        } else {
          base[i] = tops[i];
          tops[i] += value;
        }
      }
      bases.set(series.key, base);
      continue;
    }
    for (let i = 0; i < slots; i++) {
      leftMax = Math.max(leftMax, series.values[i] ?? 0);
      leftMin = Math.min(leftMin, series.values[i] ?? 0);
    }
  }
  for (let i = 0; i < slots; i++) {
    leftMax = Math.max(leftMax, tops[i]);
    leftMin = Math.min(leftMin, bottoms[i]);
  }
  let rightMax = 0;
  let rightMin = 0;
  for (const series of right) {
    for (let i = 0; i < slots; i++) {
      rightMax = Math.max(rightMax, series.values[i] ?? 0);
      rightMin = Math.min(rightMin, series.values[i] ?? 0);
    }
  }
  const scale = toolNiceScale(model.yMax ?? leftMax, leftMin);
  const rightScale = right.length ? toolNiceScale(model.yMaxRight ?? rightMax, rightMin) : null;
  /**
   * @param {{ min: number, max: number }} axis
   * @param {number} value
   */
  const project = (axis, value) =>
    padTop + plotH - ((value - axis.min) / (axis.max - axis.min)) * plotH;
  /** @param {number} value */
  const yLeft = (value) => project(scale, value);
  /** @param {number} value */
  const yRight = (value) => (rightScale ? project(rightScale, value) : yLeft(value));
  const format = model.format || compact;
  const formatRight = model.formatRight || format;
  const tickLabel =
    model.tickLabel ||
    ((/** @type {number} */ index) => toolTickLabel(model.buckets[index], model.bucketMs));
  const labels = scale.ticks.map((tick) => format(tick));
  const rightLabels = rightScale ? rightScale.ticks.map((tick) => formatRight(tick)) : [];
  /** @param {string[]} items */
  const labelWidth = (items) => items.reduce((max, label) => Math.max(max, label.length * 6.3), 0);
  const padLeft = Math.max(28, labelWidth(labels) + 12);
  const padRight = rightScale ? Math.max(28, labelWidth(rightLabels) + 12) : 8;
  const plotW = Math.max(0, width - padLeft - padRight);
  const slotW = slots > 0 ? plotW / slots : 0;
  /** @param {number} index */
  const cx = (index) => padLeft + (index + 0.5) * slotW;
  const barW = Math.max(1, Math.min(56, slotW * (slotW > 6 ? 0.72 : 0.9)));
  const tickEvery = Math.max(1, Math.ceil(slots / Math.max(1, Math.floor(plotW / 68))));
  const empty = visible.every((series) => series.values.every((value) => !value));
  /**
   * @param {ChartSeries} series
   * @param {number} index
   */
  const pointAt = (series, index) => {
    const value = series.values[index];
    if (value === null || value === undefined) return null;
    return (series.axis === "right" ? yRight : yLeft)(value);
  };
  let svg = `<svg width="${width}" height="${height}" role="img">`;
  svg += '<g class="x-chart-grid">';
  for (const tick of scale.ticks) {
    svg += `<line x1="${padLeft}" x2="${padLeft + plotW}" y1="${yLeft(tick)}" y2="${yLeft(tick)}"/>`;
  }
  svg += "</g>";
  svg += '<g class="x-chart-axis">';
  scale.ticks.forEach((tick, index) => {
    svg += `<text x="${padLeft - 8}" y="${yLeft(tick)}" dy="0.32em" text-anchor="end">${h(labels[index])}</text>`;
  });
  if (rightScale) {
    rightScale.ticks.forEach((tick, index) => {
      svg += `<text x="${padLeft + plotW + 8}" y="${yRight(tick)}" dy="0.32em" text-anchor="start">${h(rightLabels[index])}</text>`;
    });
  }
  for (let i = 0; i < slots; i++) {
    if (i % tickEvery !== 0) continue;
    svg += `<text x="${cx(i)}" y="${height - 6}" text-anchor="middle">${h(tickLabel(i))}</text>`;
  }
  svg += "</g>";
  svg += `<rect class="x-chart-cursor" x="${padLeft}" y="${padTop}" width="${slotW}" height="${plotH}" style="display:none"/>`;
  for (const series of bars) {
    const base = bases.get(series.key);
    const group = grouped.indexOf(series);
    const subW = group >= 0 ? barW / grouped.length : barW;
    const y = series.axis === "right" ? yRight : yLeft;
    for (let i = 0; i < slots; i++) {
      const value = series.values[i];
      if (!value) continue;
      const bottom = base?.[i] ?? 0;
      const y0 = y(bottom);
      const y1 = y(bottom + value);
      const x = cx(i) - barW / 2 + (group >= 0 ? group * subW : 0);
      svg += `<rect class="x-chart-bar" data-hover="false" data-index="${i}" x="${x}" y="${Math.min(y0, y1)}" width="${Math.max(1, subW - (group >= 0 && grouped.length > 1 ? 1 : 0))}" height="${Math.max(value > 0 ? 1 : 0, Math.abs(y0 - y1))}" fill="${series.color}"/>`;
    }
  }
  for (const series of visible.filter((item) => kind(item) === "line")) {
    let path = "";
    let pen = false;
    for (let i = 0; i < slots; i++) {
      const value = pointAt(series, i);
      if (value === null) {
        pen = false;
        continue;
      }
      path += `${pen ? "L" : "M"}${cx(i).toFixed(1)},${value.toFixed(1)}`;
      pen = true;
    }
    const dashes = series.dashed ? ' stroke-dasharray="4 3"' : "";
    svg += `<g><path class="x-chart-line" d="${path}" stroke="${series.color}"${dashes}/>`;
    for (let i = 0; i < slots; i++) {
      const value = pointAt(series, i);
      if (value === null) continue;
      if (
        (i > 0 && pointAt(series, i - 1) !== null) ||
        (i < slots - 1 && pointAt(series, i + 1) !== null)
      ) {
        continue;
      }
      svg += `<circle cx="${cx(i)}" cy="${value}" r="2.5" fill="${series.color}"/>`;
    }
    svg += "</g>";
  }
  svg += `<line class="x-chart-baseline" x1="${padLeft}" x2="${padLeft + plotW}" y1="${yLeft(0)}" y2="${yLeft(0)}"/>`;
  svg += "</svg>";
  return {
    svg,
    empty,
    visible,
    slots,
    width,
    height,
    padTop,
    plotH,
    padLeft,
    plotW,
    slotW,
    cx,
  };
}

/**
 * 筛选当前悬停时间桶下的非零提示行并按值降序排列。
 * @param {ChartSeries[]} visible
 * @param {number} hover
 * @returns {Array<{ series: ChartSeries, value: number }>}
 */
function toolChartTooltipRows(visible, hover) {
  return visible
    .map((series) => ({ series, value: series.values[hover] }))
    .filter(
      (row) =>
        row.value !== null && row.value !== undefined && (row.value !== 0 || visible.length === 1),
    )
    .map((row) => ({ series: row.series, value: /** @type {number} */ (row.value) }))
    .sort((a, b) => (visible.length > 1 ? b.value - a.value : 0));
}

/**
 * 更新图表悬停游标与浮动提示框位置。
 * @param {HTMLElement} root
 * @param {ReturnType<typeof toolChartSvg> & { model: ChartModel }} geometry
 * @param {number | null} hover
 */
function toolChartHover(root, geometry, hover) {
  const tooltip = /** @type {HTMLElement | null} */ (root.querySelector(".x-chart-tooltip"));
  const cursor = /** @type {SVGElement | null} */ (root.querySelector(".x-chart-cursor"));
  const bars = root.querySelectorAll(".x-chart-bar");
  if (hover === null) {
    root.dataset.hovering = "false";
    if (cursor) cursor.style.display = "none";
    for (const bar of bars) {
      if (bar instanceof SVGElement) bar.dataset.hover = "false";
    }
    if (tooltip) tooltip.hidden = true;
    return;
  }
  root.dataset.hovering = "true";
  if (cursor) {
    cursor.style.display = "";
    cursor.setAttribute("x", String(geometry.padLeft + hover * geometry.slotW));
  }
  for (const bar of bars) {
    if (bar instanceof SVGElement) {
      bar.dataset.hover = String(bar.dataset.index === String(hover));
    }
  }
  if (!tooltip) return;
  if (geometry.empty) {
    tooltip.hidden = true;
    return;
  }
  const rows = toolChartTooltipRows(geometry.visible, hover);
  const total = rows.reduce((acc, row) => acc + (row.series.axis === "right" ? 0 : row.value), 0);
  const model = geometry.model;
  /** @param {{ series: ChartSeries, value: number }} row */
  const formatValue = (row) =>
    row.series.axis === "right"
      ? (model.formatRight || model.formatTooltip || number)(row.value)
      : (model.formatTooltip || number)(row.value);
  const title = model.tooltipTitle
    ? model.tooltipTitle(hover)
    : toolBucketLabel(model.buckets[hover], model.bucketMs);
  tooltip.innerHTML = `<div class="x-chart-tooltip-title">${h(title)}</div>${rows
    .map(
      (row) =>
        `<div class="x-chart-tooltip-row"><span class="x-swatch" style="background:${row.series.color}"></span><span class="x-chart-tooltip-label">${h(row.series.label)}</span><span class="x-chart-tooltip-value">${formatValue(row)}</span></div>`,
    )
    .join("")}${
    rows.length > 1 && model.showTotal !== false && rows.some((row) => row.series.axis !== "right")
      ? `<div class="x-chart-tooltip-row x-chart-tooltip-total"><span class="x-chart-tooltip-label">Total</span><span class="x-chart-tooltip-value">${(model.formatTooltip || number)(total)}</span></div>`
      : ""
  }${model.extraRows ? model.extraRows(hover) : ""}`;
  tooltip.hidden = false;
  const cx = geometry.cx(hover);
  if (cx > geometry.width / 2) {
    tooltip.style.left = "";
    tooltip.style.right = `${geometry.width - cx + Math.min(geometry.slotW / 2, 24) + 8}px`;
  } else {
    tooltip.style.right = "";
    tooltip.style.left = `${cx + Math.min(geometry.slotW / 2, 24) + 8}px`;
  }
  tooltip.style.top = `${geometry.padTop}px`;
}

/**
 * 测量容器宽度、绘制 SVG 图表并绑定悬停事件。
 */
export function wireToolsCharts() {
  const pageEl = $("page");
  if (!pageEl) return;
  for (const root of pageEl.querySelectorAll("[data-chart-id]")) {
    if (!(root instanceof HTMLElement)) continue;
    const model = toolChartModels.get(root.dataset.chartId ?? "");
    if (!model) continue;
    const width = root.clientWidth;
    if (width <= 0) continue;
    const drawn = toolChartSvg(model, width);
    root.innerHTML = `${drawn.svg}${drawn.empty ? `<div class="x-chart-empty">${h(model.emptyLabel || "No data in this range")}</div>` : ""}<div class="x-chart-tooltip" hidden></div>`;
    const geometry = { ...drawn, model };
    root.onpointermove = (event) => {
      const box = root.getBoundingClientRect();
      const localY = event.clientY - box.top;
      if (
        event.clientX - box.left < geometry.padLeft ||
        localY < geometry.padTop ||
        localY > geometry.padTop + geometry.plotH
      ) {
        toolChartHover(root, geometry, null);
        return;
      }
      const index = Math.floor((event.clientX - box.left - geometry.padLeft) / geometry.slotW);
      toolChartHover(root, geometry, index >= 0 && index < geometry.slots ? index : null);
    };
    root.onpointerleave = () => toolChartHover(root, geometry, null);
  }
}

export { fixedCost };
