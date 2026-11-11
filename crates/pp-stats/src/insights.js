// Tools 页复刻 omp stats 的 ToolsRoute：字体、颜色、卡片/表格/图表结构与可点击位置。
let toolFilter = "",
  insightSort = "cost",
  toolMetric = "calls",
  toolSort = { key: "calls", dir: "desc" },
  toolModelSort = { key: "calls", dir: "desc" },
  toolShown = 20,
  toolModelShown = 25;

function resetInsights() {
  toolFilter = "";
  insightSort = "cost";
  toolMetric = "calls";
  toolSort = { key: "calls", dir: "desc" };
  toolModelSort = { key: "calls", dir: "desc" };
  toolShown = 20;
  toolModelShown = 25;
}

const TOOL_SERIES_COLORS = [
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
const TOOL_OTHER_COLOR = "#6c6c74";
const TOOL_TOP_TOOLS = 6;
const TOOL_ATTRIBUTION =
  "Tokens and API-equivalent cost of each invoking turn, split evenly across that turn's tool calls";
const TOOL_RANGES = {
  "1h": { windowLabel: "the last hour", bucketMs: 300000, spanMs: 3600000 },
  "24h": { windowLabel: "the last 24 hours", bucketMs: 3600000, spanMs: 86400000 },
  "7d": { windowLabel: "the last 7 days", bucketMs: 86400000, spanMs: 604800000 },
  "30d": { windowLabel: "the last 30 days", bucketMs: 86400000, spanMs: 2592000000 },
  "90d": { windowLabel: "the last 90 days", bucketMs: 86400000, spanMs: 7776000000 },
  all: { windowLabel: "all time", bucketMs: 86400000, spanMs: null },
};
const TOOL_INBOX =
  '<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><polyline points="22 12 16 12 14 15 10 15 8 12 2 12"/><path d="M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z"/></svg>';
const TOOL_ARROW_UP =
  '<svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m5 12 7-7 7 7"/><path d="M12 19V5"/></svg>';
const TOOL_ARROW_DOWN =
  '<svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14"/><path d="m19 12-7 7-7-7"/></svg>';
const TOOL_CLEAR =
  '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg>';

const toolInteger = (value) => (value == null ? "—" : Number(value).toLocaleString("en-US"));
const toolCompact = (value) =>
  value == null ? "—" : Number(value).toLocaleString("en-US", { notation: "compact" });
const toolPercent = (value, digits = 1) => `${(value * 100).toFixed(digits)}%`;
const toolErrorRate = (value) => {
  const percent = value * 100;
  if (percent > 0 && percent < 0.005) return "<0.01%";
  return toolPercent(value, percent > 0 && percent < 0.1 ? 2 : 1);
};
const toolCost = (value, digits) => {
  if (value == null) return "—";
  if (value === 0) return "$0";
  const places = digits ?? (value > 0 && value < 0.01 ? 4 : 2);
  return `$${value.toLocaleString("en-US", {
    minimumFractionDigits: places,
    maximumFractionDigits: places,
  })}`;
};
const toolEstimatedCost = (value, unpriced) =>
  value == null ? "—" : value === 0 && unpriced > 0 ? "N/A" : toolCost(value);
const toolRelative = (timestamp) => (timestamp == null ? "—" : relative(timestamp));
const toolErrorTone = (rate) => (rate >= 0.05 ? "bad" : rate >= 0.01 ? "warn" : "ok");
const toolPad = (value) => String(value).padStart(2, "0");
const toolRangeMeta = () => TOOL_RANGES[range] || TOOL_RANGES["7d"];

function toolStartOfDay(timestamp) {
  const date = new Date(timestamp);
  date.setHours(0, 0, 0, 0);
  return date.getTime();
}
function toolNextDay(timestamp) {
  const date = new Date(timestamp);
  date.setDate(date.getDate() + 1);
  return date.getTime();
}

/** Contiguous bucket starts aligned with the server's bucket function. */
function toolBucketAxis(timestamps) {
  const meta = toolRangeMeta();
  const now = data?.updated_at || Date.now();
  const buckets = [];
  if (meta.bucketMs < 86400000) {
    const first = Math.floor((now - meta.spanMs) / meta.bucketMs) * meta.bucketMs;
    const last = Math.floor(now / meta.bucketMs) * meta.bucketMs;
    for (let t = first; t <= last; t += meta.bucketMs) buckets.push(t);
  } else {
    let earliest = now;
    for (const timestamp of timestamps) if (timestamp < earliest) earliest = timestamp;
    const first = toolStartOfDay(meta.spanMs == null ? earliest : now - meta.spanMs);
    const last = toolStartOfDay(now);
    for (let t = first; t <= last; t = toolNextDay(t)) buckets.push(t);
  }
  return buckets.length > 1500 ? buckets.slice(-1500) : buckets;
}

function toolColorLookup(rows) {
  const ranked = [...rows].sort((a, b) => b.calls - a.calls || a.tool.localeCompare(b.tool));
  return new Map(
    ranked.map((row, index) => [row.tool, TOOL_SERIES_COLORS[index % TOOL_SERIES_COLORS.length]]),
  );
}

function toolPivot(points, buckets, limit, colors, keyOf = (point) => point.tool) {
  const index = new Map(buckets.map((timestamp, i) => [timestamp, i]));
  const byKey = new Map();
  for (const point of points) {
    const i = index.get(point.timestamp);
    if (i === undefined) continue;
    const key = keyOf(point);
    let values = byKey.get(key);
    if (!values) {
      values = Array.from({ length: buckets.length }, () => 0);
      byKey.set(key, values);
    }
    values[i] += point.value;
  }
  const ranked = [...byKey.entries()]
    .map(([key, values]) => ({
      key,
      values,
      total: values.reduce((sum, value) => sum + value, 0),
    }))
    .filter((entry) => entry.total !== 0)
    .sort((a, b) => b.total - a.total || a.key.localeCompare(b.key));
  const head = ranked.slice(0, limit);
  const tail = ranked.slice(limit);
  const series = head.map((entry, rank) => ({
    key: entry.key,
    label: entry.key,
    color: colors.get(entry.key) ?? TOOL_SERIES_COLORS[rank % TOOL_SERIES_COLORS.length],
    values: entry.values,
  }));
  if (tail.length) {
    const other = Array.from({ length: buckets.length }, () => 0);
    for (const entry of tail) for (let i = 0; i < other.length; i++) other[i] += entry.values[i];
    series.push({
      key: "__other__",
      label: `Other (${tail.length})`,
      color: TOOL_OTHER_COLOR,
      values: other,
    });
  }
  return series;
}

function toolDensify(points, buckets) {
  const index = new Map(buckets.map((timestamp, i) => [timestamp, i]));
  const out = Array.from({ length: buckets.length }, () => 0);
  for (const point of points) {
    const i = index.get(point.timestamp);
    if (i !== undefined) out[i] += point.value;
  }
  return out;
}

function toolNormalize(tool) {
  return {
    tool: tool.tool,
    calls: tool.calls || 0,
    errors: tool.errors,
    argsChars: tool.args_chars ?? 0,
    resultChars: tool.result_chars,
    totalTokensShare: tool.tokens ?? 0,
    outputTokensShare: tool.output_tokens ?? 0,
    costShare: tool.cost,
    unpricedRequestsShare: tool.unpriced ?? 0,
    lastUsed: tool.last_seen,
    executions: tool.executions,
    buckets: tool.buckets || [],
    errorBuckets: tool.error_buckets || [],
    models: tool.models || [],
  };
}

function toolView() {
  const rows = (data?.tools || [])
    .map(toolNormalize)
    .map((row) => {
      const errorRate = row.calls > 0 ? (row.errors ?? 0) / row.calls : 0;
      return {
        ...row,
        errorsKnown: row.errors != null,
        errorRate,
        resultKnown: row.resultChars != null,
        avgResultChars:
          row.resultChars == null || row.calls === 0 ? null : row.resultChars / row.calls,
      };
    })
    .sort((a, b) => b.calls - a.calls);
  const calls = rows.reduce((sum, row) => sum + row.calls, 0);
  const tokens = rows.reduce((sum, row) => sum + row.totalTokensShare, 0);
  const knownCosts = rows.filter((row) => row.costShare != null);
  const cost = knownCosts.length ? knownCosts.reduce((sum, row) => sum + row.costShare, 0) : null;
  const errorsKnown = rows.some((row) => row.errorsKnown);
  const errors = errorsKnown ? rows.reduce((sum, row) => sum + (row.errors ?? 0), 0) : null;
  const resultKnown = rows.some((row) => row.resultKnown);
  const resultChars = resultKnown
    ? rows.reduce((sum, row) => sum + (row.resultChars ?? 0), 0)
    : null;
  const argsChars = rows.reduce((sum, row) => sum + row.argsChars, 0);
  for (const row of rows) {
    row.callFraction = calls > 0 ? row.calls / calls : 0;
    row.tokenFraction = tokens > 0 ? row.totalTokensShare / tokens : 0;
    row.costFraction = cost > 0 ? (row.costShare ?? 0) / cost : 0;
  }
  const colors = toolColorLookup(rows);
  const callPoints = rows.flatMap((row) =>
    row.buckets.map((bucket) => ({
      timestamp: bucket.timestamp,
      tool: row.tool,
      value: bucket.requests ?? 0,
    })),
  );
  const errorPoints = rows.flatMap((row) =>
    row.errorBuckets.map((bucket) => ({
      timestamp: bucket.timestamp,
      tool: row.tool,
      value: bucket.errors ?? 0,
    })),
  );
  const timestamps = callPoints
    .map((point) => point.timestamp)
    .concat(errorPoints.map((point) => point.timestamp));
  const buckets = toolBucketAxis(timestamps);
  const callSeries = toolPivot(callPoints, buckets, TOOL_TOP_TOOLS, colors).map((series) => ({
    ...series,
    values: series.values.map((value) => value || null),
  }));
  const errorSeries = toolPivot(errorPoints, buckets, TOOL_TOP_TOOLS, colors).map((series) => ({
    ...series,
    values: series.values.map((value) => value || null),
  }));
  const trends = new Map(
    toolPivot(callPoints, buckets, Number.POSITIVE_INFINITY, colors).map((series) => [
      series.key,
      series.values,
    ]),
  );
  const toolNames = rows.map((row) => row.tool).sort((a, b) => a.localeCompare(b));
  const picked = toolNames.includes(toolFilter) ? toolFilter : null;
  const totals = {
    calls,
    errors,
    errorsKnown,
    tools: rows.length,
    tokens,
    output: rows.reduce((sum, row) => sum + row.outputTokensShare, 0),
    cost,
    unpriced: rows.reduce((sum, row) => sum + row.unpricedRequestsShare, 0),
    resultChars,
    resultKnown,
    argsChars,
  };
  return {
    rows,
    colors,
    buckets,
    totals,
    callSeries,
    errorSeries,
    totalCalls: toolDensify(callPoints, buckets),
    totalErrors: toolDensify(errorPoints, buckets),
    toolNames,
    toolFilter: picked,
    trends,
  };
}

function toolBucketWord(bucketMs) {
  return bucketMs < 3600000 ? "5 minutes" : bucketMs < 86400000 ? "hour" : "day";
}

function toolTickLabel(timestamp, bucketMs) {
  const date = new Date(timestamp);
  if (bucketMs < 86400000) return `${toolPad(date.getHours())}:${toolPad(date.getMinutes())}`;
  return date.toLocaleString("en-US", { month: "short", day: "numeric" });
}

function toolBucketLabel(timestamp, bucketMs) {
  const date = new Date(timestamp);
  if (bucketMs < 3600000) return `${toolPad(date.getHours())}:${toolPad(date.getMinutes())}`;
  if (bucketMs < 86400000)
    return `${date.toLocaleString("en-US", { month: "short", day: "numeric" })} ${toolPad(date.getHours())}:${toolPad(date.getMinutes())}`;
  return date.toLocaleString("en-US", { month: "short", day: "numeric" });
}

function toolNiceScale(max, min = 0) {
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
  for (let tick = bottom; tick <= top + step / 2; tick += step)
    ticks.push(Number(tick.toPrecision(12)));
  return { min: bottom, max: top > bottom ? top : bottom + step, ticks };
}

/** Axis-free line used by stat tiles and table rows. */
function toolSparkline(values, width, height, color) {
  const actual = width ?? 260;
  const max = Math.max(0, ...values);
  const min = Math.min(0, ...values);
  const count = values.length;
  let line = "";
  if (count > 0) {
    const step = count > 1 ? actual / (count - 1) : 0;
    for (let i = 0; i < count; i++) {
      const x = count > 1 ? i * step : actual / 2;
      const y =
        max > min ? height - 2 - ((values[i] - min) / (max - min)) * (height - 4) : height - 2;
      line += `${i === 0 ? "M" : "L"}${x.toFixed(1)},${y.toFixed(1)}`;
    }
  }
  const attrs = width
    ? `width="${width}" height="${height}"`
    : `viewBox="0 0 260 ${height}" width="100%" height="${height}" preserveAspectRatio="none"`;
  return `<svg class="x-sparkline" ${attrs} aria-hidden="true"><path class="area" d="${line}L${actual},${height}L0,${height}Z" fill="${color}"/><path class="x-chart-line" d="${line}" stroke="${color}"/></svg>`;
}

function toolStat(label, value, hint, options = {}) {
  const size = options.size ?? "md";
  return `<div class="x-stat" data-size="${size}"${options.title ? ` title="${h(options.title)}"` : ""}><div class="x-stat-label">${h(label)}</div><div class="x-stat-value">${h(value)}</div>${hint !== undefined ? `<div class="x-stat-foot"><span class="x-truncate">${h(hint)}</span></div>` : ""}${options.spark ? `<div class="x-stat-spark">${toolSparkline(options.spark, null, 28, options.sparkColor || "var(--chart-primary)")}</div>` : ""}</div>`;
}

function toolStatGrids(view) {
  const totals = view.totals;
  const failed = totals.errorsKnown ? `${toolInteger(totals.errors)} failed` : "No execution data";
  const errorRate =
    totals.errorsKnown && totals.calls > 0 ? toolErrorRate(totals.errors / totals.calls) : "—";
  const errorHint = totals.errorsKnown
    ? `${toolInteger(totals.calls - totals.errors)} succeeded`
    : "No execution data";
  const costHint =
    totals.unpriced > 0
      ? `${toolInteger(Math.round(totals.unpriced))} unpriced requests`
      : "API-equivalent";
  const resultCells = [
    toolStat(
      "Result text",
      totals.resultChars == null ? "—" : `${toolCompact(totals.resultChars)} chars`,
      undefined,
      {
        size: "sm",
        title:
          "Characters of tool-result text fed back into context; not recorded by this pp version",
      },
    ),
    toolStat("Call arguments", `${toolCompact(totals.argsChars)} chars`, undefined, {
      size: "sm",
      title: "Characters of serialized tool-call arguments",
    }),
    toolStat(
      "Avg result per call",
      totals.resultChars == null || totals.calls === 0
        ? "—"
        : `${toolCompact(Math.round(totals.resultChars / totals.calls))} chars`,
      undefined,
      {
        size: "sm",
        title: "Mean tool-result characters per call; not recorded by this pp version",
      },
    ),
    toolStat(
      "Avg arguments per call",
      `${toolCompact(totals.calls > 0 ? Math.round(totals.argsChars / totals.calls) : 0)} chars`,
      undefined,
      { size: "sm" },
    ),
  ];
  return `<div class="x-stack x-rise" style="gap:16px">
    <div class="x-stat-grid x-rise" style="--stat-min:190px">
      ${toolStat("Tool calls", toolInteger(totals.calls), failed, {
        spark: view.totalCalls,
      })}
      ${toolStat(
        "Distinct tools",
        toolInteger(totals.tools),
        view.rows[0] ? `Most used: ${view.rows[0].tool}` : undefined,
      )}
      ${toolStat("Error rate", errorRate, errorHint, {
        title: "Tool results that came back flagged as errors",
        spark: view.totalErrors,
        sparkColor: "var(--bad)",
      })}
      ${toolStat(
        "Attributed tokens",
        toolCompact(Math.round(totals.tokens)),
        `${toolCompact(Math.round(totals.output))} output`,
        { title: TOOL_ATTRIBUTION },
      )}
      ${toolStat("Attributed cost", toolEstimatedCost(totals.cost, totals.unpriced), costHint, {
        title: `${TOOL_ATTRIBUTION}; API-equivalent estimate`,
      })}
    </div>
    <div class="x-stat-grid x-rise" style="--stat-min:150px">
      ${resultCells.join("")}
    </div>
  </div>`;
}

function toolMetricSegmented() {
  return `<div class="x-segmented" data-size="sm" role="radiogroup" aria-label="Call metric"><span class="x-segmented-thumb"></span>${[
    ["calls", "Calls"],
    ["errors", "Errors"],
  ]
    .map(
      ([value, label]) =>
        `<button type="button" class="x-segmented-option" role="radio" aria-checked="${toolMetric === value}" data-active="${toolMetric === value}" data-tool-metric="${value}" data-tools-focus="metric-${value}">${label}</button>`,
    )
    .join("")}</div>`;
}

let toolChartModels = new Map();
let toolChartSeq = 0;

function toolChart(view, series, emptyLabel, options = {}) {
  const id = `tool-chart-${++toolChartSeq}`;
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

function toolLegend(series, format = toolCompact) {
  return `<div class="x-legend">${series
    .map(
      (item) =>
        `<button type="button" class="x-legend-item" data-series="${h(item.key)}" data-off="${hiddenSeries.has(item.key)}" aria-pressed="${!hiddenSeries.has(item.key)}" data-tools-focus="legend-${encodeURIComponent(item.key)}" title="${h(item.label)}"><span class="x-swatch" style="background:${item.color}"></span><span class="x-truncate">${h(item.label)}</span><span class="x-legend-value">${format(item.values.reduce((sum, value) => sum + (value ?? 0), 0))}</span></button>`,
    )
    .join("")}</div>`;
}

function toolCard(index, title, description, actions, body, flush = false) {
  return `<section class="x-card x-rise" style="--i:${index}"><header class="x-card-header"><div class="x-card-titles"><h2 class="x-card-title">${h(title)}</h2><p class="x-card-description">${h(description)}</p></div>${actions ? `<div class="x-card-actions">${actions}</div>` : ""}</header><div class="x-card-body"${flush ? ' data-flush="true"' : ""}>${body}</div></section>`;
}

function toolSortHeader(column, sort, table) {
  const sorted = sort.key === column.key ? sort.dir : undefined;
  const arrow = sorted === "asc" ? TOOL_ARROW_UP : sorted === "desc" ? TOOL_ARROW_DOWN : "";
  return `<th data-align="${column.align || "left"}" data-sortable="${Boolean(column.sort)}"${sorted ? ` data-sorted="${sorted}" aria-sort="${sorted === "asc" ? "ascending" : "descending"}"` : ""}${column.title ? ` title="${h(column.title)}"` : ""}${column.sort ? ` data-toolsort="${column.key}" data-tooltable="${table}" data-toolalign="${column.align || "left"}" data-tools-focus="sort-${table}-${column.key}"` : ""}>${h(column.header)}${arrow ? `<span class="x-sort-arrow">${arrow}</span>` : ""}</th>`;
}

function toolSorted(rows, columns, sort) {
  const column = columns.find((item) => item.key === sort.key && item.sort);
  if (!column) return rows;
  const direction = sort.dir === "asc" ? 1 : -1;
  return [...rows].sort((a, b) => {
    const left = column.sort(a);
    const right = column.sort(b);
    const delta =
      typeof left === "number" && typeof right === "number"
        ? left - right
        : String(left).localeCompare(String(right));
    return delta * direction;
  });
}

function toolTableMore(total, shown, limit, table) {
  if (total <= limit) return "";
  return `<div class="x-table-more"><span class="x-micro">${toolInteger(shown)} of ${toolInteger(total)}</span>${total > shown ? `<button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-toolpage="${table}" data-toolaction="more" data-tools-focus="${table}-more">Show more</button>` : ""}${shown > limit ? `<button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-toolpage="${table}" data-toolaction="less" data-tools-focus="${table}-less">Show fewer</button>` : ""}</div>`;
}

function toolEmpty(title, hint) {
  return `<div class="x-empty">${TOOL_INBOX}<div class="x-empty-title">${h(title)}</div>${hint ? `<div class="x-empty-hint">${hint}</div>` : ""}</div>`;
}

function toolMeter(value, max, display, color) {
  const width = max > 0 ? Math.min(100, (value / max) * 100) : 0;
  return `<div class="x-meter-cell"><span class="x-num">${display}</span><div class="x-meter"><div class="x-meter-fill" style="width:${width}%${color ? `;background:${color}` : ""}"></div></div></div>`;
}

function toolErrorsCell(row) {
  if (!row.errorsKnown)
    return `<span class="x-row" style="gap:8px;justify-content:flex-end"><span class="x-num x-dim">—</span><span class="x-badge" data-mono="true">—</span></span>`;
  const tone = row.errors > 0 ? toolErrorTone(row.errorRate) : "neutral";
  return `<span class="x-row" style="gap:8px;justify-content:flex-end"><span class="x-num x-dim">${toolInteger(row.errors)}</span><span class="x-badge" data-tone="${tone}" data-mono="true">${toolErrorRate(row.errorRate)}</span></span>`;
}

function toolToolCell(row, view) {
  const color = view.colors.get(row.tool) ?? TOOL_OTHER_COLOR;
  return `<span class="x-row" style="gap:8px"><span class="x-swatch" style="background:${color}"></span><span class="x-mono x-truncate x-tools-name" title="${h(row.tool)}">${h(row.tool)}</span></span>`;
}

function toolTrendCell(row, view) {
  const values = view.trends.get(row.tool);
  if (!values || values.length <= 1) return '<span class="x-dim">–</span>';
  const color = view.colors.get(row.tool) ?? TOOL_OTHER_COLOR;
  return `<span style="display:inline-block;width:80px;height:20px">${toolSparkline(values, 80, 20, color)}</span>`;
}

function toolColumns(view) {
  const maxCalls = view.rows[0]?.calls ?? 0;
  return [
    {
      key: "tool",
      header: "Tool",
      sort: (row) => row.tool,
      render: (row) => toolToolCell(row, view),
    },
    {
      key: "trend",
      header: "Trend",
      title: "Calls per bucket over the range",
      render: (row) => toolTrendCell(row, view),
    },
    {
      key: "calls",
      header: "Calls",
      align: "right",
      sort: (row) => row.calls,
      render: (row) =>
        toolMeter(
          row.calls,
          maxCalls,
          `<span title="${toolPercent(row.callFraction)} of all tool calls">${toolInteger(row.calls)}<span class="x-dim x-tools-share">${toolPercent(row.callFraction, 0)}</span></span>`,
        ),
    },
    {
      key: "errorRate",
      header: "Errors",
      title: "Calls whose result came back flagged as an error",
      align: "right",
      sort: (row) => row.errorRate,
      render: (row) => toolErrorsCell(row),
    },
    {
      key: "args",
      header: "Args",
      title: "Characters of serialized tool-call arguments",
      align: "right",
      sort: (row) => row.argsChars,
      render: (row) => `<span class="x-num">${toolCompact(row.argsChars)}</span>`,
    },
    {
      key: "result",
      header: "Result",
      title: "Characters of tool-result text fed back into context",
      align: "right",
      sort: (row) => row.resultChars ?? -1,
      render: (row) =>
        `<span class="x-num"${row.resultKnown ? "" : ' title="Not recorded by this pp version"'}>${toolCompact(row.resultChars)}</span>`,
    },
    {
      key: "avgResult",
      header: "Result / call",
      title: "Mean tool-result characters per call",
      align: "right",
      sort: (row) => row.avgResultChars ?? -1,
      render: (row) =>
        `<span class="x-num x-muted">${toolCompact(row.avgResultChars == null ? null : Math.round(row.avgResultChars))}</span>`,
    },
    {
      key: "tokens",
      header: "Attr. tokens",
      title: TOOL_ATTRIBUTION,
      align: "right",
      sort: (row) => row.totalTokensShare,
      render: (row) =>
        `<span class="x-num" title="${toolPercent(row.tokenFraction)} of attributed tokens">${toolCompact(Math.round(row.totalTokensShare))}<span class="x-dim x-tools-share">${toolPercent(row.tokenFraction, 0)}</span></span>`,
    },
    {
      key: "cost",
      header: "Attr. cost",
      title: `${TOOL_ATTRIBUTION}; API-equivalent estimate`,
      align: "right",
      sort: (row) => row.costShare ?? -1,
      render: (row) =>
        `<span class="x-num" title="${toolPercent(row.costFraction)} of attributed cost">${toolEstimatedCost(row.costShare, row.unpricedRequestsShare)}<span class="x-dim x-tools-share">${toolPercent(row.costFraction, 0)}</span></span>`,
    },
    {
      key: "lastUsed",
      header: "Last used",
      align: "right",
      sort: (row) => row.lastUsed ?? -1,
      render: (row) =>
        `<span class="x-muted" title="${h(localTime(row.lastUsed))}">${toolRelative(row.lastUsed)}</span>`,
    },
  ];
}

function toolTable(view) {
  const columns = toolColumns(view);
  const sorted = toolSorted(view.rows, columns, toolSort);
  const visible = sorted.slice(0, toolShown);
  if (!sorted.length) return toolEmpty("No tool calls in this range");
  const head = columns.map((column) => toolSortHeader(column, toolSort, "tool")).join("");
  const body = visible
    .map(
      (row) =>
        `<tr data-clickable="true" data-selected="${row.tool === view.toolFilter}" data-toolselect="${encodeURIComponent(row.tool)}" data-tools-focus="row-${encodeURIComponent(row.tool)}" tabindex="0">${columns.map((column) => `<td data-align="${column.align || "left"}">${column.render(row)}</td>`).join("")}</tr>`,
    )
    .join("");
  return `<div class="x-table-wrap"><table class="x-table" data-dense="true"><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>${toolTableMore(sorted.length, visible.length, 20, "tool")}`;
}

function toolModelRows(view) {
  const rows = view.rows.flatMap((tool) =>
    tool.models.map((model) => {
      const calls = model.calls || 0;
      const errors = model.errors;
      return {
        tool: tool.tool,
        model: model.model,
        provider: model.provider,
        calls,
        errors,
        errorsKnown: errors != null,
        errorRate: calls > 0 ? (errors ?? 0) / calls : 0,
        resultChars: model.result_chars,
        resultKnown: model.result_chars != null,
        totalTokensShare: model.tokens ?? 0,
        costShare: model.cost,
        unpricedRequestsShare: model.unpriced ?? 0,
        lastUsed: model.last_seen,
      };
    }),
  );
  return view.toolFilter ? rows.filter((row) => row.tool === view.toolFilter) : rows;
}

function toolModelColumns(rows, view) {
  const maxCalls = rows.reduce((max, row) => Math.max(max, row.calls), 0);
  return [
    {
      key: "tool",
      header: "Tool",
      sort: (row) => row.tool,
      render: (row) => toolToolCell(row, view),
    },
    {
      key: "model",
      header: "Model",
      sort: (row) => row.model,
      render: (row) =>
        `<div class="x-row" style="gap:10px"><div class="x-stack" style="gap:0"><span class="x-cell-primary x-truncate"><span class="x-mono">${h(row.model || "(unknown)")}</span></span><span class="x-cell-secondary x-truncate">${h(row.provider)}</span></div></div>`,
    },
    {
      key: "calls",
      header: "Calls",
      align: "right",
      sort: (row) => row.calls,
      render: (row) => toolMeter(row.calls, maxCalls, toolInteger(row.calls)),
    },
    {
      key: "errorRate",
      header: "Errors",
      align: "right",
      sort: (row) => row.errorRate,
      render: (row) => toolErrorsCell(row),
    },
    {
      key: "result",
      header: "Result",
      title: "Characters of tool-result text fed back into context",
      align: "right",
      sort: (row) => row.resultChars ?? -1,
      render: (row) =>
        `<span class="x-num"${row.resultKnown ? "" : ' title="Not recorded by this pp version"'}>${toolCompact(row.resultChars)}</span>`,
    },
    {
      key: "tokens",
      header: "Attr. tokens",
      title: TOOL_ATTRIBUTION,
      align: "right",
      sort: (row) => row.totalTokensShare,
      render: (row) =>
        `<span class="x-num">${toolCompact(Math.round(row.totalTokensShare))}</span>`,
    },
    {
      key: "cost",
      header: "Attr. cost",
      title: `${TOOL_ATTRIBUTION}; API-equivalent estimate`,
      align: "right",
      sort: (row) => row.costShare ?? -1,
      render: (row) =>
        `<span class="x-num">${toolEstimatedCost(row.costShare, row.unpricedRequestsShare)}</span>`,
    },
    {
      key: "lastUsed",
      header: "Last used",
      align: "right",
      sort: (row) => row.lastUsed ?? -1,
      render: (row) =>
        `<span class="x-muted" title="${h(localTime(row.lastUsed))}">${toolRelative(row.lastUsed)}</span>`,
    },
  ];
}

function toolModelTable(view) {
  const rows = toolModelRows(view);
  const columns = toolModelColumns(rows, view);
  const sorted = toolSorted(rows, columns, toolModelSort);
  const visible = sorted.slice(0, toolModelShown);
  if (!sorted.length)
    return toolEmpty(
      view.toolFilter
        ? `No calls to ${view.toolFilter} in this range`
        : "No tool calls in this range",
    );
  const head = columns.map((column) => toolSortHeader(column, toolModelSort, "model")).join("");
  const body = visible
    .map(
      (row) =>
        `<tr>${columns.map((column) => `<td data-align="${column.align || "left"}">${column.render(row)}</td>`).join("")}</tr>`,
    )
    .join("");
  return `<div class="x-table-wrap"><table class="x-table" data-dense="true"><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>${toolTableMore(sorted.length, visible.length, 25, "model")}`;
}

function toolChartSvg(model, width) {
  const visible = model.series.filter((series) => !hiddenSeries.has(series.key));
  const slots = model.buckets.length;
  const height = model.height;
  const padTop = 10;
  const padBottom = 24;
  const plotH = height - padTop - padBottom;
  const bars = visible.filter((series) => series.kind !== "line");
  const bases = new Map();
  const tops = Array.from({ length: slots }, () => 0);
  const bottoms = Array.from({ length: slots }, () => 0);
  let leftMax = 0;
  let leftMin = 0;
  for (const series of bars) {
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
  }
  for (let i = 0; i < slots; i++) {
    leftMax = Math.max(leftMax, tops[i]);
    leftMin = Math.min(leftMin, bottoms[i]);
  }
  const scale = toolNiceScale(leftMax, leftMin);
  const y = (value) => padTop + plotH - ((value - scale.min) / (scale.max - scale.min)) * plotH;
  const format = model.format || toolCompact;
  const tickLabel =
    model.tickLabel || ((index) => toolTickLabel(model.buckets[index], model.bucketMs));
  const labels = scale.ticks.map((tick) => format(tick));
  const labelWidth = labels.reduce((max, label) => Math.max(max, label.length * 6.3), 0);
  const padLeft = Math.max(28, labelWidth + 12);
  const padRight = 8;
  const plotW = Math.max(0, width - padLeft - padRight);
  const slotW = slots > 0 ? plotW / slots : 0;
  const cx = (index) => padLeft + (index + 0.5) * slotW;
  const barW = Math.max(1, Math.min(56, slotW * (slotW > 6 ? 0.72 : 0.9)));
  const tickEvery = Math.max(1, Math.ceil(slots / Math.max(1, Math.floor(plotW / 68))));
  const empty = visible.every((series) => series.values.every((value) => !value));
  let svg = `<svg width="${width}" height="${height}" role="img">`;
  svg += '<g class="x-chart-grid">';
  for (const tick of scale.ticks)
    svg += `<line x1="${padLeft}" x2="${padLeft + plotW}" y1="${y(tick)}" y2="${y(tick)}"/>`;
  svg += "</g>";
  svg += '<g class="x-chart-axis">';
  scale.ticks.forEach((tick, index) => {
    svg += `<text x="${padLeft - 8}" y="${y(tick)}" dy="0.32em" text-anchor="end">${h(labels[index])}</text>`;
  });
  for (let i = 0; i < slots; i++) {
    if (i % tickEvery !== 0) continue;
    svg += `<text x="${cx(i)}" y="${height - 6}" text-anchor="middle">${h(tickLabel(i))}</text>`;
  }
  svg += "</g>";
  svg += `<rect class="x-chart-cursor" x="${padLeft}" y="${padTop}" width="${slotW}" height="${plotH}" style="display:none"/>`;
  for (const series of bars) {
    const base = bases.get(series.key);
    for (let i = 0; i < slots; i++) {
      const value = series.values[i];
      if (!value) continue;
      const bottom = base?.[i] ?? 0;
      const y0 = y(bottom);
      const y1 = y(bottom + value);
      const x = cx(i) - barW / 2;
      svg += `<rect class="x-chart-bar" data-hover="false" data-index="${i}" x="${x}" y="${Math.min(y0, y1)}" width="${Math.max(1, barW)}" height="${Math.max(value > 0 ? 1 : 0, Math.abs(y0 - y1))}" fill="${series.color}"/>`;
    }
  }
  svg += `<line class="x-chart-baseline" x1="${padLeft}" x2="${padLeft + plotW}" y1="${y(0)}" y2="${y(0)}"/>`;
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

function toolChartTooltipRows(visible, hover) {
  return visible
    .map((series) => ({ series, value: series.values[hover] }))
    .filter(
      (row) =>
        row.value !== null && row.value !== undefined && (row.value !== 0 || visible.length === 1),
    )
    .sort((a, b) => (visible.length > 1 ? b.value - a.value : 0));
}

function toolChartHover(root, geometry, hover) {
  const tooltip = root.querySelector(".x-chart-tooltip");
  const cursor = root.querySelector(".x-chart-cursor");
  const bars = root.querySelectorAll(".x-chart-bar");
  if (hover === null) {
    root.dataset.hovering = "false";
    if (cursor) cursor.style.display = "none";
    for (const bar of bars) bar.dataset.hover = "false";
    if (tooltip) tooltip.hidden = true;
    return;
  }
  root.dataset.hovering = "true";
  if (cursor) {
    cursor.style.display = "";
    cursor.setAttribute("x", geometry.padLeft + hover * geometry.slotW);
  }
  for (const bar of bars) bar.dataset.hover = String(bar.dataset.index === String(hover));
  if (!tooltip) return;
  if (geometry.empty) {
    tooltip.hidden = true;
    return;
  }
  const rows = toolChartTooltipRows(geometry.visible, hover);
  const total = rows.reduce((sum, row) => sum + row.value, 0);
  const model = geometry.model;
  const formatValue = model.formatTooltip || toolInteger;
  const title = model.tooltipTitle
    ? model.tooltipTitle(hover)
    : toolBucketLabel(model.buckets[hover], model.bucketMs);
  tooltip.innerHTML = `<div class="x-chart-tooltip-title">${h(title)}</div>${rows
    .map(
      (row) =>
        `<div class="x-chart-tooltip-row"><span class="x-swatch" style="background:${row.series.color}"></span><span class="x-chart-tooltip-label">${h(row.series.label)}</span><span class="x-chart-tooltip-value">${formatValue(row.value)}</span></div>`,
    )
    .join(
      "",
    )}${rows.length > 1 && model.showTotal !== false ? `<div class="x-chart-tooltip-row x-chart-tooltip-total"><span class="x-chart-tooltip-label">Total</span><span class="x-chart-tooltip-value">${formatValue(total)}</span></div>` : ""}${model.extraRows ? model.extraRows(hover) : ""}`;
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

function wireToolsCharts() {
  for (const root of $("page").querySelectorAll("[data-chart-id]")) {
    const model = toolChartModels.get(root.dataset.chartId);
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

function toolsPage() {
  if (!data) return '<div class="loading">Loading tools…</div>';
  toolChartModels = new Map();
  toolChartSeq = 0;
  const view = toolView();
  const meta = toolRangeMeta();
  const series = toolMetric === "calls" ? view.callSeries : view.errorSeries;
  const chartTitle = toolMetric === "calls" ? "Calls over time" : "Errors over time";
  const chartDescription = `Per ${toolBucketWord(meta.bucketMs)}, top ${TOOL_TOP_TOOLS} tools stacked`;
  const chartBody = `<div class="x-stack" style="gap:12px">${toolChart(
    view,
    series,
    toolMetric === "errors" ? "No tool errors in this range" : undefined,
  )}${toolLegend(series)}</div>`;
  const byToolBody = toolTable(view);
  const clearButton = view.toolFilter
    ? `<button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-icon="true" title="Clear tool filter" aria-label="Clear tool filter" data-clear-tool data-tools-focus="clear-tool">${TOOL_CLEAR}</button>`
    : "";
  const modelActions = `<div class="x-row" style="gap:6px"><select class="x-input x-tools-filter" id="tool-filter" aria-label="Filter by tool"><option value="">All tools</option>${view.toolNames
    .map(
      (name) =>
        `<option value="${h(name)}"${name === view.toolFilter ? " selected" : ""}>${h(name)}</option>`,
    )
    .join("")}</select>${clearButton}</div>`;
  return `<div class="x-page"><header class="x-page-header x-rise"><div><h1 class="x-page-title">Tools</h1><p class="x-page-description">Which tools pp called in ${h(meta.windowLabel)}, how often they failed, and what they cost.</p></div></header>
    ${toolStatGrids(view)}
    ${toolCard(1, chartTitle, chartDescription, toolMetricSegmented(), chartBody)}
    ${toolCard(2, "By tool", `${TOOL_ATTRIBUTION}. Select a tool to break it down by model.`, "", byToolBody, true)}
    ${toolCard(3, "By tool and model", "Which models call which tools, and how often those calls fail", modelActions, toolModelTable(view), true)}
  </div>`;
}

function projectRank(projects, field, title, color) {
  const rows = [...projects]
    .filter((g) => g[field] != null)
    .sort((a, b) => b[field] - a[field])
    .slice(0, 6);
  const total = data.summary[field],
    max = Math.max(...rows.map((g) => g[field]), 1e-12);
  return chartPanel(
    title,
    field === "cost" ? "Share of priced API-equivalent estimate" : "Share of all requests",
    `<div class="insight-ranking">${rows.map((g) => `<button data-insight-open="${h(groupKey("project", g.key))}" title="${h(g.key)}" style="--rank-width:${(100 * g[field]) / max}%;--rank-color:${color}"><span>${h(g.project || g.key || "Unknown directory")}</span><span class="num">${field === "cost" ? money(g[field]) : number(g[field])} · ${percent(g[field], total)}</span></button>`).join("") || '<div class="empty">No data in this range</div>'}</div>`,
  );
}
function projectDetails(g) {
  return `<div class="group-details"><div class="session-path">${h(g.key || "Working directory not recorded")}</div>${detailsGrid(
    [
      ["Sessions", number(g.sessions)],
      ["Requests without session", number(g.requests_without_session)],
      ["Avg TTFT", duration(g.avg_ttft_ms)],
      ["Pricing coverage", costNote(g)],
      ["First active", localTime(g.first_seen)],
      ["Last active", localTime(g.last_seen)],
    ],
  )}${tokenMix(g)}<h2>Models · select a row for usage and latency</h2>${analyticsTable(
    ["Model", "Requests", "Tokens", "Cost", "Cache rate", "Errors"],
    [...g.models]
      .sort((a, b) => b.requests - a.requests)
      .map((m) =>
        groupRow(
          { ...m, key: JSON.stringify([g.key, m.key]), description: `${m.provider} · ${m.key}` },
          "project-model",
          numericCells([
            number(m.requests),
            detailed(m.tokens),
            detailed(m.cost, money(m.cost), " USD"),
            cacheRate(m),
            percent(m.failed, m.requests),
          ]),
          () =>
            `<div class="group-details">${detailsGrid([
              ["Avg duration", duration(m.avg_duration_ms)],
              ["Avg TTFT", duration(m.avg_ttft_ms)],
              ["Pricing coverage", costNote(m)],
              ["Aborted", number(m.aborted)],
            ])}${tokenMix(m)}</div>`,
          6,
        ),
      )
      .join(""),
  )}<h2>Latest requests · up to 10 in this range</h2>${insightRecent(g.recent)}</div>`;
}
function projectsPage() {
  if (!data) return '<div class="loading">Loading projects…</div>';
  const s = data.summary,
    q = query.trim().toLowerCase();
  const rows = [...data.projects]
    .filter(
      (g) =>
        !q ||
        [g.project, g.key, ...g.models.map((m) => m.key)].some((v) => v.toLowerCase().includes(q)),
    )
    .sort((a, b) => (b[insightSort] ?? -1) - (a[insightSort] ?? -1) || a.key.localeCompare(b.key));
  return `<h1>Projects</h1><p class="subtitle">Usage by working directory in the ${descriptions[range]}.</p><div class="panel metrics">${metric("Folders", number(data.projects.length))}${metric("Requests", number(s.requests), `${number(s.failed)} failed`)}${metric("API-equivalent cost", money(s.cost), costNote(s))}${metric("Conversation tokens", short(s.tokens))}${metric("Cache rate", cacheRate(s))}</div>
    <div class="graphs project-ranks">${projectRank(data.projects, "cost", "Top by cost", "var(--pink)")}${projectRank(data.projects, "requests", "Top by requests", "var(--accent)")}</div>
    <section class="panel analytics-panel"><div class="requests-heading panel-heading"><div><h2>Folders</h2><p class="panel-description">${number(rows.length)} folders · select a row for tokens, models and recent requests</p></div><div class="trace-controls">${activitySearch("Search folders", "Folder, path or model…")}<select class="insight-select" id="insight-sort" aria-label="Sort folders">${[
      ["cost", "Cost"],
      ["requests", "Requests"],
      ["tokens", "Tokens"],
      ["last_seen", "Last active"],
    ]
      .map(
        ([key, label]) =>
          `<option value="${key}"${key === insightSort ? " selected" : ""}>${label}</option>`,
      )
      .join("")}</select></div></div>${analyticsTable(
      [
        "Folder",
        "Requests",
        "Cost",
        "Tokens",
        "Cache rate",
        "Errors",
        "Avg duration",
        "Last active",
      ],
      rows
        .slice(0, visible)
        .map((g) =>
          groupRow(
            { ...g, label: g.project || g.key || "Unknown directory", description: g.key },
            "project",
            numericCells([
              number(g.requests),
              detailed(g.cost, money(g.cost), " USD"),
              detailed(g.tokens),
              cacheRate(g),
              percent(g.failed, g.requests),
              detailed(g.avg_duration_ms, duration(g.avg_duration_ms), " ms"),
              `<span title="${h(localTime(g.last_seen))}">${relative(g.last_seen)}</span>`,
            ]),
            () => projectDetails(g),
            8,
          ),
        )
        .join(""),
    )}${activityFooter(rows)}</section><p class="load-note">Folders are grouped by the full recorded working directory. Different paths with the same folder name stay separate. All summaries cover every request in the selected range.</p>${pricingNotice()}`;
}
function insightsClick(e) {
  const target = e.target.closest("[data-insight-open]");
  if (target) {
    const key = target.dataset.insightOpen;
    expandedGroups.add(key);
    query = "";
    visible = Math.max(100, data.projects.length);
    render();
    const row = [...$("page").querySelectorAll("[data-expand]")].find(
      (el) => el.dataset.expand === key,
    );
    row?.focus({ preventScroll: true });
    row?.scrollIntoView({ block: "center", behavior: "smooth" });
    return true;
  }
  const metricButton = e.target.closest("[data-tool-metric]");
  if (metricButton) {
    toolMetric = metricButton.dataset.toolMetric;
    render();
    return true;
  }
  const sortHeader = e.target.closest("[data-toolsort]");
  if (sortHeader) {
    const table = sortHeader.dataset.tooltable;
    const key = sortHeader.dataset.toolsort;
    const state = table === "model" ? toolModelSort : toolSort;
    const direction =
      state.key === key
        ? state.dir === "asc"
          ? "desc"
          : "asc"
        : sortHeader.dataset.toolalign === "right"
          ? "desc"
          : "asc";
    if (table === "model") toolModelSort = { key, dir: direction };
    else toolSort = { key, dir: direction };
    render();
    return true;
  }
  const row = e.target.closest("[data-toolselect]");
  if (row) {
    const name = decodeURIComponent(row.dataset.toolselect);
    toolFilter = toolFilter === name ? "" : name;
    render();
    return true;
  }
  const pageButton = e.target.closest("[data-toolpage]");
  if (pageButton) {
    const table = pageButton.dataset.toolpage;
    const action = pageButton.dataset.toolaction;
    if (table === "model") {
      toolModelShown = action === "more" ? toolModelShown + 25 * 4 : 25;
    } else {
      toolShown = action === "more" ? toolShown + 20 * 4 : 20;
    }
    render();
    return true;
  }
  if (e.target.closest("[data-clear-tool]")) {
    toolFilter = "";
    render();
    return true;
  }
  return false;
}
function insightsChange(e) {
  if (!["tool-filter", "insight-sort"].includes(e.target.id)) return false;
  const id = e.target.id;
  if (id === "tool-filter") toolFilter = e.target.value;
  else insightSort = e.target.value;
  render();
  $(id)?.focus({ preventScroll: true });
  return true;
}
window.addEventListener("resize", wireToolsCharts);
