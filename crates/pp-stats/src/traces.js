"use strict";
// Traces 页：按根会话折叠子 Agent 的会话列表，以及单会话时间线阅读器。
// 结构、交互与配色对齐 omp stats 的 #/traces；数据来自 pp stats 自己的快照。
let traceRequest = null;
let traceRenderedSignature = "";
let traceSort = { key: "started", dir: "desc" };
let traceShown = 50;
let traceToolSort = null;
let traceToolShown = 12;
let traceState = null;
let traceView = null;
let traceFocusKey = null;
const traceDetails = new Map();
const TRACE_ROW_H = 28;
const TRACE_LIST_H = 340;
const TRACE_LIST_LIMIT = 50;
const TRACE_KIND_COLORS = {
  model: "var(--chart-primary)",
  tool: "var(--warn)",
  subagent: "var(--chart-secondary)",
};
const TRACE_MODE_OPTIONS = [
  { value: "duration", label: "Duration", title: "Real wall-clock time" },
  { value: "requests", label: "Requests", title: "Equal width per model request" },
  { value: "calls", label: "Calls", title: "Equal width per model/tool call boundary" },
];
const TRACE_LANES = [
  { kind: "model", label: "Model" },
  { kind: "tool", label: "Tools" },
  { kind: "subagent", label: "Agents" },
];

function resetTraces(url) {
  const session = url.searchParams.get("session");
  const request = url.searchParams.get("request");
  traceRequest = /^[1-9]\d*$/.test(session || request || "")
    ? session || request
    : null;
  traceSort = { key: "started", dir: "desc" };
  traceShown = TRACE_LIST_LIMIT;
  traceToolSort = null;
  traceToolShown = 12;
  traceState = null;
  traceView = null;
  traceRenderedSignature = "";
  traceFocusKey = null;
  traceDetails.clear();
  traceRemoveDrawer();
}

function traceSignature(payload) {
  if (!payload) return "";
  if (Array.isArray(payload.sessions)) {
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
  const s = payload.summary || {};
  const requests = payload.requests || [];
  const tools = payload.tool_calls || [];
  return [
    "trace",
    range,
    payload.session_file,
    payload.title,
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

function traceRelative(timestamp) {
  const seconds = Math.max(0, (Date.now() - timestamp) / 1000);
  if (seconds < 30) return "less than a minute ago";
  if (seconds < 90) return "1 minute ago";
  const minutes = Math.round(seconds / 60);
  if (minutes < 45) return `${minutes} minutes ago`;
  if (minutes < 90) return "about 1 hour ago";
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `about ${hours} hours ago`;
  if (hours < 42) return "1 day ago";
  const days = Math.round(hours / 24);
  if (days < 30) return `${days} days ago`;
  if (days < 45) return "about 1 month ago";
  const months = Math.round(days / 30);
  if (months < 12) return `${months} months ago`;
  const years = Math.round(months / 12);
  return years <= 1 ? "about 1 year ago" : `about ${years} years ago`;
}
const traceInteger = (value) => Number(value ?? 0).toLocaleString("en-US");
const traceCompact = (value) =>
  Number(value ?? 0).toLocaleString("en-US", { notation: "compact" });
const tracePercent = (value, digits = 1) => `${(value * 100).toFixed(digits)}%`;
const traceDurationMs = (value, digits) => {
  if (value == null) return "-";
  const seconds = value / 1000;
  const precision = digits !== undefined ? digits : seconds < 1 ? 2 : 1;
  return `${seconds.toFixed(precision)}s`;
};
function traceElapsed(ms) {
  if (ms == null || !Number.isFinite(ms)) return "—";
  const value = Math.max(0, ms);
  if (value < 60000) return traceDurationMs(value);
  const totalMinutes = Math.floor(value / 60000);
  if (totalMinutes < 60)
    return `${totalMinutes}m ${String(Math.floor((value % 60000) / 1000)).padStart(2, "0")}s`;
  return `${Math.floor(totalMinutes / 60)}h ${String(totalMinutes % 60).padStart(2, "0")}m`;
}
function traceCost(value, digits) {
  if (value == null) return "—";
  if (value === 0) return "$0";
  const precision = digits !== undefined ? digits : value > 0 && value < 0.01 ? 4 : 2;
  return `$${Number(value).toLocaleString("en-US", {
    minimumFractionDigits: precision,
    maximumFractionDigits: precision,
  })}`;
}
const traceEstimatedCost = (value, unpriced) =>
  value === 0 && unpriced > 0 ? "N/A" : traceCost(value);
function traceOffset(deltaMs) {
  const sign = deltaMs < 0 ? "-" : "+";
  const abs = Math.abs(deltaMs);
  const hours = Math.floor(abs / 3600000);
  const minutes = Math.floor((abs % 3600000) / 60000);
  const seconds = Math.floor((abs % 60000) / 1000);
  const millis = Math.floor(abs % 1000);
  const withMs = abs % 1000 !== 0;
  const mmss = `${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}${
    withMs ? `.${String(millis).padStart(3, "0")}` : ""
  }`;
  return hours > 0 ? `${sign}${hours}:${mmss}` : `${sign}${mmss}`;
}
function traceClock(timestamp) {
  const date = new Date(timestamp);
  return [date.getHours(), date.getMinutes(), date.getSeconds()]
    .map((part) => String(part).padStart(2, "0"))
    .join(":");
}
function traceFileStem(file) {
  const name = String(file || "").split(/[\\/]/).pop() || "";
  return name.replace(/\.jsonl$/, "");
}
function traceFolderTail(cwd) {
  const parts = String(cwd || "").split(/[\\/]/).filter(Boolean);
  return parts.slice(-2).join("/");
}
function traceRememberScroll() {
  if (!traceState || !traceRequest) return;
  const timeline = document.querySelector("[data-trace-timeline]");
  const list = document.querySelector("[data-trace-list]");
  if (timeline) traceState.timelineScroll = timeline.scrollTop;
  if (list) traceState.transcriptScroll = list.scrollTop;
}

function tracesPage() {
  if (!data) return '<div class="loading">Loading traces…</div>';
  return traceRequest ? tracePage() : traceListPage();
}

/* ----------------------------- 会话列表 ----------------------------- */

function traceModelLabel(models) {
  if (!models || !models.length) return '<span class="x-dim">-</span>';
  const keys = models.map((model) => model.key);
  return `<span class="traces-models" title="${h(keys.join("\n"))}"><span class="x-mono x-truncate">${h(
    models[0].model,
  )}</span>${models.length > 1 ? `<span class="x-badge">+${models.length - 1}</span>` : ""}</span>`;
}
function traceListRows() {
  const sessions = data.sessions || [];
  const needle = query.trim().toLowerCase();
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
  const numeric = {
    started: (s) => s.first_seen,
    duration: (s) => s.last_seen - s.first_seen,
    requests: (s) => s.requests,
    tools: (s) => s.tool_calls,
    agents: (s) => s.agents,
    tokens: (s) => s.tokens,
    cost: (s) => s.cost,
  };
  if (numeric[traceSort.key]) {
    const value = numeric[traceSort.key];
    rows.sort((a, b) => (value(a) - value(b)) * (traceSort.dir === "asc" ? 1 : -1));
  } else if (traceSort.key === "session") {
    rows.sort(
      (a, b) =>
        String(a.title || a.session_file || "").localeCompare(
          String(b.title || b.session_file || ""),
        ) * (traceSort.dir === "asc" ? 1 : -1),
    );
  } else if (traceSort.key === "models") {
    rows.sort(
      (a, b) =>
        String(a.models?.[0]?.model || "").localeCompare(String(b.models?.[0]?.model || "")) *
        (traceSort.dir === "asc" ? 1 : -1),
    );
  }
  return rows;
}
function traceSortHeader(key, label, align, width) {
  const sorted = traceSort.key === key;
  return `<th data-ttsort="${key}" data-ttalign="${align || "left"}" data-sortable="true" tabindex="0"${
    sorted ? ` data-sorted="${traceSort.dir}"` : ""
  }${align === "right" ? ' data-align="right"' : ""}${
    width ? ` style="width:${width}"` : ""
  } aria-sort="${sorted ? (traceSort.dir === "asc" ? "ascending" : "descending") : ""}">${label}${
    sorted
      ? `<span class="x-sort-arrow">${traceSort.dir === "asc" ? "↑" : "↓"}</span>`
      : ""
  }</th>`;
}
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
function traceCostCell(value, max, unpriced) {
  const pct = max > 0 ? Math.min(100, (value / max) * 100) : 0;
  return `<div class="x-meter-cell"><span class="x-num">${traceEstimatedCost(
    value,
    unpriced,
  )}</span><div class="x-meter"><div class="x-meter-fill" style="width:${pct}%"></div></div></div>`;
}
function traceListPage() {
  traceRememberScroll();
  traceRemoveDrawer();
  traceRenderedSignature = traceSignature(data);
  const rows = traceListRows();
  const total = (data.sessions || []).length;
  const maxCost = Math.max(0, ...rows.map((row) => row.cost || 0));
  const shown = rows.slice(0, traceShown);
  const header = `<header class="x-page-header x-rise"><div><h1 class="x-page-title">Traces</h1><p class="x-page-description">Recent sessions with subagent activity folded in. Open one to inspect its timeline.</p></div></header>`;
  const description = data.sessions
    ? query.trim()
      ? `${traceInteger(rows.length)} of ${traceInteger(total)} most recent`
      : `${traceInteger(total)} most recent`
    : undefined;
  const search = `<label class="x-search traces-filter">${icon("search", 14)}<input class="x-input" id="search" type="search" aria-label="Filter sessions" placeholder="Filter by summary, project, model…" value="${h(
    query,
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
            )}</td><td><span class="x-muted" title="${h(
              localTime(session.first_seen),
            )}">${traceRelative(session.first_seen)}</span></td><td data-align="right"><span class="x-num">${traceElapsed(
              session.last_seen - session.first_seen,
            )}</span></td><td data-align="right"><span class="x-num">${traceInteger(
              session.requests,
            )}</span></td><td data-align="right"><span class="x-num">${traceInteger(
              session.tool_calls,
            )}</span></td><td data-align="right"><span class="x-num${
              session.agents ? "" : " x-dim"
            }">${traceInteger(session.agents)}</span></td><td data-align="right"><span class="x-num">${traceCompact(
              session.tokens,
            )}</span></td><td data-align="right">${traceCostCell(
              session.cost || 0,
              maxCost,
              session.unpriced || 0,
            )}</td></tr>`,
        )
        .join("")}</tbody></table></div>`
    : `<div class="x-empty">${icon("inbox", 20)}<div class="x-empty-title">${
        query.trim() ? "No matching sessions" : "No sessions found"
      }</div><div class="x-empty-hint">${
        query.trim() ? "Try a different filter." : "Run a pp session to record activity."
      }</div></div>`;
  const more =
    rows.length > TRACE_LIST_LIMIT
      ? `<div class="x-table-more"><span class="x-micro">${traceInteger(
          Math.min(traceShown, rows.length),
        )} of ${traceInteger(rows.length)}</span>${
          rows.length > traceShown
            ? '<button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-trace-more="more">Show more</button>'
            : ""
        }${
          traceShown > TRACE_LIST_LIMIT
            ? '<button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-trace-more="fewer">Show fewer</button>'
            : ""
        }</div>`
      : "";
  const note = data.has_more
    ? `<div class="load-note"><span>Loaded the latest ${traceInteger(
        data.limit,
      )} sessions in this range. Narrow the range to inspect older sessions.</span>${
        limit === 500
          ? '<button class="outline-button" data-load>Load latest 2,000</button>'
          : ""
      }</div>`
    : "";
  return `<div class="x-page traces-page">${header}<section class="x-card x-rise" style="--i:0"><header class="x-card-header"><div class="x-card-titles"><h2 class="x-card-title">Sessions</h2>${
    description ? `<p class="x-card-description">${h(description)}</p>` : ""
  }</div><div class="x-card-actions">${search}</div></header><div class="x-card-body" data-flush="true">${body}${more}</div></section>${note}${pricingNotice(
    data,
  )}</div>`;
}

/* ----------------------------- 详情模型 ----------------------------- */

function traceBegin(file) {
  if (!traceState || traceState.file !== file) {
    traceState = {
      file,
      mode: "duration",
      compress: true,
      selection: null,
      drawer: false,
      matchIndex: 0,
      collapsed: new Set(),
      viewport: null,
      transcriptScroll: 0,
      timelineScroll: 0,
      layout: null,
      scale: null,
      width: 800,
      hits: [],
      hover: null,
    };
  }
  return traceState;
}
function traceViewModel() {
  const root = data.session_file || "";
  const requests = (data.requests || [])
    .slice()
    .sort((a, b) => a.timestamp - b.timestamp || a.id - b.id);
  const toolCalls = (data.tool_calls || [])
    .slice()
    .sort((a, b) => a.timestamp - b.timestamp || a.id - b.id);
  const starts = new Map();
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
    const track = {
      id: `main/${index + 1}`,
      label: `subagent ${index + 1}`,
      file,
      model: requests.find((request) => request.session_file === file)?.model || "",
      depth: 1,
      spans: [],
      markers: [],
    };
    tracks.push(track);
    byFile.set(file, track);
  });
  for (const request of requests) {
    const track = byFile.get(request.session_file) || tracks[0];
    const known =
      request.input_tokens != null || request.output_tokens != null;
    const span = {
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
      tokens: known
        ? (request.input_tokens || 0) + (request.output_tokens || 0)
        : undefined,
      cost: request.cost,
      ttft: request.ttft_ms,
      stopReason: request.stop_reason,
      errorMessage: request.error_message,
      inputTokens: request.input_tokens,
      outputTokens: request.output_tokens,
      cachedTokens: request.cached_tokens,
      usageKnown: known,
    };
    track.spans.push(span);
    if (request.status !== "ok")
      track.markers.push({
        time: request.timestamp,
        label: `${request.status} · ${request.model}`,
        detail: request.error_message || "",
      });
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
  const spans = [];
  for (const track of tracks)
    for (const span of track.spans) spans.push({ span, track });
  spans.sort((a, b) => a.span.start - b.span.start);
  const start = traceBounds(tracks)[0];
  const needle = query.trim().toLowerCase();
  const matches = needle
    ? spans.filter(({ span }) =>
        `${span.label} ${span.detail ?? ""}`
          .toLowerCase()
          .includes(needle),
      )
    : [];
  const rows = [];
  for (const track of tracks) {
    for (const span of track.spans)
      rows.push({ key: span.id, time: span.start, span, track, depth: track.depth });
    track.markers.forEach((marker, index) =>
      rows.push({
        key: `${track.id}:marker:${index}`,
        time: marker.time,
        marker,
        track,
        depth: track.depth,
      }),
    );
  }
  if (needle) {
    rows.splice(
      0,
      rows.length,
      ...rows.filter((row) =>
        row.span
          ? matches.some(({ span }) => span.id === row.span.id)
          : (row.marker.label || "").toLowerCase().includes(needle),
      ),
    );
  }
  rows.sort((a, b) => a.time - b.time);
  return {
    file: root,
    id: data.id,
    cwd: data.cwd,
    summary: data.summary || {},
    tracks,
    spans,
    matches,
    rows,
    start,
    title: data.title || traceFileStem(root) || `Request #${data.id}`,
  };
}
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
function traceSelected(view, state) {
  if (!state.selection) return null;
  for (const track of view.tracks) {
    const span = track.spans.find((candidate) => candidate.id === state.selection);
    if (span) return { span, track };
  }
  return null;
}

/* ----------------------------- 详情页面 ----------------------------- */

function traceSummaryHTML(view) {
  const summary = view.summary;
  const share = (ms) =>
    summary.wall_ms > 0 && ms != null
      ? `${tracePercent(ms / summary.wall_ms)} of wall`
      : undefined;
  const tiles = [
    { label: "Wall time", value: traceElapsed(summary.wall_ms) },
    { label: "Model time", value: traceElapsed(summary.model_ms), hint: share(summary.model_ms) },
    { label: "Tool time", value: traceElapsed(summary.tool_ms), hint: share(summary.tool_ms) },
    { label: "Idle", value: traceElapsed(summary.idle_ms), hint: share(summary.idle_ms) },
    {
      label: "Errors",
      value: traceInteger((summary.failed || 0) + (summary.aborted || 0)),
      hint: `${traceInteger(summary.failed)} failed · ${traceInteger(summary.aborted)} aborted`,
    },
    {
      label: "Requests",
      value: traceInteger(summary.requests),
      hint: `${traceInteger(summary.tool_calls)} tool calls`,
    },
    { label: "Agents", value: traceInteger(summary.agents) },
    { label: "Tokens", value: traceCompact(summary.tokens) },
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
function traceToolbarHTML(view) {
  const mode = traceState?.mode || "duration";
  const matches = view.matches.length;
  const index = traceState ? traceState.matchIndex % Math.max(matches, 1) : 0;
  const searching = query.trim() !== "";
  const legend = [
    ["model", "Model"],
    ["tool", "Tool"],
    ["subagent", "Agent"],
  ]
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
          traceState?.compress ? " checked" : ""
        }> Compress idle</label>`
      : ""
  }<div class="traces-search"><label class="x-search">${icon(
    "search",
    14,
  )}<input class="x-input" id="search" type="search" aria-label="Search spans" placeholder="Search spans…" value="${h(
    query,
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
function traceAggregatesHTML() {
  const stats = (data.tool_stats || []).slice();
  if (!stats.length) return "";
  const key = traceToolSort?.key;
  const dir = traceToolSort?.dir === "asc" ? 1 : -1;
  const value = {
    tool: (row) => row.tool,
    calls: (row) => row.calls,
    errors: (row) => row.errors,
    total: (row) => row.total_ms,
    avg: (row) => (row.calls > 0 ? row.total_ms / row.calls : 0),
    max: (row) => row.max_ms,
  }[key];
  if (value)
    stats.sort((a, b) =>
      typeof value(a) === "number"
        ? (value(a) - value(b)) * dir
        : String(value(a)).localeCompare(String(value(b))) * dir,
    );
  const max = Math.max(...stats.map((row) => row.total_ms), 0);
  const shown = stats.slice(0, traceToolShown);
  const header = (field, label, align) => {
    const sorted = traceToolSort?.key === field;
    return `<th data-trace-tools-sort="${field}" data-sortable="true" tabindex="0"${
      sorted ? ` data-sorted="${traceToolSort.dir}"` : ""
    }${align === "right" ? ' data-align="right"' : ""} aria-sort="${
      sorted ? (traceToolSort.dir === "asc" ? "ascending" : "descending") : ""
    }">${label}${
      sorted ? `<span class="x-sort-arrow">${traceToolSort.dir === "asc" ? "↑" : "↓"}</span>` : ""
    }</th>`;
  };
  const more =
    stats.length > 12
      ? `<div class="x-table-more"><span class="x-micro">${traceInteger(
          Math.min(traceToolShown, stats.length),
        )} of ${traceInteger(stats.length)}</span>${
          stats.length > traceToolShown
            ? '<button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-trace-tools-more="more">Show more</button>'
            : ""
        }${
          traceToolShown > 12
            ? '<button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-trace-tools-more="fewer">Show fewer</button>'
            : ""
        }</div>`
      : "";
  return `<section class="x-card x-rise" style="--i:3"><header class="x-card-header"><div class="x-card-titles"><h2 class="x-card-title">Tool aggregates</h2><p class="x-card-description">${traceInteger(
    stats.length,
  )} tools, by total time</p></div></header><div class="x-card-body" data-flush="true"><div class="x-table-wrap"><table class="x-table" data-dense="true"><thead><tr>${header(
    "tool",
    "Tool",
  )}${header("calls", "Calls", "right")}${header(
    "errors",
    "Errors",
    "right",
  )}${header("total", "Total", "right")}${header(
    "avg",
    "Avg",
    "right",
  )}${header("max", "Max", "right")}</tr></thead><tbody>${shown
    .map(
      (row) =>
        `<tr><td><span class="x-mono">${h(row.tool)}</span></td><td data-align="right"><span class="x-num">${traceInteger(
          row.calls,
        )}</span></td><td data-align="right">${
          row.errors > 0
            ? `<span class="x-badge" data-tone="bad">${traceInteger(row.errors)} failed</span>`
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
function tracePage() {
  traceRememberScroll();
  traceRenderedSignature = traceSignature(data);
  const view = traceViewModel();
  traceView = view;
  traceStateFor(view);
  const refresh = `<button type="button" class="x-btn" data-variant="ghost" data-icon="true" data-trace-refresh aria-label="Refresh trace" title="Refresh trace">${icon(
    "refresh",
    14,
  )}</button>`;
  const toolbar = traceToolbarHTML(view);
  return `<div class="x-page traces-view" data-trace-file="${h(view.file)}"><div class="x-rise"><button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-trace-back data-tt-focus="back">${icon(
    "arrow",
    14,
  )} Sessions</button></div><header class="x-page-header x-rise"><div><h1 class="x-page-title traces-title x-truncate" title="${h(
    view.file || view.title,
  )}">${h(view.title)}</h1><p class="x-page-description x-mono">${h(view.cwd || "")}</p></div><div class="x-page-actions">${refresh}</div></header>${traceSummaryHTML(view)}<section class="x-card x-rise" style="--i:1"><header class="x-card-header"><div class="x-card-titles"><h2 class="x-card-title">Timeline</h2><p class="x-card-description">W/S zoom · A/D pan · drag pan · wheel zoom · 0 fit · F focus selection · dbl-click focus · Esc deselect</p></div><div class="x-card-actions"><button type="button" class="x-btn" data-variant="ghost" data-size="sm" data-trace-expand-all data-tt-focus="expand-all">Expand all</button><button type="button" class="x-btn" data-variant="ghost" data-size="sm" data-trace-collapse-all data-tt-focus="collapse-all">Collapse all</button></div></header><div class="x-card-body"><div class="x-stack" style="gap:12px">${toolbar}<canvas class="traces-minimap" data-trace-minimap role="slider" aria-label="Timeline overview brush" aria-valuemin="0" aria-valuemax="100" tabindex="-1"></canvas><div class="traces-timeline" data-trace-timeline><div class="traces-gutter" data-trace-gutter></div><div class="traces-canvas-stack" data-trace-stack><canvas class="traces-canvas" data-trace-canvas tabindex="0" role="application" aria-label="Trace timeline. W and S zoom, A and D pan, 0 fits all, F focuses the selection."></canvas><div class="traces-canvas-hover" data-trace-hover aria-hidden="true" hidden></div><div class="traces-canvas-selection" data-trace-selection aria-hidden="true" hidden></div></div></div></div></div></section><section class="x-card x-rise" style="--i:2"><header class="x-card-header"><div class="x-card-titles"><h2 class="x-card-title">Transcript</h2><p class="x-card-description">Every span and marker in order; click to inspect</p></div></header><div class="x-card-body" data-flush="true"><div class="traces-list" data-trace-list><div class="traces-list-spacer" data-trace-spacer style="height:0"><div class="traces-list-window" data-trace-window></div></div></div></div></section>${traceAggregatesHTML()}</div>`;
}

/* ----------------------------- 抽屉 ----------------------------- */

function traceDrawerMetrics(span, track) {
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
    items.push(["Tokens", `<span class="x-num">${traceInteger(span.tokens ?? 0)}</span>`]);
    items.push(["Cost", `<span class="x-num">${traceCost(span.cost ?? 0, 4)}</span>`]);
    items.push(["TTFT", `<span class="x-num">${traceDurationMs(span.ttft)}</span>`]);
    items.push(["Model", `<span class="x-mono">${h(span.model || "-")}</span>`]);
    if (span.usageKnown)
      items.push([
        "Input / output / cached",
        `<span class="x-num">${traceInteger(span.inputTokens ?? 0)} / ${traceInteger(
          span.outputTokens ?? 0,
        )} / ${traceInteger(span.cachedTokens ?? 0)}</span>`,
      ]);
    if (span.provider)
      items.push(["Provider", `<span class="x-mono">${h(span.provider)}</span>`]);
    if (span.stopReason)
      items.push(["Stop reason", `<span class="x-mono">${h(span.stopReason)}</span>`]);
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
    const child = traceView?.tracks.find((track) => track.id === span.childTrackId);
    items.push([
      "Requests",
      `<span class="x-num">${traceInteger(child?.spans.filter((s) => s.kind === "model").length || 0)}</span>`,
    ]);
    items.push([
      "Tools",
      `<span class="x-num">${traceInteger(child?.spans.filter((s) => s.kind === "tool").length || 0)}</span>`,
    ]);
  }
  return items;
}
function traceDrawerHTML(view, state) {
  const selected = traceSelected(view, state);
  if (!state.drawer || !selected) return "";
  const { span, track } = selected;
  const detail = span.kind === "model" ? traceDetails.get(span.requestId) : null;
  const pending = Boolean(detail?.pending);
  const metrics = traceDrawerMetrics(span, track);
  const errorMessage =
    span.errorMessage ||
    (detail?.data && detail.data.request?.error_message) ||
    null;
  let sections = "";
  if (errorMessage)
    sections += `<div class="x-stack" style="gap:6px"><div class="section-label tone-bad">Error message</div><pre class="code-block traces-pre">${h(
      errorMessage,
    )}</pre></div>`;
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
    sections += `<div class="x-stack" style="gap:6px"><div class="section-label">Args</div><div class="x-muted traces-note">Tool arguments and results are not recorded by pp.</div></div>`;
  }
  if (span.kind === "model") {
    if (pending || !detail)
      sections += `<div class="x-stack traces-skeleton"><div class="x-skeleton" style="height:80px"></div><div class="x-skeleton" style="height:160px"></div></div>`;
    else if (detail.error)
      sections += `<div class="traces-error" role="alert">${icon(
        "alert",
        15,
      )}<span>Failed to load request: ${h(detail.error)}</span></div>`;
    else if (detail.data) {
      sections += `<div class="x-stack" style="gap:6px"><div class="section-label">Message content</div><div class="x-muted traces-note">Text, thinking and tool arguments are not recorded by pp.</div></div>`;
      sections += `<details class="traces-json" data-trace-json><summary>Raw request</summary><pre class="code-block traces-pre"></pre></details>`;
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
    host.addEventListener("click", (event) => tracesClick(event));
    document.body.append(host);
  }
  return host;
}
function traceRemoveDrawer() {
  const host = document.getElementById("trace-drawer-host");
  if (host) host.innerHTML = "";
  traceTooltipHide();
}
function traceRenderDrawer(view, state) {
  const host = traceDrawerHost();
  host.innerHTML = traceDrawerHTML(view, state);
  const sheet = host.querySelector("[data-trace-sheet]");
  if (sheet) {
    sheet.focus({ preventScroll: true });
    for (const details of sheet.querySelectorAll("[data-trace-json]"))
      details.ontoggle = () => {
        if (!details.open) return;
        const span = traceSelected(view, state)?.span;
        const entry = span ? traceDetails.get(span.requestId) : null;
        const pre = details.querySelector("pre");
        if (pre && entry?.data && !pre.dataset.filled) {
          pre.textContent = JSON.stringify(entry.data.request ?? entry.data, null, 2);
          pre.dataset.filled = "true";
        }
      };
  }
}

/* ----------------------------- 时间轴布局 ----------------------------- */

const RULER_H = 28;
const HEADER_H = 22;
const LANE_H = 18;
const LANE_GAP = 3;
const TRACK_GAP = 14;
const MIN_WINDOW_U = 10;
const MIN_SPAN_PX = 2;
const HIT_SLOP_PX = 4;
const LABEL_MIN_PX = 56;
const SPAN_RADIUS = 3;
const DIM_ALPHA = 0.3;

function traceLayout(tracks, collapsed) {
  const byId = new Map(tracks.map((track) => [track.id, track]));
  const hidden = (track) => {
    let parent = track.parentId;
    while (parent) {
      if (collapsed.has(parent)) return true;
      parent = byId.get(parent)?.parentId ?? null;
    }
    return false;
  };
  const blocks = [];
  const lanes = [];
  let y = RULER_H + 6;
  for (const track of tracks) {
    if (hidden(track)) continue;
    const byKind = { model: [], tool: [], subagent: [] };
    for (const span of track.spans) byKind[span.kind].push(span);
    const block = {
      track,
      depth: track.depth,
      headerY: y,
      lanes: [],
      turnSpans: byKind.model,
      y0: y,
      y1: y,
    };
    y += HEADER_H;
    // 折叠的轨道只保留标题行；主轨道始终展开，其箭头只控制子轨道。
    const collapsedSelf = collapsed.has(track.id) && track.id !== "main";
    for (const lane of collapsedSelf ? [] : TRACE_LANES) {
      const spans = byKind[lane.kind];
      const core = lane.kind === "model" || lane.kind === "tool";
      if (!spans.length && !(track.id === "main" && core)) continue;
      const row = { track, kind: lane.kind, label: lane.label, y, spans };
      block.lanes.push(row);
      lanes.push(row);
      y += LANE_H + LANE_GAP;
    }
    block.y1 = collapsedSelf ? block.headerY + HEADER_H : y - LANE_GAP;
    blocks.push(block);
    y += TRACK_GAP;
  }
  return { blocks, lanes, totalHeight: Math.max(y, RULER_H + 48) };
}
function traceLaneAxes(layout, scale) {
  return layout.lanes.map((lane) => {
    const startU = [];
    const endU = [];
    const maxEndU = [];
    let max = -Infinity;
    for (const span of lane.spans) {
      startU.push(scale.toU(span.start));
      endU.push(scale.toU(span.end));
      max = Math.max(max, scale.toU(span.end));
      maxEndU.push(max);
    }
    return { startU, endU, maxEndU };
  });
}
function traceFirstVisible(maxEndU, u0) {
  let lo = 0;
  let hi = maxEndU.length;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (maxEndU[mid] < u0) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

/* ----------------------------- 虚拟时间轴 ----------------------------- */

function tracePiecewise(tPoints, uPoints) {
  const count = tPoints.length;
  const domain = [uPoints[0], uPoints[count - 1]];
  const segmentAt = (points, value) => {
    let lo = 0;
    let hi = count - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (points[mid] <= value) lo = mid;
      else hi = mid - 1;
    }
    return Math.min(lo, count - 2);
  };
  return {
    toU(t) {
      if (t <= tPoints[0]) return uPoints[0];
      if (t >= tPoints[count - 1]) return uPoints[count - 1];
      const i = segmentAt(tPoints, t);
      const span = tPoints[i + 1] - tPoints[i];
      if (span <= 0) return uPoints[i];
      return uPoints[i] + ((t - tPoints[i]) / span) * (uPoints[i + 1] - uPoints[i]);
    },
    toT(u) {
      if (u <= uPoints[0]) return tPoints[0];
      if (u >= uPoints[count - 1]) return tPoints[count - 1];
      const i = segmentAt(uPoints, u);
      const span = uPoints[i + 1] - uPoints[i];
      if (span <= 0) return tPoints[i];
      return tPoints[i] + ((u - uPoints[i]) / span) * (tPoints[i + 1] - tPoints[i]);
    },
    domain,
    gaps: [],
  };
}
function traceBounds(tracks) {
  let min = Infinity;
  let max = -Infinity;
  for (const track of tracks)
    for (const span of track.spans) {
      if (span.start < min) min = span.start;
      if (span.end > max) max = span.end;
    }
  if (!Number.isFinite(min)) return [0, 1];
  return [min, Math.max(max, min + 1)];
}
function traceEqualWidth(anchors, tMin, tMax) {
  const unique = [...new Set(anchors.filter((t) => t >= tMin && t <= tMax))].sort(
    (a, b) => a - b,
  );
  if (unique[0] !== tMin) unique.unshift(tMin);
  if (unique[unique.length - 1] !== tMax) unique.push(tMax);
  if (unique.length < 2) return tracePiecewise([tMin, tMax], [tMin, tMax]);
  const step = (tMax - tMin) / (unique.length - 1);
  return tracePiecewise(
    unique,
    unique.map((_, index) => tMin + index * step),
  );
}
function traceCompressed(tracks, tMin, tMax) {
  const intervals = [];
  for (const track of tracks)
    for (const span of track.spans) intervals.push([span.start, span.end]);
  intervals.sort((a, b) => a[0] - b[0]);
  const segments = [];
  for (const [start, end] of intervals) {
    const from = start - 2000;
    const to = end + 2000;
    const last = segments[segments.length - 1];
    if (last && from <= last[1]) last[1] = Math.max(last[1], to);
    else segments.push([from, to]);
  }
  if (!segments.length) segments.push([tMin, tMax]);
  segments[0][0] = Math.max(segments[0][0], tMin);
  segments[segments.length - 1][1] = Math.min(
    Math.max(segments[segments.length - 1][1], segments[segments.length - 1][0]),
    tMax,
  );
  const tPoints = [];
  const uPoints = [];
  const gaps = [];
  let u = segments[0][0];
  for (let i = 0; i < segments.length; i++) {
    const [start, end] = segments[i];
    if (i === 0) {
      tPoints.push(start);
      uPoints.push(u);
    } else {
      const previousEnd = segments[i - 1][1];
      const gap = start - previousEnd;
      if (gap > 30000) {
        u += 8000;
        gaps.push({ t0: previousEnd, t1: start, uMid: u - 4000 });
      } else {
        u += gap;
      }
      tPoints.push(start);
      uPoints.push(u);
    }
    u += Math.max(0, end - start);
    tPoints.push(end);
    uPoints.push(u);
  }
  return { ...tracePiecewise(tPoints, uPoints), gaps };
}
function traceScale(tracks, mode, compress) {
  const [tMin, tMax] = traceBounds(tracks);
  if (mode === "requests") {
    const main = tracks.find((track) => track.id === "main");
    return traceEqualWidth(
      (main?.spans || []).filter((span) => span.kind === "model").map((span) => span.start),
      tMin,
      tMax,
    );
  }
  if (mode === "calls") {
    const anchors = [];
    for (const track of tracks)
      for (const span of track.spans)
        if (span.kind === "model" || span.kind === "tool" || span.kind === "subagent")
          anchors.push(span.start, span.end);
    return traceEqualWidth(anchors, tMin, tMax);
  }
  if (compress) return traceCompressed(tracks, tMin, tMax);
  return tracePiecewise([tMin, tMax], [tMin, tMax]);
}
function traceNiceStep(raw) {
  const pow = 10 ** Math.floor(Math.log10(Math.max(raw, 1)));
  for (const multiple of [1, 2, 5, 10]) if (pow * multiple >= raw) return pow * multiple;
  return pow * 10;
}
function traceTicks(scale, u0, u1, width) {
  const span = u1 - u0;
  if (span <= 0 || width <= 0) return [];
  const ticks = [];
  const domain = scale.domain[0];
  const step = traceNiceStep((span * 90) / width);
  const first = domain + Math.ceil((u0 - domain) / step) * step;
  let count = 0;
  for (let u = first; u <= u1 && count < 200; u += step, count++) {
    const major = Math.round((u - domain) / step) % 5 === 0;
    ticks.push({
      u,
      major,
      label: major ? traceClock(scale.toT(u)) : traceOffset(u - domain),
    });
  }
  return ticks;
}

/* ----------------------------- 画布调色 ----------------------------- */

function traceColors() {
  const style = getComputedStyle(document.documentElement);
  const token = (name, fallback) => style.getPropertyValue(name).trim() || fallback;
  return {
    category: {
      model: token("--chart-primary", "#1fa9bb"),
      tool: token("--warn", "#c77c02"),
      subagent: token("--chart-secondary", "#d42ea3"),
    },
    error: token("--bad", "#dc3a40"),
    errorSoft: token("--bad-soft", "rgba(220,58,64,0.1)"),
    marker: token("--ink-1", "#151518"),
    turnBand: token("--hover", "rgba(0,0,0,0.03)"),
    grid: token("--chart-grid", "rgba(0,0,0,0.06)"),
    tick: token("--chart-axis", "#9a9aa2"),
    spanText: token("--ink-inverse", "#fff"),
    selection: token("--focus", "#1fa9bb"),
    brush: token("--selected", "rgba(0,0,0,0.06)"),
    fontSans: token("--font-sans", "system-ui, sans-serif"),
    fontMono: token("--font-mono", "ui-monospace, monospace"),
  };
}

function traceSpanPath(ctx, x, y, w, h) {
  ctx.beginPath();
  if (w >= SPAN_RADIUS * 2 + 1) ctx.roundRect(x, y, w, h, SPAN_RADIUS);
  else ctx.rect(x, y, w, h);
}
function traceDrawScene(state, view, layout, laneAxes, colors) {
  const canvas = document.querySelector("[data-trace-canvas]");
  if (!canvas) return [];
  const width = state.width;
  const ctx = canvas.getContext("2d");
  if (!ctx) return [];
  const hits = [];
  const height = layout.totalHeight;
  const dpr = devicePixelRatio || 1;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, width, height);
  const scale = state.scale;
  const viewport = traceViewport(state);
  const { u0, u1 } = viewport;
  const spanU = Math.max(u1 - u0, 1e-9);
  const toX = (u) => ((u - u0) / spanU) * width;
  for (const block of layout.blocks) {
    if (block.track.id !== "main") continue;
    const spans = block.turnSpans;
    for (let i = 0; i < spans.length; i++) {
      const start = toX(scale.toU(spans[i].start));
      const end = i + 1 < spans.length ? toX(scale.toU(spans[i + 1].start)) : width;
      if (end < 0 || start > width) continue;
      if (i % 2 === 1) {
        ctx.fillStyle = colors.turnBand;
        ctx.fillRect(
          Math.max(0, start),
          block.y0,
          Math.min(width, end) - Math.max(0, start),
          block.y1 - block.y0,
        );
      }
      if (start >= 0 && start <= width) {
        ctx.strokeStyle = colors.grid;
        ctx.beginPath();
        ctx.moveTo(Math.round(start) + 0.5, block.y0);
        ctx.lineTo(Math.round(start) + 0.5, block.y1);
        ctx.stroke();
      }
    }
  }
  ctx.font = `9px ${colors.fontMono}`;
  const gapXs = [];
  for (const gap of scale.gaps) {
    const x = toX(gap.uMid);
    if (x < -24 || x > width + 24) continue;
    gapXs.push(x);
    ctx.save();
    ctx.strokeStyle = colors.grid;
    ctx.setLineDash([2, 4]);
    ctx.beginPath();
    ctx.moveTo(x, RULER_H);
    ctx.lineTo(x, height);
    ctx.stroke();
    ctx.restore();
    ctx.fillStyle = colors.tick;
    ctx.textAlign = "center";
    ctx.fillText(`⋯ ${traceElapsed(gap.t1 - gap.t0)}`, x, 11);
    ctx.textAlign = "left";
  }
  for (const tick of traceTicks(scale, u0, u1, width)) {
    const x = Math.round(toX(tick.u)) + 0.5;
    ctx.strokeStyle = colors.grid;
    ctx.beginPath();
    if (tick.major) {
      ctx.moveTo(x, 4);
      ctx.lineTo(x, height);
    } else {
      ctx.moveTo(x, RULER_H - 8);
      ctx.lineTo(x, RULER_H);
    }
    ctx.stroke();
    ctx.fillStyle = colors.tick;
    if (tick.major) {
      if (!gapXs.some((gapX) => Math.abs(gapX - x) < 44))
        ctx.fillText(tick.label, x + 4, 11);
    } else if (!gapXs.some((gapX) => Math.abs(gapX - x) < 44)) {
      ctx.globalAlpha = 0.75;
      ctx.fillText(tick.label, x + 3, RULER_H - 10);
      ctx.globalAlpha = 1;
    }
  }
  ctx.strokeStyle = colors.grid;
  ctx.beginPath();
  ctx.moveTo(0, RULER_H + 0.5);
  ctx.lineTo(width, RULER_H + 0.5);
  ctx.stroke();
  ctx.font = `10px ${colors.fontSans}`;
  const matchIds = traceMatchIds(view);
  for (let index = 0; index < layout.lanes.length; index++) {
    const lane = layout.lanes[index];
    const { startU, endU, maxEndU } = laneAxes[index];
    for (let i = traceFirstVisible(maxEndU, u0); i < lane.spans.length; i++) {
      if (startU[i] > u1) break;
      if (endU[i] < u0) continue;
      const span = lane.spans[i];
      const x = toX(startU[i]);
      const w = Math.max(MIN_SPAN_PX, toX(endU[i]) - x);
      const y = lane.y;
      const alpha = matchIds === null || matchIds.has(span.id) ? 1 : DIM_ALPHA;
      const color = colors.category[span.kind];
      ctx.globalAlpha = alpha;
      ctx.fillStyle = color;
      traceSpanPath(ctx, x, y, w, LANE_H);
      ctx.fill();
      if (span.isError) {
        ctx.fillStyle = colors.errorSoft;
        traceSpanPath(ctx, x, y, w, LANE_H);
        ctx.fill();
        ctx.fillStyle = colors.error;
        ctx.fillRect(x, y, 2, LANE_H);
      }
      if (span.kind === "model" && typeof span.ttft === "number" && span.ttft > 0 && w > 20) {
        const ttftX = toX(scale.toU(span.start + span.ttft));
        if (ttftX > x + 1 && ttftX < x + w - 1) {
          ctx.fillStyle = "rgba(0,0,0,0.5)";
          ctx.fillRect(ttftX, y + 3, 1, LANE_H - 6);
        }
      }
      if (w > LABEL_MIN_PX) {
        ctx.save();
        ctx.beginPath();
        ctx.rect(x + 3, y, w - 6, LANE_H);
        ctx.clip();
        ctx.fillStyle = colors.spanText;
        ctx.fillText(span.label, x + 5, y + LANE_H - 5);
        ctx.restore();
      }
      ctx.globalAlpha = 1;
      const hitW = Math.max(w, HIT_SLOP_PX);
      hits.push({
        kind: "span",
        x: x - (hitW - w) / 2,
        y,
        w: hitW,
        h: LANE_H,
        span,
        track: lane.track,
      });
    }
  }
  for (const block of layout.blocks) {
    for (const marker of block.track.markers) {
      const x = toX(scale.toU(marker.time));
      if (x < -6 || x > width + 6) continue;
      const cy = block.headerY + HEADER_H / 2;
      ctx.fillStyle = colors.marker;
      ctx.beginPath();
      ctx.moveTo(x, cy - 4);
      ctx.lineTo(x + 4, cy);
      ctx.lineTo(x, cy + 4);
      ctx.lineTo(x - 4, cy);
      ctx.closePath();
      ctx.fill();
      hits.push({ kind: "marker", x: x - 5, y: cy - 5, w: 10, h: 10, marker, track: block.track });
    }
  }
  return hits;
}
function traceMatchIds(view) {
  if (!query.trim()) return null;
  return new Set(view.matches.map(({ span }) => span.id));
}
function traceViewport(state) {
  if (!state.viewport) return { u0: state.scale.domain[0], u1: state.scale.domain[1] };
  return state.viewport;
}
function traceClampViewport(state, u0, u1) {
  const [d0, d1] = state.scale.domain;
  let span = Math.max(MIN_WINDOW_U, u1 - u0);
  span = Math.min(span, d1 - d0 || MIN_WINDOW_U);
  let start = u0;
  if (start < d0) start = d0;
  if (start + span > d1) start = d1 - span;
  return { u0: start, u1: start + span };
}
function traceSpanBox(state, layout, span, track) {
  const viewport = traceViewport(state);
  const u0 = state.scale.toU(span.start);
  const u1 = state.scale.toU(span.end);
  if (u0 > viewport.u1 || u1 < viewport.u0) return null;
  const lane = layout.lanes.find(
    (candidate) => candidate.track === track && candidate.kind === span.kind,
  );
  if (!lane) return null;
  const spanU = Math.max(viewport.u1 - viewport.u0, 1e-9);
  const x = ((u0 - viewport.u0) / spanU) * state.width;
  const end = ((u1 - viewport.u0) / spanU) * state.width;
  return { x, y: lane.y, w: Math.max(MIN_SPAN_PX, end - x) };
}

/* ----------------------------- 绘制与交互 ----------------------------- */

function traceDrawMinimap(state, view, colors) {
  const canvas = document.querySelector("[data-trace-minimap]");
  if (!canvas) return;
  const width = Math.max(100, Math.floor(canvas.parentElement?.clientWidth || 800));
  const height = 44;
  const dpr = devicePixelRatio || 1;
  canvas.width = Math.floor(width * dpr);
  canvas.height = Math.floor(height * dpr);
  canvas.style.height = `${height}px`;
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, width, height);
  const strips = ["model", "tool", "subagent"];
  const [d0, d1] = state.scale.domain;
  const domain = Math.max(d1 - d0, 1e-9);
  const buckets = strips.map(() => new Float32Array(width));
  for (const track of view.tracks)
    for (const span of track.spans) {
      const index = strips.indexOf(span.kind);
      if (index === -1) continue;
      const strip = buckets[index];
      const from = Math.max(0, Math.floor(((state.scale.toU(span.start) - d0) / domain) * width));
      const to = Math.min(width - 1, Math.ceil(((state.scale.toU(span.end) - d0) / domain) * width));
      for (let x = from; x <= to; x++) strip[x] = Math.min(1, strip[x] + 0.34);
    }
  const stripHeight = height / strips.length;
  strips.forEach((kind, index) => {
    const strip = buckets[index];
    ctx.fillStyle = colors.category[kind];
    for (let x = 0; x < width; x++) {
      if (strip[x] <= 0) continue;
      ctx.globalAlpha = 0.25 + strip[x] * 0.75;
      ctx.fillRect(x, index * stripHeight + 1, 1, stripHeight - 2);
    }
    ctx.globalAlpha = 1;
  });
  const viewport = traceViewport(state);
  const x0 = ((viewport.u0 - d0) / domain) * width;
  const x1 = ((viewport.u1 - d0) / domain) * width;
  ctx.fillStyle = colors.brush;
  ctx.fillRect(x0, 0, x1 - x0, height);
  ctx.strokeStyle = colors.selection;
  ctx.strokeRect(x0 + 0.5, 0.5, x1 - x0 - 1, height - 1);
  ctx.fillStyle = colors.selection;
  ctx.fillRect(x0, 0, 2, height);
  ctx.fillRect(x1 - 2, 0, 2, height);
}
function traceDrawOverlays(state, view, layout) {
  const hover = document.querySelector("[data-trace-hover]");
  const selection = document.querySelector("[data-trace-selection]");
  const selected = traceSelected(view, state);
  const matchIds = traceMatchIds(view);
  if (hover) {
    const hit = state.hover;
    const span = hit?.kind === "span" ? hit : null;
    const box = span ? traceSpanBox(state, layout, span.span, span.track) : null;
    if (!box) hover.hidden = true;
    else {
      hover.hidden = false;
      hover.style.left = `${box.x}px`;
      hover.style.top = `${box.y}px`;
      hover.style.width = `${box.w}px`;
      hover.style.height = `${LANE_H}px`;
      hover.style.opacity = matchIds === null || matchIds.has(span.span.id) ? "1" : String(DIM_ALPHA);
    }
  }
  if (selection) {
    const box = selected ? traceSpanBox(state, layout, selected.span, selected.track) : null;
    if (!box) selection.hidden = true;
    else {
      selection.hidden = false;
      selection.style.left = `${box.x - 1}px`;
      selection.style.top = `${box.y - 1}px`;
      selection.style.width = `${box.w + 2}px`;
      selection.style.height = `${LANE_H + 2}px`;
      selection.style.borderColor = traceColors().selection;
    }
  }
}
function traceRedraw() {
  if (!traceState || !traceView || page !== "traces" || !traceRequest) return;
  const state = traceState;
  const layout = state.layout;
  if (!layout) return;
  state.hits = traceDrawScene(state, traceView, layout, state.laneAxes, traceColors());
  traceDrawMinimap(state, traceView, traceColors());
  traceDrawOverlays(state, traceView, layout);
  const count = document.querySelector("[data-trace-match-count]");
  if (count) {
    const matches = traceView.matches.length;
    const index = state.matchIndex % Math.max(matches, 1);
    count.textContent = matches === 0 ? "0" : `${index + 1}/${matches}`;
  }
}
function traceRenderGutter(state, layout) {
  const gutter = document.querySelector("[data-trace-gutter]");
  if (!gutter) return;
  gutter.style.height = `${layout.totalHeight}px`;
  gutter.innerHTML = layout.blocks
    .map((block) => {
      const chevron = `<button type="button" class="traces-chevron" data-trace-chevron="${h(
        block.track.id,
      )}" data-tt-focus="chevron:${h(block.track.id)}" aria-label="${
        state.collapsed.has(block.track.id) ? "Expand" : "Collapse"
      } ${h(block.track.label)}">${icon(
        state.collapsed.has(block.track.id) ? "chevron" : "down",
        12,
      )}</button>`;
      return `<div><div class="traces-gutter-header" style="top:${block.headerY}px;left:${
        4 + block.depth * 14
      }px">${chevron}<span class="traces-gutter-track x-truncate" title="${h(
        block.track.label,
      )}">${h(block.track.label)}</span>${
        block.track.model
          ? `<span class="traces-gutter-model x-mono x-truncate" title="${h(
              block.track.model,
            )}">${h(block.track.model)}</span>`
          : ""
      }</div>${block.lanes
        .map(
          (lane) =>
            `<div class="traces-gutter-lane" style="top:${lane.y}px;left:${
              20 + block.depth * 14
            }px">${lane.label}</div>`,
        )
        .join("")}</div>`;
    })
    .join("");
}
function traceTooltip() {
  let tooltip = document.getElementById("trace-tooltip");
  if (!tooltip) {
    tooltip = document.createElement("div");
    tooltip.id = "trace-tooltip";
    tooltip.className = "traces-tooltip";
    tooltip.hidden = true;
    document.body.append(tooltip);
  }
  return tooltip;
}
function traceTooltipHide() {
  const tooltip = document.getElementById("trace-tooltip");
  if (tooltip) tooltip.hidden = true;
}
function traceShowTooltip(hit, state, view, clientX, clientY) {
  const tooltip = traceTooltip();
  if (hit.kind === "marker") {
    tooltip.innerHTML = `<div class="traces-tooltip-title">${h(
      hit.marker.label,
    )}</div><div class="x-muted x-num">${traceClock(hit.marker.time)} (${traceOffset(
      hit.marker.time - view.start,
    )})</div>${hit.marker.detail ? `<div class="traces-tooltip-detail">${h(hit.marker.detail)}</div>` : ""}`;
  } else {
    const span = hit.span;
    const start = traceBounds(view.tracks)[0];
    tooltip.innerHTML = `<div class="traces-tooltip-title x-truncate">${h(
      span.label,
    )}</div><div class="x-muted x-num">${traceDurationMs(
      span.end - span.start,
    )} · ${traceClock(span.start)} (${traceOffset(span.start - start)})${
      span.unterminated ? " · unterminated" : ""
    }</div>${
      span.kind === "model"
        ? `<div class="x-muted x-num">${traceInteger(span.tokens ?? 0)} tok · ${traceCost(
            span.cost ?? 0,
            4,
          )}${span.ttft != null ? ` · TTFT ${traceDurationMs(span.ttft)}` : ""}${
            span.isError ? " · error" : ""
          }</div>`
        : ""
    }${span.kind === "subagent" && span.model ? `<div class="x-muted x-num">${h(span.model)}</div>` : ""}${
      span.detail ? `<div class="traces-tooltip-detail">${h(span.detail)}</div>` : ""
    }`;
  }
  tooltip.hidden = false;
  const box = tooltip.getBoundingClientRect();
  tooltip.style.left = `${Math.max(8, Math.min(clientX + 14, innerWidth - box.width - 8))}px`;
  tooltip.style.top = `${
    clientY + 16 + box.height > innerHeight - 8 ? clientY - box.height - 12 : clientY + 16
  }px`;
}
function traceSelect(view, state, id, open) {
  state.selection = id;
  state.drawer = Boolean(open && id);
  const selected = traceSelected(view, state);
  if (state.drawer && selected?.span.kind === "model")
    traceLoadRequestDetail(selected.span.requestId);
  traceRenderDrawer(view, state);
  traceDrawOverlays(state, view, state.layout);
  traceUpdateTranscriptSelection(view, state);
  if (selected) traceCenterOnSpan(state, selected.span);
  if (!state.drawer) {
    const row = state.selection
      ? document.querySelector(`[data-trace-row="${CSS.escape(state.selection)}"]`)
      : null;
    (row || document.querySelector("[data-trace-canvas]"))?.focus({
      preventScroll: true,
    });
  }
}
function traceCenterOnSpan(state, span) {
  const u = state.scale.toU(span.start);
  const end = state.scale.toU(span.end);
  const viewport = traceViewport(state);
  if (u >= viewport.u0 && end <= viewport.u1) return;
  const window = viewport.u1 - viewport.u0;
  const mid = (u + end) / 2;
  const [d0, d1] = state.scale.domain;
  let start = mid - window / 2;
  if (start < d0) start = d0;
  if (start + window > d1) start = Math.max(d0, d1 - window);
  state.viewport = { u0: start, u1: start + window };
  traceRedraw();
}
function traceUpdateTranscriptSelection(view, state) {
  const list = document.querySelector("[data-trace-list]");
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
  for (const row of list.querySelectorAll("[data-trace-row]"))
    row.dataset.selected = String(row.dataset.traceRow === state.selection);
}
function traceTranscriptRow(row, view, state) {
  const span = row.span;
  const marker = row.marker;
  const kind = span ? span.kind : marker ? "marker" : "";
  const color = span ? `var(${TRACE_KIND_COLORS[span.kind]})` : "var(--ink-1)";
  const label = span ? span.label : marker.label;
  const detail = span ? span.detail : undefined;
  const duration = span ? traceDurationMs(span.end - span.start) : "";
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
  }<span>${duration}</span><span>${traceOffset(row.time - view.start)}</span></span>`;
  const selected = span && span.id === state.selection;
  return span
    ? `<button type="button" class="traces-row" data-trace-row="${h(
        span.id,
      )}" data-selected="${selected}" style="${indent}">${content}</button>`
    : `<div class="traces-row" data-static="true" style="${indent}">${content}</div>`;
}
function traceRenderTranscriptWindow(view, state) {
  const list = document.querySelector("[data-trace-list]");
  if (!list) return;
  const spacer = list.querySelector("[data-trace-spacer]");
  const windowEl = list.querySelector("[data-trace-window]");
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
function traceZoomAt(state, factor, anchorPx, width) {
  const { u0, u1 } = traceViewport(state);
  const span = u1 - u0;
  const fraction = anchorPx === null ? 0.5 : Math.min(1, Math.max(0, anchorPx / width));
  const anchor = u0 + span * fraction;
  const next = span * factor;
  state.viewport = traceClampViewport(
    state,
    anchor - next * fraction,
    anchor + next * (1 - fraction),
  );
  traceRedraw();
}
function tracePanBy(state, delta) {
  const { u0, u1 } = traceViewport(state);
  state.viewport = traceClampViewport(state, u0 + delta, u1 + delta);
  traceRedraw();
}
function traceZoomToSpan(state, span) {
  const from = state.scale.toU(span.start);
  const to = state.scale.toU(span.end);
  const pad = Math.max((to - from) * 0.1, MIN_WINDOW_U / 2);
  state.viewport = traceClampViewport(state, from - pad, to + pad);
  traceRedraw();
}
function traceWireCanvas(view, state, layout) {
  const canvas = document.querySelector("[data-trace-canvas]");
  const stack = document.querySelector("[data-trace-stack]");
  if (!canvas || !stack) return;
  const colors = traceColors();
  const width = Math.max(100, Math.floor(stack.clientWidth));
  state.width = width;
  state.layout = layout;
  canvas.style.width = `${width}px`;
  canvas.style.height = `${layout.totalHeight}px`;
  canvas.width = Math.floor(width * (devicePixelRatio || 1));
  canvas.height = Math.floor(layout.totalHeight * (devicePixelRatio || 1));
  state.laneAxes = traceLaneAxes(layout, state.scale);
  state.hits = traceDrawScene(state, view, layout, state.laneAxes, colors);
  let drag = null;
  const localX = (event) => event.clientX - canvas.getBoundingClientRect().left;
  const hitAt = (event) => {
    const box = canvas.getBoundingClientRect();
    const x = event.clientX - box.left;
    const y = event.clientY - box.top;
    for (let index = state.hits.length - 1; index >= 0; index--) {
      const hit = state.hits[index];
      if (x >= hit.x && x <= hit.x + hit.w && y >= hit.y && y <= hit.y + hit.h) return hit;
    }
    return null;
  };
  canvas.onpointerdown = (event) => {
    canvas.setPointerCapture(event.pointerId);
    drag = { pointerId: event.pointerId, lastX: event.clientX, moved: false };
    canvas.focus({ preventScroll: true });
  };
  canvas.onpointermove = (event) => {
    if (drag && drag.pointerId === event.pointerId) {
      const delta = event.clientX - drag.lastX;
      if (delta) {
        if (Math.abs(delta) > 2) drag.moved = true;
        drag.lastX = event.clientX;
        const span = traceViewport(state).u1 - traceViewport(state).u0;
        tracePanBy(state, (-delta / (width || 1)) * span);
      }
      state.hover = null;
      traceTooltipHide();
      return;
    }
    const hit = hitAt(event);
    state.hover = hit;
    traceDrawOverlays(state, view, layout);
    if (hit) traceShowTooltip(hit, state, view, event.clientX, event.clientY);
    else traceTooltipHide();
  };
  canvas.onpointerup = (event) => {
    const current = drag;
    drag = null;
    if (current?.moved) return;
    const hit = hitAt(event);
    if (hit?.kind === "span") traceSelect(view, state, hit.span.id, true);
    else traceSelect(view, state, null, false);
  };
  canvas.onpointerleave = () => {
    state.hover = null;
    traceDrawOverlays(state, view, layout);
    traceTooltipHide();
  };
  canvas.ondblclick = (event) => {
    const hit = hitAt(event);
    if (hit?.kind === "span") traceZoomToSpan(state, hit.span);
  };
  canvas.onwheel = (event) => {
    event.preventDefault();
    const x = localX(event);
    state.hover = null;
    traceDrawOverlays(state, view, layout);
    const span = traceViewport(state).u1 - traceViewport(state).u0;
    if (event.shiftKey) {
      const delta = Math.abs(event.deltaY) >= Math.abs(event.deltaX) ? event.deltaY : event.deltaX;
      tracePanBy(state, (delta / canvas.clientWidth) * span);
      return;
    }
    if (event.deltaX !== 0) tracePanBy(state, (event.deltaX / canvas.clientWidth) * span);
    if (event.deltaY !== 0) traceZoomAt(state, 1.0015 ** event.deltaY, x, canvas.clientWidth);
  };
  canvas.onkeydown = (event) => {
    const span = traceViewport(state).u1 - traceViewport(state).u0;
    switch (event.key) {
      case "w":
      case "W":
        traceZoomAt(state, 1 / 1.3, null, canvas.clientWidth);
        break;
      case "s":
      case "S":
        traceZoomAt(state, 1.3, null, canvas.clientWidth);
        break;
      case "a":
      case "A":
        tracePanBy(state, -span * 0.2);
        break;
      case "d":
      case "D":
        tracePanBy(state, span * 0.2);
        break;
      case "0":
        state.viewport = null;
        traceRedraw();
        break;
      case "f":
      case "F": {
        const selected = traceSelected(view, state);
        if (selected) traceZoomToSpan(state, selected.span);
        break;
      }
      case "Escape":
        traceSelect(view, state, null, false);
        break;
      default:
        return;
    }
    event.preventDefault();
    event.stopPropagation();
  };
  state.observer?.disconnect();
  state.observer = new ResizeObserver(() => {
    const next = Math.max(100, Math.floor(stack.clientWidth));
    if (next === state.width) return;
    state.width = next;
    canvas.style.width = `${next}px`;
    canvas.width = Math.floor(next * (devicePixelRatio || 1));
    state.hits = traceDrawScene(state, view, layout, state.laneAxes, traceColors());
    traceDrawMinimap(state, view, traceColors());
    traceDrawOverlays(state, view, layout);
  });
  state.observer.observe(stack);
  const minimap = document.querySelector("[data-trace-minimap]");
  if (minimap) {
    let minimapDrag = null;
    const point = (event) => {
      const box = minimap.getBoundingClientRect();
      return { x: event.clientX - box.left, width: box.width };
    };
    const toU = (x, width) => {
      const [d0, d1] = state.scale.domain;
      return d0 + (x / Math.max(width, 1)) * (d1 - d0);
    };
    minimap.onpointerdown = (event) => {
      const { x, width: mw } = point(event);
      const [d0, d1] = state.scale.domain;
      const viewport = traceViewport(state);
      const x0 = ((viewport.u0 - d0) / Math.max(d1 - d0, 1e-9)) * mw;
      const x1 = ((viewport.u1 - d0) / Math.max(d1 - d0, 1e-9)) * mw;
      let mode = "create";
      if (Math.abs(x - x0) <= 6) mode = "left";
      else if (Math.abs(x - x1) <= 6) mode = "right";
      else if (x > x0 && x < x1) mode = "move";
      else {
        const u = toU(x, mw);
        state.viewport = traceClampViewport(state, u, u + MIN_WINDOW_U);
        traceRedraw();
      }
      minimapDrag = { mode, startX: x, startU: toU(x, mw), start: viewport, width: mw };
      minimap.setPointerCapture(event.pointerId);
    };
    minimap.onpointermove = (event) => {
      if (!minimapDrag) return;
      const { x, width: mw } = point(event);
      const delta = toU(x, mw) - minimapDrag.startU;
      const { u0, u1 } = minimapDrag.start;
      const [d0, d1] = state.scale.domain;
      let next;
      if (minimapDrag.mode === "move") next = { u0: u0 + delta, u1: u1 + delta };
      else if (minimapDrag.mode === "left")
        next = { u0: Math.min(u0 + delta, u1 - MIN_WINDOW_U), u1 };
      else if (minimapDrag.mode === "right")
        next = { u0, u1: Math.max(u1 + delta, u0 + MIN_WINDOW_U) };
      else {
        const anchor = toU(minimapDrag.startX, mw);
        const current = toU(x, mw);
        next = {
          u0: Math.min(anchor, current),
          u1: Math.max(anchor, current, Math.min(anchor, current) + MIN_WINDOW_U),
        };
      }
      state.viewport = traceClampViewport(state, next.u0, Math.min(next.u1, d1));
      state.viewport = traceClampViewport(state, state.viewport.u0, Math.max(state.viewport.u1, state.viewport.u0 + MIN_WINDOW_U));
      traceRedraw();
    };
    minimap.onpointerup = () => {
      minimapDrag = null;
    };
  }
}

/* ----------------------------- 接线 ----------------------------- */

let traceListenersBound = false;
function traceBindPageListeners() {
  if (traceListenersBound) return;
  traceListenersBound = true;
  $("page").addEventListener("keydown", (event) => {
    if (event.key !== "Enter" && event.key !== " " && event.key !== "Spacebar") return;
    const action = event.target.closest(
      "tr[data-trace-open],th[data-ttsort],th[data-trace-tools-sort]",
    );
    if (!action || event.target.closest("button,a,input,select,textarea")) return;
    event.preventDefault();
    action.click();
  });
}

function wireTraces() {
  traceBindPageListeners();
  traceTooltipHide();
  if (!data || !traceRequest) return;
  const view = traceView && traceView.file === (data.session_file || "") ? traceView : traceViewModel();
  traceView = view;
  const state = traceStateFor(view);
  state.scale = traceScale(view.tracks, state.mode, state.compress);
  const layout = traceLayout(view.tracks, state.collapsed);
  state.layout = layout;
  traceRenderGutter(state, layout);
  traceWireCanvas(view, state, layout);
  traceDrawMinimap(state, view, traceColors());
  traceDrawOverlays(state, view, layout);
  traceRenderDrawer(view, state);
  const timeline = document.querySelector("[data-trace-timeline]");
  if (timeline) {
    timeline.scrollTop = state.timelineScroll;
    timeline.onscroll = () => {
      state.timelineScroll = timeline.scrollTop;
    };
  }
  const list = document.querySelector("[data-trace-list]");
  if (list) {
    list.onscroll = () => {
      state.transcriptScroll = list.scrollTop;
      traceRenderTranscriptWindow(view, state);
    };
    list.scrollTop = state.transcriptScroll;
    traceRenderTranscriptWindow(view, state);
  }
  const canvas = document.querySelector("[data-trace-canvas]");
  if (!state.focusedCanvas && !state.drawer && canvas) {
    state.focusedCanvas = true;
    canvas.focus({ preventScroll: true });
  }
  if (traceFocusKey) {
    const element = [...document.querySelectorAll("[data-tt-focus]")].find(
      (candidate) => candidate.dataset.ttFocus === traceFocusKey,
    );
    element?.focus({ preventScroll: true });
    traceFocusKey = null;
  }
}
function tracesRerender() {
  const active = document.activeElement?.closest?.("[data-tt-focus]");
  traceFocusKey = active?.dataset.ttFocus ?? null;
  traceRememberScroll();
  render();
}
function tracesClick(event) {
  const target = event.target.closest(
    "[data-trace-open],[data-trace-back],[data-trace-refresh],[data-ttsort],[data-trace-more],[data-trace-match],[data-trace-mode],[data-trace-compress],[data-trace-chevron],[data-trace-expand-all],[data-trace-collapse-all],[data-trace-row],[data-trace-close],[data-trace-copy],[data-trace-open-request],[data-trace-open-child],[data-trace-tools-sort],[data-trace-tools-more]",
  );
  if (!target) return false;
  const state = traceState;
  const view = traceView;
  const d = target.dataset;
  if (d.traceOpen !== undefined) {
    location.hash = `/traces?${new URLSearchParams({ range, session: d.traceOpen })}`;
    return true;
  }
  if ("traceBack" in d) {
    location.hash = `/traces?range=${range}`;
    return true;
  }
  if ("traceRefresh" in d) {
    traceDetails.clear();
    traceRenderedSignature = "";
    refresh();
    return true;
  }
  if (d.ttsort !== undefined) {
    const key = d.ttsort;
    if (traceSort.key === key)
      traceSort = { key, dir: traceSort.dir === "asc" ? "desc" : "asc" };
    else traceSort = { key, dir: d.ttalign === "right" ? "desc" : "asc" };
    tracesRerender();
    return true;
  }
  if (d.traceMore !== undefined) {
    traceShown = d.traceMore === "more" ? traceShown + TRACE_LIST_LIMIT * 4 : TRACE_LIST_LIMIT;
    tracesRerender();
    return true;
  }
  if (d.traceMode !== undefined) {
    state.mode = d.traceMode;
    state.viewport = null;
    tracesRerender();
    return true;
  }
  if (d.traceCompress !== undefined) {
    state.compress = target.checked;
    state.viewport = null;
    tracesRerender();
    return true;
  }
  if (d.traceMatch !== undefined) {
    if (!view || !view.matches.length) return true;
    state.matchIndex =
      (state.matchIndex + Number(d.traceMatch) + view.matches.length) % view.matches.length;
    const span = view.matches[state.matchIndex].span;
    traceSelect(view, state, span.id, false);
    return true;
  }
  if (d.traceChevron !== undefined) {
    if (state.collapsed.has(d.traceChevron)) state.collapsed.delete(d.traceChevron);
    else state.collapsed.add(d.traceChevron);
    tracesRerender();
    return true;
  }
  if ("traceExpandAll" in d) {
    state.collapsed = new Set();
    tracesRerender();
    return true;
  }
  if ("traceCollapseAll" in d) {
    state.collapsed = new Set(view.tracks.filter((track) => track.id !== "main").map((track) => track.id));
    tracesRerender();
    return true;
  }
  if (d.traceRow !== undefined) {
    traceSelect(view, state, d.traceRow, true);
    return true;
  }
  if ("traceClose" in d) {
    traceSelect(view, state, state.selection, false);
    return true;
  }
  if ("traceCopy" in d) {
    const selected = traceSelected(view, state);
    if (!selected) return true;
    const detail = traceDetails.get(selected.span.requestId);
    const payload = detail?.data ?? selected.span;
    navigator.clipboard
      ?.writeText(JSON.stringify(payload, null, 2))
      .then(() => {
        target.innerHTML = icon("check", 15);
        setTimeout(() => (target.innerHTML = icon("copy", 15)), 1500);
      })
      .catch(() => {});
    return true;
  }
  if (d.traceOpenRequest !== undefined) {
    traceSelect(view, state, state.selection, false);
    openDrawer(Number(d.traceOpenRequest), target);
    return true;
  }
  if (d.traceOpenChild !== undefined) {
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
    if (traceToolSort?.key === key)
      traceToolSort = { key, dir: traceToolSort.dir === "asc" ? "desc" : "asc" };
    else traceToolSort = { key, dir: target.dataset.align === "right" ? "desc" : "asc" };
    tracesRerender();
    return true;
  }
  if (d.traceToolsMore !== undefined) {
    traceToolShown = d.traceToolsMore === "more" ? traceToolShown + 48 : 12;
    tracesRerender();
    return true;
  }
  return false;
}

function traceLoadRequestDetail(id) {
  if (!id || traceDetails.has(id)) return;
  traceDetails.set(id, { pending: true });
  getJSON(`/api/stats/requests/${id}`)
    .then((detail) => traceDetails.set(id, { data: detail }))
    .catch((error) => traceDetails.set(id, { error: error.message }))
    .finally(() => {
      if (page === "traces" && traceRequest && traceView)
        traceRenderDrawer(traceView, traceState);
    });
}

document.addEventListener("keydown", (event) => {
  if (event.key !== "Escape" || !traceState?.drawer) return;
  if (!$("overlay").hidden) return;
  event.preventDefault();
  traceSelect(traceView, traceState, traceState.selection, false);
});
new MutationObserver(() => {
  if (page === "traces" && traceRequest && traceState?.layout) traceRedraw();
}).observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
