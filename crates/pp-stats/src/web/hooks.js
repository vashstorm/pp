/**
 * Hooks page: lifecycle Hook execution overview, time distribution, and stats tables by event and by
 * project.
 */

import {
  resetToolCharts,
  toolBucketAxis,
  toolBucketWord,
  toolCard,
  toolChart,
  toolLegend,
  toolRangeMeta,
  toolStat,
} from "./charts.js";
import { errorRateText, h, icon, localTime, number, ratioPercent, relative } from "./format.js";
import { app, hooks } from "./state.js";
import { renderSortHeader, sortRows, xEmpty, xMeter } from "./table.js";

/** @typedef {import("../../web-types/api.d.ts").HookEventRow} HookEventRow */
/** @typedef {import("../../web-types/api.d.ts").HookProjectRow} HookProjectRow */
/** @typedef {import("../../web-types/api.d.ts").HooksResponse} HooksResponse */
/** @typedef {import("../../web-types/api.d.ts").SortDirection} SortDirection */
/** @typedef {import("../../web-types/api.d.ts").SortState} SortState */

/** @type {SortState} */
let hookEventSort = { key: "runs", dir: "desc" };
/** @type {SortState} */
let hookProjectSort = { key: "runs", dir: "desc" };
/** @type {Set<string>} */
let hookExpandedEvents = new Set();

export function resetHooks() {
  hookEventSort = { key: "runs", dir: "desc" };
  hookProjectSort = { key: "runs", dir: "desc" };
  hookExpandedEvents = new Set();
}

/** @param {number | null | undefined} ms */
function hookDuration(ms) {
  if (ms == null) return "—";
  if (ms < 1000) return `${ms < 10 ? ms.toFixed(1) : Math.round(ms)}ms`;
  return `${(ms / 1000).toFixed(ms < 10000 ? 2 : 1)}s`;
}

/**
 * @param {number} count
 * @param {number} total
 * @param {number} warnThreshold
 * @param {number} badThreshold
 */
function hookRateBadge(count, total, warnThreshold, badThreshold) {
  if (!(total > 0)) return '<span class="x-num x-dim">0</span>';
  const rate = count / total;
  const tone =
    count === 0 ? "neutral" : rate >= badThreshold ? "bad" : rate >= warnThreshold ? "warn" : "ok";
  return `<span class="x-row" style="gap:8px;justify-content:flex-end"><span class="x-num x-dim">${number(count)}</span><span class="x-badge" data-tone="${tone}" data-mono="true">${errorRateText(rate)}</span></span>`;
}

/** @param {HookEventRow} eventRow */
function hookEventToolsDetail(eventRow) {
  const tools = eventRow.tools || [];
  const maxRuns = tools.reduce((m, t) => Math.max(m, t.runs || 0), 0);
  const body = tools
    .map(
      (tool) => `<tr>
        <td><span class="x-mono">${h(tool.tool)}</span></td>
        <td data-align="right">${xMeter(tool.runs, maxRuns, `${number(tool.runs)}<span class="x-dim x-tools-share">${ratioPercent(eventRow.runs > 0 ? tool.runs / eventRow.runs : 0, 0)}</span>`)}</td>
        <td data-align="right"><span class="x-num">${number(tool.ok)}</span></td>
        <td data-align="right">${hookRateBadge(tool.blocked, tool.runs, 0.01, 0.25)}</td>
        <td data-align="right">${hookRateBadge(tool.errors, tool.runs, 0.01, 0.05)}</td>
        <td data-align="right"><span class="x-num">${number(tool.modified)}</span></td>
        <td data-align="right"><span class="x-num">${hookDuration(tool.avg_duration_ms)}</span></td>
        <td data-align="right"><span class="x-num x-muted">${hookDuration(tool.max_duration_ms)}</span></td>
        <td data-align="right"><span class="x-num x-muted">${hookDuration(tool.total_duration_ms)}</span></td>
      </tr>`,
    )
    .join("");
  return `<div class="x-providers-mix" style="padding:10px 16px 12px 38px">
    <div class="x-section-label">By matched tool (${number(tools.length)})</div>
    <div class="x-table-wrap"><table class="x-table" data-dense="true"><thead><tr>
      <th>Tool</th><th data-align="right">Runs</th><th data-align="right">Allowed</th><th data-align="right">Blocked</th><th data-align="right">Errors</th><th data-align="right">Modified</th><th data-align="right">Avg</th><th data-align="right">Max</th><th data-align="right">Total</th>
    </tr></thead><tbody>${body}</tbody></table></div>
  </div>`;
}

/**
 * @param {HookEventRow[]} events
 * @param {number} totalRuns
 */
function hookEventTable(events, totalRuns) {
  if (!events.length) {
    return xEmpty(
      "No hook executions in this range",
      "Hook runs appear here once configured command hooks execute.",
    );
  }
  const maxRuns = events.reduce((m, e) => Math.max(m, e.runs || 0), 0);
  /** @type {import("./table.js").TableColumn<HookEventRow>[]} */
  const columns = [
    {
      key: "event",
      header: "Event",
      sort: (row) => row.event,
      render: (row) => {
        const hasTools = (row.tools?.length || 0) > 0;
        const open = hasTools && hookExpandedEvents.has(row.event);
        const chev = hasTools
          ? `<span class="x-models-chevron" data-open="${open}">${icon("chevron", 12)}</span>`
          : '<span style="display:inline-block;width:12px"></span>';
        const badge = hasTools
          ? `<span class="x-badge">${number(row.tools.length)} tools</span>`
          : "";
        return `<span class="x-row" style="gap:8px">${chev}<span class="x-mono x-cell-primary">${h(row.event)}</span>${badge}</span>`;
      },
    },
    {
      key: "runs",
      header: "Runs",
      align: "right",
      sort: (row) => row.runs || 0,
      render: (row) =>
        xMeter(
          row.runs,
          maxRuns,
          `${number(row.runs)}<span class="x-dim x-tools-share">${ratioPercent(totalRuns > 0 ? row.runs / totalRuns : 0, 0)}</span>`,
        ),
    },
    {
      key: "ok",
      header: "Allowed",
      align: "right",
      sort: (row) => row.ok || 0,
      render: (row) => `<span class="x-num">${number(row.ok)}</span>`,
    },
    {
      key: "blocked",
      header: "Blocked",
      title: "Handlers that returned deny, block, continue:false, or exit code 2",
      align: "right",
      sort: (row) => row.blocked || 0,
      render: (row) => hookRateBadge(row.blocked, row.runs, 0.01, 0.25),
    },
    {
      key: "errors",
      header: "Errors",
      title: "Handlers that timed out, exited abnormally, or produced invalid JSON",
      align: "right",
      sort: (row) => row.errors || 0,
      render: (row) => hookRateBadge(row.errors, row.runs, 0.01, 0.05),
    },
    {
      key: "modified",
      header: "Modified",
      title: "Runs that rewrote tool input/output or injected context",
      align: "right",
      sort: (row) => row.modified || 0,
      render: (row) => `<span class="x-num">${number(row.modified)}</span>`,
    },
    {
      key: "avg",
      header: "Avg time",
      align: "right",
      sort: (row) => row.avg_duration_ms ?? -1,
      render: (row) => `<span class="x-num">${hookDuration(row.avg_duration_ms)}</span>`,
    },
    {
      key: "max",
      header: "Max time",
      align: "right",
      sort: (row) => row.max_duration_ms ?? -1,
      render: (row) => `<span class="x-num x-muted">${hookDuration(row.max_duration_ms)}</span>`,
    },
    {
      key: "total",
      header: "Total time",
      align: "right",
      sort: (row) => row.total_duration_ms || 0,
      render: (row) => `<span class="x-num x-muted">${hookDuration(row.total_duration_ms)}</span>`,
    },
  ];
  const sorted = sortRows(events, columns, hookEventSort);
  const head = columns
    .map((column) =>
      renderSortHeader(column, hookEventSort, {
        sortAttr: "data-hooksort",
        tableAttr: "data-hooktable",
        table: "event",
        alignAttr: "data-hookalign",
        focusAttr: "data-hooks-focus",
        focusPrefix: "sort-event-",
      }),
    )
    .join("");
  const body = sorted
    .map((row) => {
      const hasTools = (row.tools?.length || 0) > 0;
      const open = hasTools && hookExpandedEvents.has(row.event);
      const attrs = hasTools
        ? ` data-clickable="true" data-selected="${open}" data-hookevent="${h(row.event)}" data-hooks-focus="event-${h(row.event)}" tabindex="0"`
        : "";
      const cells = columns
        .map((column) => `<td data-align="${column.align || "left"}">${column.render(row)}</td>`)
        .join("");
      const detail = open
        ? `<tr><td colspan="${columns.length}" data-wrap="true" style="padding:0">${hookEventToolsDetail(row)}</td></tr>`
        : "";
      return `<tr${attrs}>${cells}</tr>${detail}`;
    })
    .join("");
  return `<div class="x-table-wrap"><table class="x-table" data-dense="true"><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>`;
}

/** @param {HookProjectRow[]} projects */
function hookProjectTable(projects) {
  if (!projects.length) return xEmpty("No project hook activity in this range");
  const maxRuns = projects.reduce((m, p) => Math.max(m, p.runs || 0), 0);
  /** @type {import("./table.js").TableColumn<HookProjectRow>[]} */
  const columns = [
    {
      key: "project",
      header: "Project",
      sort: (row) => row.project || row.key || "",
      render: (row) =>
        `<div class="x-stack" style="gap:0"><span class="x-cell-primary x-truncate">${h(row.project || row.key || "Unknown directory")}</span><span class="x-cell-secondary x-mono x-truncate" title="${h(row.key)}">${h(row.key)}</span></div>`,
    },
    {
      key: "runs",
      header: "Runs",
      align: "right",
      sort: (row) => row.runs || 0,
      render: (row) => xMeter(row.runs, maxRuns, number(row.runs)),
    },
    {
      key: "blocked",
      header: "Blocked",
      align: "right",
      sort: (row) => row.blocked || 0,
      render: (row) => hookRateBadge(row.blocked, row.runs, 0.01, 0.25),
    },
    {
      key: "errors",
      header: "Errors",
      align: "right",
      sort: (row) => row.errors || 0,
      render: (row) => hookRateBadge(row.errors, row.runs, 0.01, 0.05),
    },
    {
      key: "modified",
      header: "Modified",
      align: "right",
      sort: (row) => row.modified || 0,
      render: (row) => `<span class="x-num">${number(row.modified)}</span>`,
    },
    {
      key: "events",
      header: "Events",
      align: "right",
      sort: (row) => row.events || 0,
      render: (row) => `<span class="x-num">${number(row.events)}</span>`,
    },
    {
      key: "sessions",
      header: "Sessions",
      align: "right",
      sort: (row) => row.sessions || 0,
      render: (row) => `<span class="x-num">${number(row.sessions)}</span>`,
    },
    {
      key: "avg",
      header: "Avg time",
      align: "right",
      sort: (row) => row.avg_duration_ms ?? -1,
      render: (row) => `<span class="x-num">${hookDuration(row.avg_duration_ms)}</span>`,
    },
    {
      key: "lastSeen",
      header: "Last run",
      align: "right",
      sort: (row) => row.last_seen ?? -1,
      render: (row) =>
        `<span class="x-muted" title="${row.last_seen != null ? h(localTime(row.last_seen)) : ""}">${row.last_seen == null ? "—" : relative(row.last_seen)}</span>`,
    },
  ];
  const sorted = sortRows(projects, columns, hookProjectSort);
  const head = columns
    .map((column) =>
      renderSortHeader(column, hookProjectSort, {
        sortAttr: "data-hooksort",
        tableAttr: "data-hooktable",
        table: "project",
        alignAttr: "data-hookalign",
        focusAttr: "data-hooks-focus",
        focusPrefix: "sort-project-",
      }),
    )
    .join("");
  const body = sorted
    .map(
      (row) =>
        `<tr>${columns.map((column) => `<td data-align="${column.align || "left"}">${column.render(row)}</td>`).join("")}</tr>`,
    )
    .join("");
  return `<div class="x-table-wrap"><table class="x-table" data-dense="true"><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>`;
}

/**
 * Renders the Hooks page.
 * @returns {string}
 */
export function hooksPage() {
  const payload = /** @type {HooksResponse | null} */ (app.data);
  if (!payload) return '<div class="loading">Loading hooks…</div>';
  resetToolCharts();
  const s = payload.summary;
  const meta = toolRangeMeta();
  const buckets = toolBucketAxis((payload.buckets || []).map((b) => b.timestamp));
  const byTime = new Map((payload.buckets || []).map((b) => [b.timestamp, b]));
  const totalRuns = buckets.map((t) => byTime.get(t)?.runs ?? 0);
  const totalBlocked = buckets.map((t) => byTime.get(t)?.blocked ?? 0);
  const totalErrors = buckets.map((t) => byTime.get(t)?.errors ?? 0);
  const totalOk = buckets.map((t) => byTime.get(t)?.ok ?? 0);
  const series = [
    { key: "ok", label: "Allowed", color: "var(--ok)", values: totalOk.map((v) => v || null) },
    {
      key: "blocked",
      label: "Blocked",
      color: "var(--warn)",
      values: totalBlocked.map((v) => v || null),
    },
    {
      key: "errors",
      label: "Errors",
      color: "var(--bad)",
      values: totalErrors.map((v) => v || null),
    },
  ];
  const runs = s.runs || 0;
  const blockRate = runs > 0 ? errorRateText((s.blocked || 0) / runs) : "—";
  const errRate = runs > 0 ? errorRateText((s.errors || 0) / runs) : "—";
  const modRate = runs > 0 ? errorRateText((s.modified || 0) / runs) : "—";
  const statGrid = `<div class="x-stat-grid x-rise" style="--stat-min:175px">
    ${toolStat(
      "Hook runs",
      number(runs),
      `${number(s.ok || 0)} allowed · ${number(s.sessions || 0)} sessions`,
      {
        spark: totalRuns,
      },
    )}
    ${toolStat("Blocked", number(s.blocked || 0), `${blockRate} block rate`, {
      title: "Handlers that returned deny, block, continue:false, or exit code 2",
      spark: totalBlocked,
      sparkColor: "var(--warn)",
    })}
    ${toolStat(
      "Handler errors",
      errRate,
      `${number(s.errors || 0)} errors · ${number(runs - (s.errors || 0))} completed`,
      {
        title:
          "Handlers that failed due to timeout, abnormal exit code, or invalid protocol output",
        spark: totalErrors,
        sparkColor: "var(--bad)",
      },
    )}
    ${toolStat("Modified", number(s.modified || 0), `${modRate} rewrite / context`, {
      title: "Handler runs that rewrote tool input/output or injected additionalContext",
    })}
    ${toolStat(
      "Avg duration",
      hookDuration(s.avg_duration_ms),
      `${hookDuration(s.total_duration_ms)} total · max ${hookDuration(s.max_duration_ms)}`,
      {
        title: "Elapsed wall-clock time per handler execution",
      },
    )}
  </div>`;
  const chart = toolChart(
    { buckets, bucketMs: meta.bucketMs },
    series,
    "No hook runs in this range",
    {
      extraRows: (index) => {
        const b = byTime.get(buckets[index]);
        if (!b?.runs) return "";
        return `<div class="x-chart-tooltip-row"><span class="x-chart-tooltip-label">Modified</span><span class="x-chart-tooltip-value">${number(b.modified)}</span></div><div class="x-chart-tooltip-row"><span class="x-chart-tooltip-label">Avg time</span><span class="x-chart-tooltip-value">${hookDuration(b.avg_duration_ms)}</span></div>`;
      },
    },
  );
  const chartBody = `<div class="x-stack" style="gap:12px">${chart}${toolLegend(series, number, { focusAttribute: "data-hooks-focus" })}</div>`;
  return `<div class="x-page">
    <header class="x-page-header x-rise"><div><h1 class="x-page-title">Hooks</h1><p class="x-page-description">Lifecycle command hook executions, decisions, rewrites, and latency in ${h(meta.windowLabel)}.</p></div></header>
    ${statGrid}
    ${toolCard(1, "Hook activity over time", `Per ${toolBucketWord(meta.bucketMs)}, stacked by handler outcome`, "", chartBody)}
    ${toolCard(2, "By hook event", "Select a tool event row to break executions down by matched tool", "", hookEventTable(payload.events || [], runs), true)}
    ${toolCard(3, "By project", "Directories where hooks ran", "", hookProjectTable(payload.projects || []), true)}
  </div>`;
}

/**
 * Handles sorting and event-row expansion clicks on the Hooks page.
 * @param {Element} target
 * @returns {boolean}
 */
export function hooksClick(target) {
  const hookSortHeader = target.closest("[data-hooksort]");
  if (hookSortHeader instanceof HTMLElement) {
    const table = hookSortHeader.dataset.hooktable;
    const key = hookSortHeader.dataset.hooksort ?? "";
    const state = table === "project" ? hookProjectSort : hookEventSort;
    /** @type {SortDirection} */
    const direction =
      state.key === key
        ? state.dir === "asc"
          ? "desc"
          : "asc"
        : hookSortHeader.dataset.hookalign === "right"
          ? "desc"
          : "asc";
    if (table === "project") hookProjectSort = { key, dir: direction };
    else hookEventSort = { key, dir: direction };
    hooks.render();
    return true;
  }
  const hookEventRow = target.closest("[data-hookevent]");
  if (hookEventRow instanceof HTMLElement) {
    const name = hookEventRow.dataset.hookevent ?? "";
    if (hookExpandedEvents.has(name)) hookExpandedEvents.delete(name);
    else hookExpandedEvents.add(name);
    hooks.render();
    return true;
  }
  return false;
}
