/**
 * Tools page and Insights group interaction dispatch (Tools / Hooks / Projects).
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
  wireToolsCharts,
} from "./web/charts.js";
import {
  $,
  compact,
  errorRateText,
  estimatedCost,
  h,
  localTime,
  number,
  ratioPercent,
  relative,
  TOOL_CLEAR,
} from "./web/format.js";
import { hooksClick, hooksPage, resetHooks } from "./web/hooks.js";
import { projectsPage, resetProjects, setInsightSort } from "./web/projects.js";
import { app, hooks } from "./web/state.js";
import { renderSortHeader, renderTableMore, sortRows, xEmpty, xMeter } from "./web/table.js";

/** @typedef {import("../web-types/api.d.ts").ProjectsResponse} ProjectsResponse */
/** @typedef {import("../web-types/api.d.ts").SortDirection} SortDirection */
/** @typedef {import("../web-types/api.d.ts").SortState} SortState */
/** @typedef {import("../web-types/api.d.ts").ToolRow} ToolRow */
/** @typedef {import("../web-types/api.d.ts").ToolsResponse} ToolsResponse */

const TOOL_TOP_TOOLS = 6;
const TOOL_ATTRIBUTION =
  "Tokens and API-equivalent cost of each invoking turn, split evenly across that turn's tool calls";

let toolFilter = "";
/** @type {"calls" | "errors"} */
let toolMetric = "calls";
/** @type {SortState} */
let toolSort = { key: "calls", dir: "desc" };
/** @type {SortState} */
let toolModelSort = { key: "calls", dir: "desc" };
let toolShown = 20;
let toolModelShown = 25;

export function resetInsights() {
  toolFilter = "";
  toolMetric = "calls";
  toolSort = { key: "calls", dir: "desc" };
  toolModelSort = { key: "calls", dir: "desc" };
  toolShown = 20;
  toolModelShown = 25;
  resetHooks();
  resetProjects();
}

/** @param {number} rate */
const toolErrorTone = (rate) => (rate >= 0.05 ? "bad" : rate >= 0.01 ? "warn" : "ok");

/** @param {ToolRow[]} rows */
function toolColorLookup(rows) {
  const palette = toolSeriesColors();
  const ranked = [...rows].sort((a, b) => b.calls - a.calls || a.tool.localeCompare(b.tool));
  return new Map(ranked.map((row, index) => [row.tool, palette[index % palette.length]]));
}

/**
 * @param {Array<{ timestamp: number, tool: string, value: number }>} points
 * @param {number[]} buckets
 * @param {number} limit
 * @param {Map<string, string>} colors
 */
export function toolPivot(points, buckets, limit, colors) {
  const palette = toolSeriesColors();
  return pivotSeries(points, buckets, limit, {
    keyOf: (point) => point.tool,
    timestampOf: (point) => point.timestamp,
    valueOf: (point) => point.value,
    colorOf: (key, rank) => colors.get(key) ?? palette[rank % palette.length],
  });
}

/** @param {ToolRow} tool */
function toolNormalize(tool) {
  return {
    tool: tool.tool,
    calls: tool.calls || 0,
    errors: tool.errors,
    argsChars: tool.args_chars ?? 0,
    resultChars: tool.result_chars,
    resultCalls: tool.result_calls ?? 0,
    totalTokensShare: tool.tokens ?? null,
    outputTokensShare: tool.output_tokens ?? null,
    usageKnown: tool.usage_known ?? 0,
    costShare: tool.cost,
    unpricedRequestsShare: tool.unpriced ?? 0,
    lastUsed: tool.last_seen,
    executions: tool.executions,
    buckets: tool.buckets || [],
    errorBuckets: tool.error_buckets || [],
    models: tool.models || [],
  };
}

/**
 * Builds the Tools page view model so tests and rendering share it.
 * @param {ToolsResponse | null} [source]
 */
export function toolView(source = /** @type {ToolsResponse | null} */ (app.data)) {
  const rows = (source?.tools || [])
    .map(toolNormalize)
    .map((row) => {
      const errorRate = row.calls > 0 ? (row.errors ?? 0) / row.calls : 0;
      return {
        ...row,
        errorsKnown: row.errors != null,
        errorRate,
        resultKnown: row.resultChars != null,
        avgResultChars:
          row.resultChars == null || row.resultCalls === 0
            ? null
            : row.resultChars / row.resultCalls,
        callFraction: 0,
        tokenFraction: /** @type {number | null} */ (null),
        costFraction: 0,
      };
    })
    .sort((a, b) => b.calls - a.calls);
  const calls = rows.reduce((sum, row) => sum + row.calls, 0);
  const knownTokens = rows.flatMap((row) =>
    row.totalTokensShare == null ? [] : [row.totalTokensShare],
  );
  const tokens = knownTokens.length
    ? knownTokens.reduce((sum, value) => sum + value, 0)
    : rows.length === 0
      ? 0
      : null;
  const knownOutput = rows.flatMap((row) =>
    row.outputTokensShare == null ? [] : [row.outputTokensShare],
  );
  const output = knownOutput.length
    ? knownOutput.reduce((sum, value) => sum + value, 0)
    : rows.length === 0
      ? 0
      : null;
  const usageKnown = rows.reduce((sum, row) => sum + row.usageKnown, 0);
  // Known-request coverage: null when entirely unknown, and unknown rows never count as covered.
  const tokenCoverage = tokens == null || calls === 0 ? null : usageKnown / calls;
  const knownCosts = rows.filter((row) => row.costShare != null);
  const cost = knownCosts.length
    ? knownCosts.reduce((sum, row) => sum + (row.costShare ?? 0), 0)
    : null;
  const errorsKnown = rows.some((row) => row.errorsKnown);
  const errors = errorsKnown ? rows.reduce((sum, row) => sum + (row.errors ?? 0), 0) : null;
  const resultKnown = rows.some((row) => row.resultKnown);
  const resultChars = resultKnown
    ? rows.reduce((sum, row) => sum + (row.resultChars ?? 0), 0)
    : null;
  const resultCalls = rows.reduce((sum, row) => sum + row.resultCalls, 0);
  const argsChars = rows.reduce((sum, row) => sum + row.argsChars, 0);
  for (const row of rows) {
    row.callFraction = calls > 0 ? row.calls / calls : 0;
    row.tokenFraction =
      row.totalTokensShare == null
        ? null
        : tokens != null && tokens > 0
          ? row.totalTokensShare / tokens
          : 0;
    row.costFraction = cost != null && cost > 0 ? (row.costShare ?? 0) / cost : 0;
  }
  const colors = toolColorLookup(source?.tools || []);
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
    output,
    usageKnown,
    tokenCoverage,
    cost,
    unpriced: rows.reduce((sum, row) => sum + row.unpricedRequestsShare, 0),
    resultChars,
    resultKnown,
    resultCalls,
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

/** @param {ReturnType<typeof toolView>} view */
function toolStatGrids(view) {
  const totals = view.totals;
  const failed = totals.errorsKnown ? `${number(totals.errors)} failed` : "No execution data";
  const errorRate =
    totals.errorsKnown && totals.calls > 0
      ? errorRateText((totals.errors ?? 0) / totals.calls)
      : "—";
  const errorHint = totals.errorsKnown
    ? `${number(totals.calls - (totals.errors ?? 0))} succeeded`
    : "No execution data";
  const costHint =
    totals.unpriced > 0
      ? `${number(Math.round(totals.unpriced))} unpriced requests`
      : "API-equivalent";
  const outputHint = compact(totals.output == null ? null : Math.round(totals.output));
  const tokensHint =
    totals.tokens == null
      ? "No token usage recorded"
      : totals.tokenCoverage != null && totals.tokenCoverage < 1
        ? `${outputHint} output · ${ratioPercent(totals.tokenCoverage, 0)} of calls known`
        : `${outputHint} output`;
  const resultCells = [
    toolStat(
      "Result text",
      totals.resultChars == null ? "—" : `${compact(totals.resultChars)} chars`,
      totals.resultKnown ? `${number(totals.resultCalls)} recorded calls` : "No result data",
      {
        size: "sm",
        title:
          "Characters of tool-result text fed back into context; totals include recorded results only",
      },
    ),
    toolStat("Call arguments", `${compact(totals.argsChars)} chars`, undefined, {
      size: "sm",
      title: "Characters of serialized tool-call arguments",
    }),
    toolStat(
      "Avg result per call",
      totals.resultChars == null || totals.resultCalls === 0
        ? "—"
        : `${compact(Math.round(totals.resultChars / totals.resultCalls))} chars`,
      totals.resultKnown ? `${number(totals.resultCalls)} recorded calls` : "No result data",
      {
        size: "sm",
        title:
          "Mean tool-result characters per recorded result; historical and unexecuted calls are excluded",
      },
    ),
    toolStat(
      "Avg arguments per call",
      `${compact(totals.calls > 0 ? Math.round(totals.argsChars / totals.calls) : 0)} chars`,
      undefined,
      { size: "sm" },
    ),
  ];
  return `<div class="x-stack x-rise" style="gap:16px">
    <div class="x-stat-grid x-rise" style="--stat-min:190px">
      ${toolStat("Tool calls", number(totals.calls), failed, {
        spark: view.totalCalls,
      })}
      ${toolStat(
        "Distinct tools",
        number(totals.tools),
        view.rows[0] ? `Most used: ${view.rows[0].tool}` : undefined,
      )}
      ${toolStat("Error rate", errorRate, errorHint, {
        title: "Tool results that came back flagged as errors",
        spark: view.totalErrors,
        sparkColor: "var(--bad)",
      })}
      ${toolStat(
        "Attributed tokens",
        compact(totals.tokens == null ? null : Math.round(totals.tokens)),
        tokensHint,
        { title: TOOL_ATTRIBUTION },
      )}
      ${toolStat("Attributed cost", estimatedCost(totals.cost, totals.unpriced), costHint, {
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

/** @param {{ errorsKnown: boolean, errors: number | null, errorRate: number }} row */
function toolErrorsCell(row) {
  if (!row.errorsKnown) {
    return '<span class="x-row" style="gap:8px;justify-content:flex-end"><span class="x-num x-dim">—</span><span class="x-badge" data-mono="true">—</span></span>';
  }
  const tone = (row.errors ?? 0) > 0 ? toolErrorTone(row.errorRate) : "neutral";
  return `<span class="x-row" style="gap:8px;justify-content:flex-end"><span class="x-num x-dim">${number(row.errors)}</span><span class="x-badge" data-tone="${tone}" data-mono="true">${errorRateText(row.errorRate)}</span></span>`;
}

/**
 * @param {{ tool: string }} row
 * @param {ReturnType<typeof toolView>} view
 */
function toolToolCell(row, view) {
  const color = view.colors.get(row.tool) ?? TOOL_OTHER_COLOR;
  return `<span class="x-row" style="gap:8px"><span class="x-swatch" style="background:${color}"></span><span class="x-mono x-truncate x-tools-name" title="${h(row.tool)}">${h(row.tool)}</span></span>`;
}

/**
 * @param {ReturnType<typeof toolView>["rows"][number]} row
 * @param {ReturnType<typeof toolView>} view
 */
function toolTrendCell(row, view) {
  const values = view.trends.get(row.tool);
  if (!values || values.length <= 1) return '<span class="x-dim">–</span>';
  const color = view.colors.get(row.tool) ?? TOOL_OTHER_COLOR;
  return `<span style="display:inline-block;width:80px;height:20px">${toolSparkline(values, 80, 20, color)}</span>`;
}

/**
 * @param {ReturnType<typeof toolView>} view
 * @returns {import("./web/table.js").TableColumn<ReturnType<typeof toolView>["rows"][number]>[]}
 */
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
        xMeter(
          row.calls,
          maxCalls,
          `<span title="${ratioPercent(row.callFraction)} of all tool calls">${number(row.calls)}<span class="x-dim x-tools-share">${ratioPercent(row.callFraction, 0)}</span></span>`,
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
      render: (row) => `<span class="x-num">${compact(row.argsChars)}</span>`,
    },
    {
      key: "result",
      header: "Result",
      title: "Characters of tool-result text fed back into context",
      align: "right",
      sort: (row) => row.resultChars ?? -1,
      render: (row) =>
        `<span class="x-num" title="${row.resultKnown ? `${number(row.resultCalls)} recorded calls` : "No result data"}">${compact(row.resultChars)}</span>`,
    },
    {
      key: "avgResult",
      header: "Result / call",
      title: "Mean tool-result characters per recorded result",
      align: "right",
      sort: (row) => row.avgResultChars ?? -1,
      render: (row) =>
        `<span class="x-num x-muted">${compact(row.avgResultChars == null ? null : Math.round(row.avgResultChars))}</span>`,
    },
    {
      key: "tokens",
      header: "Attr. tokens",
      title: TOOL_ATTRIBUTION,
      align: "right",
      sort: (row) => row.totalTokensShare ?? -1,
      render: (row) =>
        row.totalTokensShare == null || row.tokenFraction == null
          ? '<span class="x-num x-muted" title="Token usage not recorded for these calls">—</span>'
          : `<span class="x-num" title="${ratioPercent(row.tokenFraction)} of attributed tokens">${compact(Math.round(row.totalTokensShare))}<span class="x-dim x-tools-share">${ratioPercent(row.tokenFraction, 0)}</span></span>`,
    },
    {
      key: "cost",
      header: "Attr. cost",
      title: `${TOOL_ATTRIBUTION}; API-equivalent estimate`,
      align: "right",
      sort: (row) => row.costShare ?? -1,
      render: (row) =>
        `<span class="x-num" title="${ratioPercent(row.costFraction)} of attributed cost">${estimatedCost(row.costShare, row.unpricedRequestsShare)}<span class="x-dim x-tools-share">${ratioPercent(row.costFraction, 0)}</span></span>`,
    },
    {
      key: "lastUsed",
      header: "Last used",
      align: "right",
      sort: (row) => row.lastUsed ?? -1,
      render: (row) =>
        `<span class="x-muted" title="${h(localTime(row.lastUsed))}">${row.lastUsed == null ? "—" : relative(row.lastUsed)}</span>`,
    },
  ];
}

/** @param {ReturnType<typeof toolView>} view */
function toolTable(view) {
  const columns = toolColumns(view);
  const sorted = sortRows(view.rows, columns, toolSort);
  const visible = sorted.slice(0, toolShown);
  if (!sorted.length) return xEmpty("No tool calls in this range");
  const head = columns
    .map((column) =>
      renderSortHeader(column, toolSort, {
        sortAttr: "data-toolsort",
        tableAttr: "data-tooltable",
        table: "tool",
        alignAttr: "data-toolalign",
        focusAttr: "data-tools-focus",
        focusPrefix: "sort-tool-",
      }),
    )
    .join("");
  const body = visible
    .map(
      (row) =>
        `<tr data-clickable="true" data-selected="${row.tool === view.toolFilter}" data-toolselect="${encodeURIComponent(row.tool)}" data-tools-focus="row-${encodeURIComponent(row.tool)}" tabindex="0">${columns.map((column) => `<td data-align="${column.align || "left"}">${column.render(row)}</td>`).join("")}</tr>`,
    )
    .join("");
  return `<div class="x-table-wrap"><table class="x-table" data-dense="true"><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>${renderTableMore(
    {
      total: sorted.length,
      shown: visible.length,
      step: 20,
      moreAttr: 'data-toolpage="tool" data-toolaction="more"',
      lessAttr: 'data-toolpage="tool" data-toolaction="less"',
      focusAttr: "data-tools-focus",
      focusMore: "tool-more",
      focusLess: "tool-less",
    },
  )}`;
}

/** @param {ReturnType<typeof toolView>} view */
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
        totalTokensShare: model.tokens ?? null,
        outputTokensShare: model.output_tokens ?? null,
        usageKnown: model.usage_known ?? 0,
        costShare: model.cost,
        unpricedRequestsShare: model.unpriced ?? 0,
        lastUsed: model.last_seen,
      };
    }),
  );
  return view.toolFilter ? rows.filter((row) => row.tool === view.toolFilter) : rows;
}

/**
 * @param {ReturnType<typeof toolModelRows>} rows
 * @param {ReturnType<typeof toolView>} view
 * @returns {import("./web/table.js").TableColumn<ReturnType<typeof toolModelRows>[number]>[]}
 */
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
      render: (row) => xMeter(row.calls, maxCalls, number(row.calls)),
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
        `<span class="x-num"${row.resultKnown ? "" : ' title="Not recorded by this pp version"'}>${compact(row.resultChars)}</span>`,
    },
    {
      key: "tokens",
      header: "Attr. tokens",
      title: TOOL_ATTRIBUTION,
      align: "right",
      sort: (row) => row.totalTokensShare ?? -1,
      render: (row) =>
        row.totalTokensShare == null
          ? '<span class="x-num x-muted" title="Token usage not recorded for these calls">—</span>'
          : `<span class="x-num">${compact(Math.round(row.totalTokensShare))}</span>`,
    },
    {
      key: "cost",
      header: "Attr. cost",
      title: `${TOOL_ATTRIBUTION}; API-equivalent estimate`,
      align: "right",
      sort: (row) => row.costShare ?? -1,
      render: (row) =>
        `<span class="x-num">${estimatedCost(row.costShare, row.unpricedRequestsShare)}</span>`,
    },
    {
      key: "lastUsed",
      header: "Last used",
      align: "right",
      sort: (row) => row.lastUsed ?? -1,
      render: (row) =>
        `<span class="x-muted" title="${h(localTime(row.lastUsed))}">${row.lastUsed == null ? "—" : relative(row.lastUsed)}</span>`,
    },
  ];
}

/** @param {ReturnType<typeof toolView>} view */
function toolModelTable(view) {
  const rows = toolModelRows(view);
  const columns = toolModelColumns(rows, view);
  const sorted = sortRows(rows, columns, toolModelSort);
  const visible = sorted.slice(0, toolModelShown);
  if (!sorted.length) {
    return xEmpty(
      view.toolFilter
        ? `No calls to ${view.toolFilter} in this range`
        : "No tool calls in this range",
    );
  }
  const head = columns
    .map((column) =>
      renderSortHeader(column, toolModelSort, {
        sortAttr: "data-toolsort",
        tableAttr: "data-tooltable",
        table: "model",
        alignAttr: "data-toolalign",
        focusAttr: "data-tools-focus",
        focusPrefix: "sort-model-",
      }),
    )
    .join("");
  const body = visible
    .map(
      (row) =>
        `<tr>${columns.map((column) => `<td data-align="${column.align || "left"}">${column.render(row)}</td>`).join("")}</tr>`,
    )
    .join("");
  return `<div class="x-table-wrap"><table class="x-table" data-dense="true"><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>${renderTableMore(
    {
      total: sorted.length,
      shown: visible.length,
      step: 25,
      moreAttr: 'data-toolpage="model" data-toolaction="more"',
      lessAttr: 'data-toolpage="model" data-toolaction="less"',
      focusAttr: "data-tools-focus",
      focusMore: "model-more",
      focusLess: "model-less",
    },
  )}`;
}

/**
 * Renders the Tools page.
 * @returns {string}
 */
export function toolsPage() {
  if (!app.data) return '<div class="loading">Loading tools…</div>';
  resetToolCharts();
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

/**
 * Handles click interaction on the Tools / Hooks / Projects pages.
 * @param {MouseEvent} e
 * @returns {boolean}
 */
export function insightsClick(e) {
  if (!(e.target instanceof Element)) return false;
  const target = e.target.closest("[data-insight-open]");
  if (target instanceof HTMLElement) {
    const key = target.dataset.insightOpen ?? "";
    const projectsPayload = /** @type {ProjectsResponse | null} */ (app.data);
    app.expandedGroups.add(key);
    app.query = "";
    app.visible = Math.max(100, projectsPayload?.projects.length ?? 100);
    hooks.render();
    const row = [...($("page")?.querySelectorAll("[data-expand]") ?? [])].find(
      (el) => el instanceof HTMLElement && el.dataset.expand === key,
    );
    if (row instanceof HTMLElement) {
      row.focus({ preventScroll: true });
      row.scrollIntoView({ block: "center", behavior: "smooth" });
    }
    return true;
  }
  const metricButton = e.target.closest("[data-tool-metric]");
  if (metricButton instanceof HTMLElement) {
    toolMetric = metricButton.dataset.toolMetric === "errors" ? "errors" : "calls";
    hooks.render();
    return true;
  }
  const sortHeader = e.target.closest("[data-toolsort]");
  if (sortHeader instanceof HTMLElement) {
    const table = sortHeader.dataset.tooltable;
    const key = sortHeader.dataset.toolsort ?? "";
    const state = table === "model" ? toolModelSort : toolSort;
    /** @type {SortDirection} */
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
    hooks.render();
    return true;
  }
  const row = e.target.closest("[data-toolselect]");
  if (row instanceof HTMLElement) {
    const name = decodeURIComponent(row.dataset.toolselect ?? "");
    toolFilter = toolFilter === name ? "" : name;
    hooks.render();
    return true;
  }
  const pageButton = e.target.closest("[data-toolpage]");
  if (pageButton instanceof HTMLElement) {
    const table = pageButton.dataset.toolpage;
    const action = pageButton.dataset.toolaction;
    if (table === "model") {
      toolModelShown = action === "more" ? toolModelShown + 25 * 4 : 25;
    } else {
      toolShown = action === "more" ? toolShown + 20 * 4 : 20;
    }
    hooks.render();
    return true;
  }
  if (e.target.closest("[data-clear-tool]")) {
    toolFilter = "";
    hooks.render();
    return true;
  }
  return hooksClick(e.target);
}

/**
 * Handles dropdown selection changes on the Tools and Projects pages.
 * @param {Event} e
 * @returns {boolean}
 */
export function insightsChange(e) {
  if (
    !(e.target instanceof HTMLSelectElement) ||
    !["tool-filter", "insight-sort"].includes(e.target.id)
  ) {
    return false;
  }
  const id = e.target.id;
  if (id === "tool-filter") {
    toolFilter = e.target.value;
  } else {
    setInsightSort(/** @type {"cost" | "requests" | "tokens" | "last_seen"} */ (e.target.value));
  }
  hooks.render();
  $(id)?.focus({ preventScroll: true });
  return true;
}

let insightsBound = false;

/**
 * Binds the chart redraw listener for window width changes (idempotent).
 */
export function initInsights() {
  if (insightsBound) return;
  insightsBound = true;
  window.addEventListener("resize", wireToolsCharts);
}

export { hooksPage, projectsPage };
