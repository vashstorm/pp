/**
 * Traces detail components: SummaryStrip, toolbar, virtualized Transcript, tool aggregation and span
 * drawer.
 */

import { compact, fixedCost, h, icon, number, ratioPercent } from "./format.js";
import { app, getJSON } from "./state.js";
import {
  TRACE_KIND_COLORS,
  TRACE_LIST_H,
  TRACE_MODE_OPTIONS,
  TRACE_ROW_H,
  traceBridge,
  traceClock,
  traceDurationMs,
  traceElapsed,
  traceEstimatedCost,
  traceOffset,
  traceSelected,
  traceStore,
} from "./traces-state.js";
import { traceCenterOnSpan, traceDrawOverlays, traceTooltipHide } from "./traces-timeline.js";

/** @typedef {import("../../web-types/api.d.ts").RequestDetailResponse} RequestDetailResponse */
/** @typedef {import("../../web-types/api.d.ts").TraceDetailResponse} TraceDetailResponse */
/** @typedef {import("./traces-state.js").TraceSpan} TraceSpan */
/** @typedef {import("./traces-state.js").TraceState} TraceState */
/** @typedef {import("./traces-state.js").TraceTrack} TraceTrack */
/** @typedef {import("./traces-state.js").TraceTranscriptRow} TraceTranscriptRow */
/** @typedef {import("./traces-state.js").TraceViewModel} TraceViewModel */

/**
 * Renders the summary strip of compact metrics at the top of the Traces detail (9 compact metrics).
 * @param {TraceViewModel} view
 * @returns {string}
 */
export function traceSummaryHTML(view) {
  const summary = view.summary;
  /** @param {number | undefined} ms */
  const share = (ms) =>
    summary.wall_ms != null && summary.wall_ms > 0 && ms != null
      ? `${ratioPercent(ms / summary.wall_ms)} of wall`
      : undefined;
  const tiles = [
    { label: "Wall time", value: traceElapsed(summary.wall_ms) },
    { label: "Model time", value: traceElapsed(summary.model_ms), hint: share(summary.model_ms) },
    { label: "Tool time", value: traceElapsed(summary.tool_ms), hint: share(summary.tool_ms) },
    { label: "Idle", value: traceElapsed(summary.idle_ms), hint: share(summary.idle_ms) },
    {
      label: "Errors",
      value: number((summary.failed || 0) + (summary.aborted || 0)),
      hint: `${number(summary.failed)} failed · ${number(summary.aborted)} aborted`,
    },
    {
      label: "Requests",
      value: number(summary.requests),
      hint: `${number(summary.tool_calls)} tool calls`,
    },
    { label: "Agents", value: number(summary.agents) },
    { label: "Tokens", value: compact(summary.tokens) },
    {
      label: "Cost",
      value: traceEstimatedCost(summary.cost || 0, summary.unpriced || 0),
    },
  ];
  return `<div class="x-stat-grid x-rise" style="--stat-min:112px">${tiles
    .map(
      (tile) =>
        `<div class="x-stat" data-size="sm"><div class="x-stat-label">${tile.label}</div><div class="x-stat-value">${h(
          tile.value,
        )}</div><div class="x-stat-foot">${
          tile.hint ? `<span class="x-truncate">${h(tile.hint)}</span>` : ""
        }</div></div>`,
    )
    .join("")}</div>`;
}

/**
 * Renders the timeline axis mode, idle compression toggle, span search box and legend.
 * @param {TraceViewModel} view
 * @returns {string}
 */
export function traceToolbarHTML(view) {
  const mode = traceStore.state?.mode || "duration";
  const matches = view.matches.length;
  const index = traceStore.state ? traceStore.state.matchIndex % Math.max(matches, 1) : 0;
  const searching = app.query.trim() !== "";
  const legend = /** @type {const} */ ([
    ["model", "Model"],
    ["tool", "Tool"],
    ["subagent", "Agent"],
  ])
    .map(
      ([kind, label]) =>
        `<span class="traces-legend-item"><span class="x-swatch" style="background:var(${
          TRACE_KIND_COLORS[kind]
        })"></span>${label}</span>`,
    )
    .join("");
  return `<div class="traces-toolbar"><div class="x-segmented" data-size="sm" role="radiogroup" aria-label="Axis mode"><span class="x-segmented-thumb"></span>${TRACE_MODE_OPTIONS.map(
    (option) =>
      `<button type="button" class="x-segmented-option" role="radio" aria-checked="${
        option.value === mode
      }" data-active="${option.value === mode}" data-trace-mode="${
        option.value
      }" data-tt-focus="mode:${option.value}" title="${option.title}">${option.label}</button>`,
  ).join("")}</div>${
    mode === "duration"
      ? `<label class="traces-check"><input type="checkbox" data-trace-compress data-tt-focus="compress"${
          traceStore.state?.compress ? " checked" : ""
        }> Compress idle</label>`
      : ""
  }<div class="traces-search"><label class="x-search">${icon(
    "search",
    14,
  )}<input class="x-input" id="search" type="search" aria-label="Search spans" placeholder="Search spans…" value="${h(
    app.query,
  )}" style="width:200px"></label>${
    searching
      ? `<span class="x-num x-muted traces-match-count" data-trace-match-count>${
          matches === 0 ? "0" : `${index + 1}/${matches}`
        }</span><button type="button" class="x-btn" data-variant="ghost" data-size="sm" data-icon="true" data-trace-match="-1" aria-label="Previous match"${
          matches === 0 ? " disabled" : ""
        }><span class="traces-chevron-left">${icon("chevron", 14)}</span></button><button type="button" class="x-btn" data-variant="ghost" data-size="sm" data-icon="true" data-trace-match="1" aria-label="Next match"${
          matches === 0 ? " disabled" : ""
        }>${icon("chevron", 14)}</button>`
      : ""
  }</div><div class="traces-legend">${legend}</div></div>`;
}

/**
 * Renders the per-session tool duration aggregation card.
 * @returns {string}
 */
export function traceAggregatesHTML() {
  const payload = /** @type {TraceDetailResponse | null} */ (app.data);
  const stats = (payload?.tool_stats || []).slice();
  if (!stats.length) return "";
  const key = traceStore.toolSort?.key;
  const dir = traceStore.toolSort?.dir === "asc" ? 1 : -1;
  /** @type {Record<string, (row: TraceDetailResponse["tool_stats"][number]) => string | number>} */
  const valueMap = {
    tool: (row) => row.tool,
    calls: (row) => row.calls,
    errors: (row) => row.errors,
    total: (row) => row.total_ms,
    avg: (row) => (row.calls > 0 ? row.total_ms / row.calls : 0),
    max: (row) => row.max_ms,
  };
  const value = key ? valueMap[key] : undefined;
  if (value) {
    stats.sort((a, b) => {
      const av = value(a);
      const bv = value(b);
      return typeof av === "number" && typeof bv === "number"
        ? (av - bv) * dir
        : String(av).localeCompare(String(bv)) * dir;
    });
  }
  const max = Math.max(...stats.map((row) => row.total_ms), 0);
  const shown = stats.slice(0, traceStore.toolShown);
  /**
   * @param {string} field
   * @param {string} label
   * @param {"left" | "right"} [align]
   */
  const header = (field, label, align) => {
    const sorted = traceStore.toolSort?.key === field;
    const sortDir = traceStore.toolSort?.dir;
    return `<th data-trace-tools-sort="${field}" data-sortable="true" tabindex="0"${
      sorted ? ` data-sorted="${sortDir}"` : ""
    }${align === "right" ? ' data-align="right"' : ""} aria-sort="${
      sorted ? (sortDir === "asc" ? "ascending" : "descending") : ""
    }">${label}${
      sorted ? `<span class="x-sort-arrow">${sortDir === "asc" ? "↑" : "↓"}</span>` : ""
    }</th>`;
  };
  const more =
    stats.length > 12
      ? `<div class="x-table-more"><span class="x-micro">${number(
          Math.min(traceStore.toolShown, stats.length),
        )} of ${number(stats.length)}</span>${
          stats.length > traceStore.toolShown
            ? '<button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-trace-tools-more="more">Show more</button>'
            : ""
        }${
          traceStore.toolShown > 12
            ? '<button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-trace-tools-more="fewer">Show fewer</button>'
            : ""
        }</div>`
      : "";
  return `<section class="x-card x-rise" style="--i:3"><header class="x-card-header"><div class="x-card-titles"><h2 class="x-card-title">Tool aggregates</h2><p class="x-card-description">${number(
    stats.length,
  )} tools, by total time</p></div></header><div class="x-card-body" data-flush="true"><div class="x-table-wrap"><table class="x-table" data-dense="true"><thead><tr>${header(
    "tool",
    "Tool",
  )}${header("calls", "Calls", "right")}${header("errors", "Errors", "right")}${header(
    "total",
    "Total",
    "right",
  )}${header("avg", "Avg", "right")}${header("max", "Max", "right")}</tr></thead><tbody>${shown
    .map(
      (row) =>
        `<tr><td><span class="x-mono">${h(row.tool)}</span></td><td data-align="right"><span class="x-num">${number(
          row.calls,
        )}</span></td><td data-align="right">${
          row.errors > 0
            ? `<span class="x-badge" data-tone="bad">${number(row.errors)} failed</span>`
            : '<span class="x-num x-dim">0</span>'
        }</td><td data-align="right"><div class="x-meter-cell"><span class="x-num">${traceDurationMs(
          row.total_ms,
        )}</span><div class="x-meter"><div class="x-meter-fill" style="width:${
          max > 0 ? Math.min(100, (row.total_ms / max) * 100) : 0
        }%;background:var(--warn)"></div></div></div></td><td data-align="right"><span class="x-num">${traceDurationMs(
          row.calls > 0 ? row.total_ms / row.calls : 0,
        )}</span></td><td data-align="right"><span class="x-num">${traceDurationMs(
          row.max_ms,
        )}</span></td></tr>`,
    )
    .join("")}</tbody></table></div>${more}</div></section>`;
}

/**
 * @param {TraceSpan} span
 * @param {TraceTrack} track
 */
function traceDrawerMetrics(span, track) {
  /** @type {Array<[string, string]>} */
  const items = [
    [
      "Duration",
      `<span class="x-num">${traceDurationMs(span.end - span.start)}${
        span.unterminated ? '<span class="x-muted"> · unterminated</span>' : ""
      }</span>`,
    ],
    ["Start", `<span class="x-num">${traceClock(span.start)}</span>`],
    ["Track", `<span class="x-mono">${h(track?.label || "-")}</span>`],
  ];
  if (span.kind === "model") {
    items.push(["Tokens", `<span class="x-num">${number(span.tokens ?? 0)}</span>`]);
    items.push(["Cost", `<span class="x-num">${fixedCost(span.cost ?? 0, 4)}</span>`]);
    items.push(["TTFT", `<span class="x-num">${traceDurationMs(span.ttft)}</span>`]);
    items.push(["Model", `<span class="x-mono">${h(span.model || "-")}</span>`]);
    if (span.usageKnown) {
      items.push([
        "Input / output / cached",
        `<span class="x-num">${number(span.inputTokens ?? 0)} / ${number(
          span.outputTokens ?? 0,
        )} / ${number(span.cachedTokens ?? 0)}</span>`,
      ]);
    }
    if (span.provider) items.push(["Provider", `<span class="x-mono">${h(span.provider)}</span>`]);
    if (span.stopReason) {
      items.push(["Stop reason", `<span class="x-mono">${h(span.stopReason)}</span>`]);
    }
  } else if (span.kind === "tool") {
    items.push(["Status", `<span class="x-mono">${h(span.status)}</span>`]);
    items.push(["Tool", `<span class="x-mono">${h(span.tool)}</span>`]);
    items.push(["Call ID", `<span class="x-mono">${h(span.callId)}</span>`]);
    items.push([
      "Request",
      span.requestId
        ? `<button type="button" class="x-btn" data-size="sm" data-trace-open-request="${span.requestId}">#${span.requestId}</button>`
        : '<span class="x-dim">Not linked</span>',
    ]);
  } else if (span.kind === "subagent") {
    items.push(["Model", `<span class="x-mono">${h(span.model || "-")}</span>`]);
    const child = traceStore.view?.tracks.find((candidate) => candidate.id === span.childTrackId);
    items.push([
      "Requests",
      `<span class="x-num">${number(child?.spans.filter((s) => s.kind === "model").length || 0)}</span>`,
    ]);
    items.push([
      "Tools",
      `<span class="x-num">${number(child?.spans.filter((s) => s.kind === "tool").length || 0)}</span>`,
    ]);
  }
  return items;
}

/**
 * @param {TraceViewModel} view
 * @param {TraceState} state
 */
function traceDrawerHTML(view, state) {
  const selected = traceSelected(view, state);
  if (!state.drawer || !selected) return "";
  const { span, track } = selected;
  const detail =
    span.kind === "model" && span.requestId != null ? traceStore.details.get(span.requestId) : null;
  const pending = Boolean(detail?.pending);
  const metrics = traceDrawerMetrics(span, track);
  const errorMessage = span.errorMessage || detail?.data?.request?.error_message || null;
  let sections = "";
  if (errorMessage) {
    sections += `<div class="x-stack" style="gap:6px"><div class="section-label tone-bad">Error message</div><pre class="code-block traces-pre">${h(
      errorMessage,
    )}</pre></div>`;
  }
  if (span.kind === "subagent") {
    sections += `<div class="x-stack" style="gap:8px">${
      span.detail
        ? `<div class="section-label">Task</div><pre class="code-block traces-pre">${h(span.detail)}</pre>`
        : ""
    }${
      span.childTrackId
        ? `<div><button type="button" class="x-btn" data-size="sm" data-trace-open-child="${h(
            span.childTrackId,
          )}">Open child track <span class="x-mono">${h(span.childTrackId)}</span></button></div>`
        : ""
    }</div>`;
  }
  if (span.kind === "tool") {
    sections +=
      '<div class="x-stack" style="gap:6px"><div class="section-label">Args</div><div class="x-muted traces-note">Tool arguments and results are not recorded by pp.</div></div>';
  }
  if (span.kind === "model") {
    if (pending || !detail) {
      sections +=
        '<div class="x-stack traces-skeleton"><div class="x-skeleton" style="height:80px"></div><div class="x-skeleton" style="height:160px"></div></div>';
    } else if (detail.error) {
      sections += `<div class="traces-error" role="alert">${icon(
        "alert",
        15,
      )}<span>Failed to load request: ${h(detail.error)}</span></div>`;
    } else if (detail.data) {
      sections +=
        '<div class="x-stack" style="gap:6px"><div class="section-label">Message content</div><div class="x-muted traces-note">Text, thinking and tool arguments are not recorded by pp.</div></div>';
      sections +=
        '<details class="traces-json" data-trace-json><summary>Raw request</summary><pre class="code-block traces-pre"></pre></details>';
    }
  }
  return `<div class="trace-scrim" data-trace-close></div><aside class="trace-sheet" role="dialog" aria-modal="true" tabindex="-1" data-trace-sheet aria-label="Selected span details"><header class="trace-sheet-header"><div class="trace-sheet-titles"><h2 class="trace-sheet-title">${h(
    span.label,
  )}</h2><div class="trace-sheet-subtitle"><span class="x-badge" data-tone="${
    span.isError ? "bad" : "neutral"
  }">${span.isError ? `${span.kind} · error` : span.kind}</span></div></div><button type="button" class="x-btn" data-variant="ghost" data-icon="true" data-trace-copy aria-label="Copy raw JSON" title="Copy raw JSON">${icon(
    "copy",
    15,
  )}</button><button type="button" class="x-btn" data-variant="ghost" data-icon="true" data-trace-close aria-label="Close span details">${icon(
    "close",
    16,
  )}</button></header><div class="trace-sheet-body"><div class="x-stack traces-drawer" style="gap:16px"><dl class="kv">${metrics
    .map(
      ([label, value]) =>
        `<div class="kv-item"><dt class="kv-key">${label}</dt><dd class="kv-value">${value}</dd></div>`,
    )
    .join("")}</dl>${sections}</div></div></aside>`;
}

function traceDrawerHost() {
  let host = document.getElementById("trace-drawer-host");
  if (!host) {
    host = document.createElement("div");
    host.id = "trace-drawer-host";
    host.addEventListener("click", (event) => traceBridge.onClick(event));
    document.body.append(host);
  }
  return host;
}

export function traceRemoveDrawer() {
  const host = document.getElementById("trace-drawer-host");
  if (host) host.innerHTML = "";
  traceTooltipHide();
}

/**
 * Renders or refreshes the Span drawer.
 * @param {TraceViewModel} view
 * @param {TraceState} state
 */
export function traceRenderDrawer(view, state) {
  const host = traceDrawerHost();
  host.innerHTML = traceDrawerHTML(view, state);
  const sheet = host.querySelector("[data-trace-sheet]");
  if (sheet instanceof HTMLElement) {
    sheet.focus({ preventScroll: true });
    for (const details of sheet.querySelectorAll("[data-trace-json]")) {
      if (!(details instanceof HTMLDetailsElement)) continue;
      details.ontoggle = () => {
        if (!details.open) return;
        const span = traceSelected(view, state)?.span;
        const entry = span?.requestId != null ? traceStore.details.get(span.requestId) : null;
        const pre = details.querySelector("pre");
        if (pre instanceof HTMLElement && entry?.data && !pre.dataset.filled) {
          pre.textContent = JSON.stringify(entry.data.request ?? entry.data, null, 2);
          pre.dataset.filled = "true";
        }
      };
    }
  }
}

/**
 * Renders a single virtualized Transcript row.
 * @param {TraceTranscriptRow} row
 * @param {TraceViewModel} view
 * @param {TraceState} state
 */
function traceTranscriptRow(row, view, state) {
  const span = row.span;
  const marker = row.marker;
  const kind = span ? span.kind : marker ? "marker" : "";
  const color = span ? `var(${TRACE_KIND_COLORS[span.kind]})` : "var(--ink-1)";
  const label = span ? span.label : marker?.label || "";
  const detail = span ? span.detail : undefined;
  const durationText = span ? traceDurationMs(span.end - span.start) : "";
  const indent = `padding-left:${12 + row.depth * 12}px`;
  const content = `<span class="traces-kind" style="--traces-kind-color:${color}">${h(
    kind,
  )}</span>${
    row.depth > 0
      ? `<span class="traces-row-track x-mono x-truncate" title="${h(
          row.track.label,
        )}">${h(row.track.id)}</span>`
      : ""
  }<span class="traces-row-label x-truncate">${h(label)}</span>${
    detail ? `<span class="traces-row-detail x-truncate">${h(detail)}</span>` : ""
  }<span class="traces-row-meta x-num">${
    span?.isError ? '<span class="traces-error-tag">error</span>' : ""
  }<span>${durationText}</span><span>${traceOffset(row.time - view.start)}</span></span>`;
  const selected = Boolean(span && span.id === state.selection);
  return span
    ? `<button type="button" class="traces-row" data-trace-row="${h(
        span.id,
      )}" data-selected="${selected}" style="${indent}">${content}</button>`
    : `<div class="traces-row" data-static="true" style="${indent}">${content}</div>`;
}

/**
 * Renders the Transcript rows visible in the current scroll window.
 * @param {TraceViewModel} view
 * @param {TraceState} state
 */
export function traceRenderTranscriptWindow(view, state) {
  const list = /** @type {HTMLElement | null} */ (document.querySelector("[data-trace-list]"));
  if (!list) return;
  const spacer = /** @type {HTMLElement | null} */ (list.querySelector("[data-trace-spacer]"));
  const windowEl = /** @type {HTMLElement | null} */ (list.querySelector("[data-trace-window]"));
  if (!spacer || !windowEl) return;
  const rows = view.rows;
  spacer.style.height = `${rows.length * TRACE_ROW_H}px`;
  const top = list.scrollTop;
  const first = Math.max(0, Math.floor(top / TRACE_ROW_H) - 20);
  const last = Math.min(rows.length, Math.ceil((top + TRACE_LIST_H) / TRACE_ROW_H) + 20);
  windowEl.style.transform = `translateY(${first * TRACE_ROW_H}px)`;
  windowEl.innerHTML = rows
    .slice(first, last)
    .map((row) => traceTranscriptRow(row, view, state))
    .join("");
}

/**
 * @param {TraceViewModel} view
 * @param {TraceState} state
 */
function traceUpdateTranscriptSelection(view, state) {
  const list = /** @type {HTMLElement | null} */ (document.querySelector("[data-trace-list]"));
  if (!list) return;
  const rows = view.rows;
  const index = rows.findIndex((row) => row.span?.id === state.selection);
  if (index === -1) return;
  const top = index * TRACE_ROW_H;
  if (top < list.scrollTop || top + TRACE_ROW_H > list.scrollTop + TRACE_LIST_H) {
    list.scrollTop = Math.max(0, top - TRACE_LIST_H / 2);
    state.transcriptScroll = list.scrollTop;
    traceRenderTranscriptWindow(view, state);
  }
  for (const row of list.querySelectorAll("[data-trace-row]")) {
    if (row instanceof HTMLElement) {
      row.dataset.selected = String(row.dataset.traceRow === state.selection);
    }
  }
}

/**
 * Asynchronously loads the detail of a model request (used by the span drawer to show the Raw
 * request).
 * @param {number | null | undefined} id
 */
export function traceLoadRequestDetail(id) {
  if (!id || traceStore.details.has(id)) return;
  traceStore.details.set(id, { pending: true });
  getJSON(/** @type {string} */ (`/api/stats/requests/${id}`))
    .then((detail) =>
      traceStore.details.set(id, {
        data: /** @type {RequestDetailResponse} */ (detail),
      }),
    )
    .catch((/** @type {unknown} */ error) =>
      traceStore.details.set(id, {
        error: error instanceof Error ? error.message : String(error),
      }),
    )
    .finally(() => {
      if (app.page === "traces" && traceStore.request && traceStore.view && traceStore.state) {
        traceRenderDrawer(traceStore.view, traceStore.state);
      }
    });
}

/**
 * Selects the given span and opens or closes the right-side drawer accordingly.
 * @param {TraceViewModel} view
 * @param {TraceState} state
 * @param {string | null} id
 * @param {boolean} open
 */
export function traceSelect(view, state, id, open) {
  state.selection = id;
  state.drawer = Boolean(open && id);
  const selected = traceSelected(view, state);
  if (state.drawer && selected?.span.kind === "model") {
    traceLoadRequestDetail(selected.span.requestId);
  }
  traceRenderDrawer(view, state);
  if (state.layout) traceDrawOverlays(state, view, state.layout);
  traceUpdateTranscriptSelection(view, state);
  if (selected) traceCenterOnSpan(state, selected.span);
  if (!state.drawer) {
    const row = state.selection
      ? document.querySelector(`[data-trace-row="${CSS.escape(state.selection)}"]`)
      : null;
    const focusEl = row || document.querySelector("[data-trace-canvas]");
    if (focusEl instanceof HTMLElement) focusEl.focus({ preventScroll: true });
  }
}
