/**
 * Traces 页：按根会话折叠子 Agent 的会话列表，以及单会话时间线阅读器入口。
 * 结构、交互与配色对齐 omp stats 的 #/traces；数据来自 pp stats 自己的快照。
 */

import { pricingNotice } from "./web/analytics-shared.js";
import { $, compact, h, icon, localTime, number, relative } from "./web/format.js";
import { app, hooks } from "./web/state.js";
import {
  traceAggregatesHTML,
  traceRemoveDrawer,
  traceRenderDrawer,
  traceRenderTranscriptWindow,
  traceSelect,
  traceSummaryHTML,
  traceToolbarHTML,
} from "./web/traces-drawer.js";
import {
  TRACE_LIST_LIMIT,
  traceBegin,
  traceBridge,
  traceElapsed,
  traceEstimatedCost,
  traceFileStem,
  traceFolderTail,
  traceRememberScroll,
  traceSelected,
  traceStore,
} from "./web/traces-state.js";
import {
  traceBounds,
  traceLayout,
  traceRedraw,
  traceRenderGutter,
  traceScale,
  traceTooltipHide,
  traceWireCanvas,
} from "./web/traces-timeline.js";

/** @typedef {import("../web-types/api.d.ts").SortDirection} SortDirection */
/** @typedef {import("../web-types/api.d.ts").TraceDetailResponse} TraceDetailResponse */
/** @typedef {import("../web-types/api.d.ts").TraceSessionSummary} TraceSessionSummary */
/** @typedef {import("../web-types/api.d.ts").TracesListResponse} TracesListResponse */
/** @typedef {import("./web/traces-state.js").TraceSpan} TraceSpan */
/** @typedef {import("./web/traces-state.js").TraceTrack} TraceTrack */
/** @typedef {import("./web/traces-state.js").TraceTranscriptRow} TraceTranscriptRow */
/** @typedef {import("./web/traces-state.js").TraceViewModel} TraceViewModel */

/**
 * 返回当前请求中的 trace 会话请求 ID（列表视图时为 null）。
 * @returns {string | null}
 */
export function getTraceRequest() {
  return traceStore.request;
}

/**
 * 返回上次渲染的 trace 数据签名。
 * @returns {string}
 */
export function getTraceRenderedSignature() {
  return traceStore.renderedSignature;
}

/**
 * 根据 URL 查询参数重置 Traces 视图状态。
 * @param {URL} url
 */
export function resetTraces(url) {
  const session = url.searchParams.get("session");
  const request = url.searchParams.get("request");
  const candidate = session || request || "";
  traceStore.request = /^[1-9]\d*$/.test(candidate) ? candidate : null;
  traceStore.sort = { key: "started", dir: "desc" };
  traceStore.shown = TRACE_LIST_LIMIT;
  traceStore.toolSort = null;
  traceStore.toolShown = 12;
  traceStore.state = null;
  traceStore.view = null;
  traceStore.renderedSignature = "";
  traceStore.focusKey = null;
  traceStore.details.clear();
  traceRemoveDrawer();
}

/**
 * 计算 Traces 列表或详情载荷的稳定签名，供 5 秒轮询判断是否跳过重绘。
 * @param {TracesListResponse | TraceDetailResponse | null | undefined} payload
 * @param {string} [range]
 * @returns {string}
 */
export function traceSignature(payload, range = app.range) {
  if (!payload) return "";
  if ("sessions" in payload && Array.isArray(payload.sessions)) {
    const sessions = payload.sessions;
    return [
      "list",
      range,
      payload.total,
      sessions.length,
      sessions[0]?.id ?? 0,
      sessions.at(-1)?.id ?? 0,
      payload.has_more ? 1 : 0,
      sessions.map((s) => s.title || "").join("\u0000"),
    ].join(":");
  }
  const detail = /** @type {TraceDetailResponse} */ (payload);
  const s = detail.summary || {};
  const requests = detail.requests || [];
  const tools = detail.tool_calls || [];
  return [
    "trace",
    range,
    detail.session_file,
    detail.title,
    requests.length,
    requests.at(-1)?.id ?? 0,
    tools.length,
    tools.at(-1)?.id ?? 0,
    s.requests,
    s.wall_ms,
    s.model_ms,
    s.tool_ms,
    s.tool_calls,
    s.tokens,
    s.cost,
    s.failed,
    s.aborted,
  ].join(":");
}

/** @param {TraceSessionSummary["models"]} models */
function traceModelLabel(models) {
  if (!models?.length) return '<span class="x-dim">-</span>';
  const keys = models.map((model) => model.key);
  return `<span class="traces-models" title="${h(keys.join("\n"))}"><span class="x-mono x-truncate">${h(
    models[0].model,
  )}</span>${models.length > 1 ? `<span class="x-badge">+${models.length - 1}</span>` : ""}</span>`;
}

function traceListRows() {
  const payload = /** @type {TracesListResponse} */ (app.data);
  const sessions = payload.sessions || [];
  const needle = app.query.trim().toLowerCase();
  const rows = sessions.filter(
    (s) =>
      !needle ||
      [
        s.title,
        s.session_file,
        s.project,
        s.cwd,
        ...(s.models || []).map((model) => model.key),
      ].some((value) =>
        String(value || "")
          .toLowerCase()
          .includes(needle),
      ),
  );
  /** @type {Record<string, (s: TraceSessionSummary) => number>} */
  const numeric = {
    started: (s) => s.first_seen ?? 0,
    duration: (s) => (s.last_seen ?? 0) - (s.first_seen ?? 0),
    requests: (s) => s.requests,
    tools: (s) => s.tool_calls,
    agents: (s) => s.agents,
    tokens: (s) => s.tokens ?? 0,
    cost: (s) => s.cost ?? 0,
  };
  const sortFn = numeric[traceStore.sort.key];
  if (sortFn) {
    rows.sort((a, b) => (sortFn(a) - sortFn(b)) * (traceStore.sort.dir === "asc" ? 1 : -1));
  } else if (traceStore.sort.key === "session") {
    rows.sort(
      (a, b) =>
        String(a.title || a.session_file || "").localeCompare(
          String(b.title || b.session_file || ""),
        ) * (traceStore.sort.dir === "asc" ? 1 : -1),
    );
  } else if (traceStore.sort.key === "models") {
    rows.sort(
      (a, b) =>
        String(a.models?.[0]?.model || "").localeCompare(String(b.models?.[0]?.model || "")) *
        (traceStore.sort.dir === "asc" ? 1 : -1),
    );
  }
  return rows;
}

/**
 * @param {string} key
 * @param {string} label
 * @param {"left" | "right"} [align]
 * @param {string | number} [width]
 */
function traceSortHeader(key, label, align, width) {
  const sorted = traceStore.sort.key === key;
  return `<th data-ttsort="${key}" data-ttalign="${align || "left"}" data-sortable="true" tabindex="0"${
    sorted ? ` data-sorted="${traceStore.sort.dir}"` : ""
  }${align === "right" ? ' data-align="right"' : ""}${
    width ? ` style="width:${width}"` : ""
  } aria-sort="${sorted ? (traceStore.sort.dir === "asc" ? "ascending" : "descending") : ""}">${label}${
    sorted ? `<span class="x-sort-arrow">${traceStore.sort.dir === "asc" ? "↑" : "↓"}</span>` : ""
  }</th>`;
}

/** @param {TraceSessionSummary} session */
function traceSessionCell(session) {
  const file = session.session_file || "";
  const name = file ? traceFileStem(file) : `Request #${session.id}`;
  const title = session.title || name;
  return `<div class="x-stack" style="gap:0"><span class="x-cell-primary x-truncate" title="${h(
    file || title,
  )}">${h(title)}</span><span class="x-cell-secondary x-truncate x-mono">${h(
    traceFolderTail(session.cwd),
  )}</span></div>`;
}

/**
 * @param {number} value
 * @param {number} max
 * @param {number} unpriced
 */
function traceCostCell(value, max, unpriced) {
  const pct = max > 0 ? Math.min(100, (value / max) * 100) : 0;
  return `<div class="x-meter-cell"><span class="x-num">${traceEstimatedCost(
    value,
    unpriced,
  )}</span><div class="x-meter"><div class="x-meter-fill" style="width:${pct}%"></div></div></div>`;
}

function traceListPage() {
  const payload = /** @type {TracesListResponse} */ (app.data);
  traceRememberScroll();
  traceRemoveDrawer();
  traceStore.renderedSignature = traceSignature(payload);
  const rows = traceListRows();
  const total = (payload.sessions || []).length;
  const maxCost = Math.max(0, ...rows.map((row) => row.cost || 0));
  const shown = rows.slice(0, traceStore.shown);
  const header =
    '<header class="x-page-header x-rise"><div><h1 class="x-page-title">Traces</h1><p class="x-page-description">Recent sessions with subagent activity folded in. Open one to inspect its timeline.</p></div></header>';
  const description = payload.sessions
    ? app.query.trim()
      ? `${number(rows.length)} of ${number(total)} most recent`
      : `${number(total)} most recent`
    : undefined;
  const search = `<label class="x-search traces-filter">${icon("search", 14)}<input class="x-input" id="search" type="search" aria-label="Filter sessions" placeholder="Filter by summary, project, model…" value="${h(
    app.query,
  )}" style="width:260px"></label>`;
  const body = rows.length
    ? `<div class="x-table-wrap"><table class="x-table traces-sessions"><thead><tr>${traceSortHeader(
        "session",
        "Session",
        "left",
        "32%",
      )}${traceSortHeader("models", "Models")}${traceSortHeader(
        "started",
        "Started",
        "right",
      )}${traceSortHeader("duration", "Duration", "right")}${traceSortHeader(
        "requests",
        "Requests",
        "right",
      )}${traceSortHeader("tools", "Tools", "right")}${traceSortHeader(
        "agents",
        "Agents",
        "right",
      )}${traceSortHeader("tokens", "Tokens", "right")}${traceSortHeader(
        "cost",
        "Cost",
        "right",
        150,
      )}</tr></thead><tbody>${shown
        .map(
          (session) =>
            `<tr data-trace-open="${session.id}" data-clickable="true" tabindex="0" aria-label="Open session ${h(
              session.title || traceFileStem(session.session_file) || session.id,
            )}"><td>${traceSessionCell(session)}</td><td>${traceModelLabel(
              session.models,
            )}</td><td><span class="x-muted" title="${
              session.first_seen != null ? h(localTime(session.first_seen)) : ""
            }">${session.first_seen == null ? "—" : relative(session.first_seen)}</span></td><td data-align="right"><span class="x-num">${traceElapsed(
              (session.last_seen ?? 0) - (session.first_seen ?? 0),
            )}</span></td><td data-align="right"><span class="x-num">${number(
              session.requests,
            )}</span></td><td data-align="right"><span class="x-num">${number(
              session.tool_calls,
            )}</span></td><td data-align="right"><span class="x-num${
              session.agents ? "" : " x-dim"
            }">${number(session.agents)}</span></td><td data-align="right"><span class="x-num">${compact(
              session.tokens,
            )}</span></td><td data-align="right">${traceCostCell(
              session.cost || 0,
              maxCost,
              session.unpriced || 0,
            )}</td></tr>`,
        )
        .join("")}</tbody></table></div>`
    : `<div class="x-empty">${icon("inbox", 20)}<div class="x-empty-title">${
        app.query.trim() ? "No matching sessions" : "No sessions found"
      }</div><div class="x-empty-hint">${
        app.query.trim() ? "Try a different filter." : "Run a pp session to record activity."
      }</div></div>`;
  const more =
    rows.length > TRACE_LIST_LIMIT
      ? `<div class="x-table-more"><span class="x-micro">${number(
          Math.min(traceStore.shown, rows.length),
        )} of ${number(rows.length)}</span>${
          rows.length > traceStore.shown
            ? '<button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-trace-more="more">Show more</button>'
            : ""
        }${
          traceStore.shown > TRACE_LIST_LIMIT
            ? '<button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-trace-more="fewer">Show fewer</button>'
            : ""
        }</div>`
      : "";
  const note = payload.has_more
    ? `<div class="load-note"><span>Loaded the latest ${number(
        payload.limit,
      )} sessions in this range. Narrow the range to inspect older sessions.</span>${
        app.limit === 500
          ? '<button class="outline-button" data-load>Load latest 2,000</button>'
          : ""
      }</div>`
    : "";
  return `<div class="x-page traces-page">${header}<section class="x-card x-rise" style="--i:0"><header class="x-card-header"><div class="x-card-titles"><h2 class="x-card-title">Sessions</h2>${
    description ? `<p class="x-card-description">${h(description)}</p>` : ""
  }</div><div class="x-card-actions">${search}</div></header><div class="x-card-body" data-flush="true">${body}${more}</div></section>${note}${pricingNotice()}</div>`;
}

/**
 * 根据单条根会话详情载荷构建轨道、span 与 Transcript 视图模型。
 * @param {TraceDetailResponse} [payload]
 * @param {string} [needle]
 * @returns {TraceViewModel}
 */
export function traceViewModel(
  payload = /** @type {TraceDetailResponse} */ (app.data),
  needle = app.query.trim().toLowerCase(),
) {
  const root = payload.session_file || "";
  const requests = (payload.requests || [])
    .slice()
    .sort((a, b) => a.timestamp - b.timestamp || a.id - b.id);
  const toolCalls = (payload.tool_calls || [])
    .slice()
    .sort((a, b) => a.timestamp - b.timestamp || a.id - b.id);
  /** @type {Map<string, number>} */
  const starts = new Map();
  /**
   * @param {string | null | undefined} file
   * @param {number} time
   */
  const noteStart = (file, time) => {
    if (!file) return;
    const current = starts.get(file);
    if (current === undefined || time < current) starts.set(file, time);
  };
  for (const request of requests) noteStart(request.session_file, request.timestamp);
  for (const tool of toolCalls) noteStart(tool.session_file, tool.timestamp);
  const files = [...new Set([...starts.keys()])];
  const children = files
    .filter((file) => file !== root)
    .sort((a, b) => (starts.get(a) ?? 0) - (starts.get(b) ?? 0));
  /** @type {TraceTrack[]} */
  const tracks = [
    {
      id: "main",
      label: "Main",
      file: root,
      model: requests.find((request) => request.session_file === root)?.model || "",
      depth: 0,
      spans: [],
      markers: [],
    },
  ];
  const byFile = new Map([[root, tracks[0]]]);
  children.forEach((file, index) => {
    /** @type {TraceTrack} */
    const track = {
      id: `main/${index + 1}`,
      label: `subagent ${index + 1}`,
      file,
      model: requests.find((request) => request.session_file === file)?.model || "",
      depth: 1,
      parentId: "main",
      spans: [],
      markers: [],
    };
    tracks.push(track);
    byFile.set(file, track);
  });
  for (const request of requests) {
    const track =
      (request.session_file ? byFile.get(request.session_file) : undefined) || tracks[0];
    const known = request.input_tokens != null || request.output_tokens != null;
    track.spans.push({
      id: `r${request.id}`,
      kind: "model",
      track: track.id,
      start: request.timestamp,
      end: request.timestamp + Math.max(0, request.duration_ms || 0),
      label: request.model,
      detail: request.purpose,
      isError: request.status !== "ok",
      status: request.status,
      requestId: request.id,
      model: request.model,
      provider: request.provider,
      tokens: known ? (request.input_tokens || 0) + (request.output_tokens || 0) : undefined,
      cost: request.cost,
      ttft: request.ttft_ms,
      stopReason: request.stop_reason,
      errorMessage: request.error_message,
      inputTokens: request.input_tokens,
      outputTokens: request.output_tokens,
      cachedTokens: request.cached_tokens,
      usageKnown: known,
    });
    if (request.status !== "ok") {
      track.markers.push({
        time: request.timestamp,
        label: `${request.status} · ${request.model}`,
        detail: request.error_message || "",
      });
    }
  }
  for (const tool of toolCalls) {
    const track = byFile.get(tool.session_file) || tracks[0];
    track.spans.push({
      id: `t${tool.id}`,
      kind: "tool",
      track: track.id,
      start: tool.timestamp,
      end: tool.timestamp + Math.max(0, tool.duration_ms || 0),
      label: tool.tool,
      detail: tool.error_message || "",
      isError: tool.status === "error",
      status: tool.status,
      tool: tool.tool,
      callId: tool.call_id,
      requestId: tool.request_id,
      errorMessage: tool.error_message,
    });
  }
  for (const track of tracks.slice(1)) {
    if (!track.spans.length) continue;
    const start = Math.min(...track.spans.map((span) => span.start));
    const end = Math.max(...track.spans.map((span) => span.end));
    tracks[0].spans.push({
      id: `s:${track.file}`,
      kind: "subagent",
      track: "main",
      start,
      end,
      label: track.label,
      detail: "",
      model: track.model,
      childTrackId: track.id,
      isError: track.spans.some((span) => span.isError),
    });
  }
  for (const track of tracks) {
    track.spans.sort((a, b) => a.start - b.start || a.end - b.end);
    track.markers.sort((a, b) => a.time - b.time);
  }
  /** @type {Array<{ span: TraceSpan, track: TraceTrack }>} */
  const spans = [];
  for (const track of tracks) for (const span of track.spans) spans.push({ span, track });
  spans.sort((a, b) => a.span.start - b.span.start);
  const start = traceBounds(tracks)[0];
  const matches = needle
    ? spans.filter(({ span }) =>
        `${span.label} ${span.detail ?? ""}`.toLowerCase().includes(needle),
      )
    : [];
  /** @type {TraceTranscriptRow[]} */
  const rows = [];
  for (const track of tracks) {
    for (const span of track.spans) {
      rows.push({ key: span.id, time: span.start, span, track, depth: track.depth });
    }
    for (const [index, marker] of track.markers.entries()) {
      rows.push({
        key: `${track.id}:marker:${index}`,
        time: marker.time,
        marker,
        track,
        depth: track.depth,
      });
    }
  }
  if (needle) {
    rows.splice(
      0,
      rows.length,
      ...rows.filter((row) =>
        row.span
          ? matches.some(({ span }) => span.id === row.span?.id)
          : (row.marker?.label || "").toLowerCase().includes(needle),
      ),
    );
  }
  rows.sort((a, b) => a.time - b.time);
  return {
    file: root,
    id: payload.id,
    cwd: payload.cwd,
    summary: payload.summary || {},
    tracks,
    spans,
    matches,
    rows,
    start,
    title: payload.title || traceFileStem(root) || `Request #${payload.id}`,
  };
}

/** @param {TraceViewModel} view */
function traceStateFor(view) {
  const state = traceBegin(view.file);
  if (!state.initialized) {
    state.initialized = true;
    state.collapsed = new Set(
      view.tracks.filter((track) => track.id !== "main").map((track) => track.id),
    );
  }
  return state;
}

function tracePage() {
  const payload = /** @type {TraceDetailResponse} */ (app.data);
  traceRememberScroll();
  traceStore.renderedSignature = traceSignature(payload);
  const view = traceViewModel(payload);
  traceStore.view = view;
  traceStateFor(view);
  const refreshBtn = `<button type="button" class="x-btn" data-variant="ghost" data-icon="true" data-trace-refresh aria-label="Refresh trace" title="Refresh trace">${icon(
    "refresh",
    14,
  )}</button>`;
  const toolbar = traceToolbarHTML(view);
  return `<div class="x-page traces-view" data-trace-file="${h(view.file)}"><div class="x-rise"><button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-trace-back data-tt-focus="back">${icon(
    "arrow",
    14,
  )} Sessions</button></div><header class="x-page-header x-rise"><div><h1 class="x-page-title traces-title x-truncate" title="${h(
    view.file || view.title,
  )}">${h(view.title)}</h1><p class="x-page-description x-mono">${h(view.cwd || "")}</p></div><div class="x-page-actions">${refreshBtn}</div></header>${traceSummaryHTML(view)}<section class="x-card x-rise" style="--i:1"><header class="x-card-header"><div class="x-card-titles"><h2 class="x-card-title">Timeline</h2><p class="x-card-description">W/S zoom · A/D pan · drag pan · wheel zoom · 0 fit · F focus selection · dbl-click focus · Esc deselect</p></div><div class="x-card-actions"><button type="button" class="x-btn" data-variant="ghost" data-size="sm" data-trace-expand-all data-tt-focus="expand-all">Expand all</button><button type="button" class="x-btn" data-variant="ghost" data-size="sm" data-trace-collapse-all data-tt-focus="collapse-all">Collapse all</button></div></header><div class="x-card-body"><div class="x-stack" style="gap:12px">${toolbar}<canvas class="traces-minimap" data-trace-minimap role="slider" aria-label="Timeline overview brush" aria-valuemin="0" aria-valuemax="100" tabindex="-1"></canvas><div class="traces-timeline" data-trace-timeline><div class="traces-gutter" data-trace-gutter></div><div class="traces-canvas-stack" data-trace-stack><canvas class="traces-canvas" data-trace-canvas tabindex="0" role="application" aria-label="Trace timeline. W and S zoom, A and D pan, 0 fits all, F focuses the selection."></canvas><div class="traces-canvas-hover" data-trace-hover aria-hidden="true" hidden></div><div class="traces-canvas-selection" data-trace-selection aria-hidden="true" hidden></div></div></div></div></div></section><section class="x-card x-rise" style="--i:2"><header class="x-card-header"><div class="x-card-titles"><h2 class="x-card-title">Transcript</h2><p class="x-card-description">Every span and marker in order; click to inspect</p></div></header><div class="x-card-body" data-flush="true"><div class="traces-list" data-trace-list><div class="traces-list-spacer" data-trace-spacer style="height:0"><div class="traces-list-window" data-trace-window></div></div></div></div></section>${traceAggregatesHTML()}</div>`;
}

/**
 * 渲染 Traces 列表页或单会话时间线详情页。
 * @returns {string}
 */
export function tracesPage() {
  if (!app.data) return '<div class="loading">Loading traces…</div>';
  return traceStore.request ? tracePage() : traceListPage();
}

let traceListenersBound = false;
function traceBindPageListeners() {
  if (traceListenersBound) return;
  traceListenersBound = true;
  $("page")?.addEventListener("keydown", (event) => {
    if (event.key !== "Enter" && event.key !== " " && event.key !== "Spacebar") return;
    if (!(event.target instanceof Element)) return;
    const action = event.target.closest(
      "tr[data-trace-open],th[data-ttsort],th[data-trace-tools-sort]",
    );
    if (
      !(action instanceof HTMLElement) ||
      event.target.closest("button,a,input,select,textarea")
    ) {
      return;
    }
    event.preventDefault();
    action.click();
  });
}

/**
 * 在页面 DOM 挂载后绑定 Traces 画布、Minimap、Transcript 与滚动恢复。
 */
export function wireTraces() {
  traceBindPageListeners();
  traceTooltipHide();
  const payload = /** @type {TraceDetailResponse | null} */ (app.data);
  if (!payload || !traceStore.request) return;
  const view =
    traceStore.view && traceStore.view.file === (payload.session_file || "")
      ? traceStore.view
      : traceViewModel(payload);
  traceStore.view = view;
  const state = traceStateFor(view);
  state.scale = traceScale(view.tracks, state.mode, state.compress);
  const layout = traceLayout(view.tracks, state.collapsed);
  state.layout = layout;
  traceRenderGutter(state, layout);
  traceWireCanvas(view, state, layout);
  traceRedraw();
  traceRenderDrawer(view, state);
  const timeline = /** @type {HTMLElement | null} */ (
    document.querySelector("[data-trace-timeline]")
  );
  if (timeline) {
    timeline.scrollTop = state.timelineScroll;
    timeline.onscroll = () => {
      state.timelineScroll = timeline.scrollTop;
    };
  }
  const list = /** @type {HTMLElement | null} */ (document.querySelector("[data-trace-list]"));
  if (list) {
    list.onscroll = () => {
      state.transcriptScroll = list.scrollTop;
      traceRenderTranscriptWindow(view, state);
    };
    list.scrollTop = state.transcriptScroll;
    traceRenderTranscriptWindow(view, state);
  }
  const canvas = document.querySelector("[data-trace-canvas]");
  if (!state.focusedCanvas && !state.drawer && canvas instanceof HTMLElement) {
    state.focusedCanvas = true;
    canvas.focus({ preventScroll: true });
  }
  if (traceStore.focusKey) {
    const element = [...document.querySelectorAll("[data-tt-focus]")].find(
      (candidate) =>
        candidate instanceof HTMLElement && candidate.dataset.ttFocus === traceStore.focusKey,
    );
    if (element instanceof HTMLElement) element.focus({ preventScroll: true });
    traceStore.focusKey = null;
  }
}

function tracesRerender() {
  const active = document.activeElement?.closest?.("[data-tt-focus]");
  traceStore.focusKey = active instanceof HTMLElement ? (active.dataset.ttFocus ?? null) : null;
  traceRememberScroll();
  hooks.render();
}

/**
 * 处理 Traces 列表与详情交互点击。
 * @param {MouseEvent} event
 * @returns {boolean}
 */
export function tracesClick(event) {
  if (!(event.target instanceof Element)) return false;
  const target = event.target.closest(
    "[data-trace-open],[data-trace-back],[data-trace-refresh],[data-ttsort],[data-trace-more],[data-trace-match],[data-trace-mode],[data-trace-compress],[data-trace-chevron],[data-trace-expand-all],[data-trace-collapse-all],[data-trace-row],[data-trace-close],[data-trace-copy],[data-trace-open-request],[data-trace-open-child],[data-trace-tools-sort],[data-trace-tools-more]",
  );
  if (!(target instanceof HTMLElement)) return false;
  const state = traceStore.state;
  const view = traceStore.view;
  const d = target.dataset;
  if (d.traceOpen !== undefined) {
    window.location.hash = `/traces?${new URLSearchParams({
      range: app.range,
      session: d.traceOpen,
    })}`;
    return true;
  }
  if ("traceBack" in d) {
    window.location.hash = `/traces?range=${app.range}`;
    return true;
  }
  if ("traceRefresh" in d) {
    traceStore.details.clear();
    traceStore.renderedSignature = "";
    hooks.refresh();
    return true;
  }
  if (d.ttsort !== undefined) {
    const key = d.ttsort;
    if (traceStore.sort.key === key) {
      traceStore.sort = { key, dir: traceStore.sort.dir === "asc" ? "desc" : "asc" };
    } else {
      traceStore.sort = { key, dir: d.ttalign === "right" ? "desc" : "asc" };
    }
    tracesRerender();
    return true;
  }
  if (d.traceMore !== undefined) {
    traceStore.shown =
      d.traceMore === "more" ? traceStore.shown + TRACE_LIST_LIMIT * 4 : TRACE_LIST_LIMIT;
    tracesRerender();
    return true;
  }
  if (d.traceMode !== undefined && state) {
    state.mode = /** @type {"duration" | "requests" | "calls"} */ (d.traceMode);
    state.viewport = null;
    tracesRerender();
    return true;
  }
  if (d.traceCompress !== undefined && state && target instanceof HTMLInputElement) {
    state.compress = target.checked;
    state.viewport = null;
    tracesRerender();
    return true;
  }
  if (d.traceMatch !== undefined && view && state) {
    if (!view.matches.length) return true;
    state.matchIndex =
      (state.matchIndex + Number(d.traceMatch) + view.matches.length) % view.matches.length;
    const span = view.matches[state.matchIndex].span;
    traceSelect(view, state, span.id, false);
    return true;
  }
  if (d.traceChevron !== undefined && state) {
    if (state.collapsed.has(d.traceChevron)) state.collapsed.delete(d.traceChevron);
    else state.collapsed.add(d.traceChevron);
    tracesRerender();
    return true;
  }
  if ("traceExpandAll" in d && state) {
    state.collapsed = new Set();
    tracesRerender();
    return true;
  }
  if ("traceCollapseAll" in d && view && state) {
    state.collapsed = new Set(
      view.tracks.filter((track) => track.id !== "main").map((track) => track.id),
    );
    tracesRerender();
    return true;
  }
  if (d.traceRow !== undefined && view && state) {
    traceSelect(view, state, d.traceRow, true);
    return true;
  }
  if ("traceClose" in d && view && state) {
    traceSelect(view, state, state.selection, false);
    return true;
  }
  if ("traceCopy" in d && view && state) {
    const selected = traceSelected(view, state);
    if (!selected) return true;
    const detail =
      selected.span.requestId != null ? traceStore.details.get(selected.span.requestId) : undefined;
    const payload = detail?.data ?? selected.span;
    navigator.clipboard
      ?.writeText(JSON.stringify(payload, null, 2))
      .then(() => {
        target.innerHTML = icon("check", 15);
        setTimeout(() => {
          target.innerHTML = icon("copy", 15);
        }, 1500);
      })
      .catch(() => {});
    return true;
  }
  if (d.traceOpenRequest !== undefined && view && state) {
    traceSelect(view, state, state.selection, false);
    hooks.openDrawer(Number(d.traceOpenRequest), target);
    return true;
  }
  if (d.traceOpenChild !== undefined && view && state) {
    state.collapsed.delete("main");
    state.collapsed.delete(d.traceOpenChild);
    const child = view.tracks.find((track) => track.id === d.traceOpenChild);
    const first = child?.spans[0];
    if (first) state.selection = first.id;
    tracesRerender();
    return true;
  }
  if (d.traceToolsSort !== undefined) {
    const key = d.traceToolsSort;
    if (traceStore.toolSort?.key === key) {
      /** @type {SortDirection} */
      const dir = traceStore.toolSort.dir === "asc" ? "desc" : "asc";
      traceStore.toolSort = { key, dir };
    } else {
      traceStore.toolSort = { key, dir: target.dataset.align === "right" ? "desc" : "asc" };
    }
    tracesRerender();
    return true;
  }
  if (d.traceToolsMore !== undefined) {
    traceStore.toolShown = d.traceToolsMore === "more" ? traceStore.toolShown + 48 : 12;
    tracesRerender();
    return true;
  }
  return false;
}

let tracesInitialized = false;

/**
 * 注册 Traces 模块桥接与全局抽屉 Esc/主题监听（幂等）。
 */
export function initTraces() {
  traceBridge.redraw = traceRedraw;
  traceBridge.onClick = tracesClick;
  traceBridge.select = traceSelect;
  if (tracesInitialized) return;
  tracesInitialized = true;
  document.addEventListener("keydown", (event) => {
    if (event.key !== "Escape" || !traceStore.state?.drawer) return;
    const overlay = $("overlay");
    if (overlay && !overlay.hidden) return;
    event.preventDefault();
    if (traceStore.view) {
      traceSelect(traceStore.view, traceStore.state, traceStore.state.selection, false);
    }
  });
  new MutationObserver(() => {
    if (app.page === "traces" && traceStore.request && traceStore.state?.layout) traceRedraw();
  }).observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
}

export { traceRedraw, traceRemoveDrawer };
