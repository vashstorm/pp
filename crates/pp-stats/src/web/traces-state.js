/**
 * Traces 模块共享状态、通用格式辅助与子模块回调桥接。
 */

import { fixedCost, h } from "./format.js";

/** @typedef {import("../../web-types/api.d.ts").RequestDetailResponse} RequestDetailResponse */
/** @typedef {import("../../web-types/api.d.ts").SortState} SortState */
/** @typedef {import("../../web-types/api.d.ts").TraceDetailSummary} TraceDetailSummary */

export const TRACE_ROW_H = 28;
export const TRACE_LIST_H = 340;
export const TRACE_LIST_LIMIT = 50;

/** @type {Record<"model" | "tool" | "subagent", string>} */
export const TRACE_KIND_COLORS = {
  model: "--chart-primary",
  tool: "--warn",
  subagent: "--chart-secondary",
};

export const TRACE_MODE_OPTIONS = [
  { value: "duration", label: "Duration", title: "Real wall-clock time" },
  { value: "requests", label: "Requests", title: "Equal width per model request" },
  { value: "calls", label: "Calls", title: "Equal width per model/tool call boundary" },
];

/** @type {Array<{ kind: "model" | "tool" | "subagent", label: string }>} */
export const TRACE_LANES = [
  { kind: "model", label: "Model" },
  { kind: "tool", label: "Tools" },
  { kind: "subagent", label: "Agents" },
];

/**
 * @typedef {{
 *   id: string,
 *   kind: "model" | "tool" | "subagent",
 *   track: string,
 *   start: number,
 *   end: number,
 *   label: string,
 *   detail: string,
 *   isError: boolean,
 *   status?: string,
 *   requestId?: number | null,
 *   model?: string,
 *   provider?: string,
 *   tokens?: number,
 *   cost?: number | null,
 *   ttft?: number | null,
 *   stopReason?: string | null,
 *   errorMessage?: string | null,
 *   inputTokens?: number | null,
 *   outputTokens?: number | null,
 *   cachedTokens?: number | null,
 *   usageKnown?: boolean,
 *   tool?: string,
 *   callId?: string,
 *   childTrackId?: string,
 *   unterminated?: boolean,
 * }} TraceSpan
 */

/**
 * @typedef {{
 *   time: number,
 *   label: string,
 *   detail: string,
 * }} TraceMarker
 */

/**
 * @typedef {{
 *   id: string,
 *   label: string,
 *   file: string,
 *   model: string,
 *   depth: number,
 *   parentId?: string | null,
 *   spans: TraceSpan[],
 *   markers: TraceMarker[],
 * }} TraceTrack
 */

/**
 * @typedef {{
 *   key: string,
 *   time: number,
 *   span?: TraceSpan,
 *   marker?: TraceMarker,
 *   track: TraceTrack,
 *   depth: number,
 * }} TraceTranscriptRow
 */

/**
 * @typedef {{
 *   file: string,
 *   id: number,
 *   cwd: string,
 *   summary: Partial<TraceDetailSummary>,
 *   tracks: TraceTrack[],
 *   spans: Array<{ span: TraceSpan, track: TraceTrack }>,
 *   matches: Array<{ span: TraceSpan, track: TraceTrack }>,
 *   rows: TraceTranscriptRow[],
 *   start: number,
 *   title: string,
 * }} TraceViewModel
 */

/**
 * @typedef {{
 *   toU: (t: number) => number,
 *   toT: (u: number) => number,
 *   domain: [number, number],
 *   gaps: Array<{ t0: number, t1: number, uMid: number }>,
 * }} TraceScale
 */

/**
 * @typedef {{
 *   track: TraceTrack,
 *   kind: "model" | "tool" | "subagent",
 *   label: string,
 *   y: number,
 *   spans: TraceSpan[],
 * }} TraceLayoutLane
 */

/**
 * @typedef {{
 *   track: TraceTrack,
 *   depth: number,
 *   headerY: number,
 *   lanes: TraceLayoutLane[],
 *   turnSpans: TraceSpan[],
 *   y0: number,
 *   y1: number,
 * }} TraceLayoutBlock
 */

/**
 * @typedef {{
 *   blocks: TraceLayoutBlock[],
 *   lanes: TraceLayoutLane[],
 *   totalHeight: number,
 * }} TraceLayout
 */

/**
 * @typedef {({ kind: "span", x: number, y: number, w: number, h: number, span: TraceSpan, track: TraceTrack }
 *   | { kind: "marker", x: number, y: number, w: number, h: number, marker: TraceMarker, track: TraceTrack })} TraceHit
 */

/**
 * @typedef {{
 *   file: string,
 *   initialized?: boolean,
 *   mode: "duration" | "requests" | "calls",
 *   compress: boolean,
 *   selection: string | null,
 *   drawer: boolean,
 *   matchIndex: number,
 *   collapsed: Set<string>,
 *   viewport: { u0: number, u1: number } | null,
 *   transcriptScroll: number,
 *   timelineScroll: number,
 *   layout: TraceLayout | null,
 *   laneAxes?: Array<{ startU: number[], endU: number[], maxEndU: number[] }>,
 *   scale: TraceScale | null,
 *   width: number,
 *   hits: TraceHit[],
 *   hover: TraceHit | null,
 *   observer?: ResizeObserver,
 *   focusedCanvas?: boolean,
 * }} TraceState
 */

/** @type {{
 *   request: string | null,
 *   renderedSignature: string,
 *   sort: SortState,
 *   shown: number,
 *   toolSort: SortState | null,
 *   toolShown: number,
 *   state: TraceState | null,
 *   view: TraceViewModel | null,
 *   focusKey: string | null,
 *   details: Map<number, { pending?: boolean, data?: RequestDetailResponse, error?: string }>,
 * }} */
export const traceStore = {
  request: null,
  renderedSignature: "",
  sort: { key: "started", dir: "desc" },
  shown: TRACE_LIST_LIMIT,
  toolSort: null,
  toolShown: 12,
  state: null,
  view: null,
  focusKey: null,
  details: new Map(),
};

/** @type {{
 *   redraw: () => void,
 *   onClick: (event: MouseEvent) => boolean,
 *   select: (view: TraceViewModel, state: TraceState, id: string | null, open: boolean) => void,
 * }} */
export const traceBridge = {
  redraw: () => {},
  onClick: () => false,
  select: () => {},
};

/**
 * 格式化 Trace 毫秒耗时。
 * @param {number | null | undefined} value
 * @param {number} [digits]
 * @returns {string}
 */
export const traceDurationMs = (value, digits) => {
  if (value == null) return "-";
  const seconds = value / 1000;
  const precision = digits !== undefined ? digits : seconds < 1 ? 2 : 1;
  return `${seconds.toFixed(precision)}s`;
};

/**
 * 格式化超过一分钟的长耗时为 `Xm Ys` 或 `Xh Ym`。
 * @param {number | null | undefined} ms
 * @returns {string}
 */
export function traceElapsed(ms) {
  if (ms == null || !Number.isFinite(ms)) return "—";
  const value = Math.max(0, ms);
  if (value < 60000) return traceDurationMs(value);
  const totalMinutes = Math.floor(value / 60000);
  if (totalMinutes < 60) {
    return `${totalMinutes}m ${String(Math.floor((value % 60000) / 1000)).padStart(2, "0")}s`;
  }
  return `${Math.floor(totalMinutes / 60)}h ${String(totalMinutes % 60).padStart(2, "0")}m`;
}

/**
 * 格式化相对会话起点的偏移量（如 `+01:23.450`）。
 * @param {number} deltaMs
 * @returns {string}
 */
export function traceOffset(deltaMs) {
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

/**
 * 格式化 `HH:MM:SS` 时钟标签。
 * @param {number} timestamp
 * @returns {string}
 */
export function traceClock(timestamp) {
  const date = new Date(timestamp);
  return [date.getHours(), date.getMinutes(), date.getSeconds()]
    .map((part) => String(part).padStart(2, "0"))
    .join(":");
}

/**
 * 提取会话文件 stem。
 * @param {string | null | undefined} file
 * @returns {string}
 */
export function traceFileStem(file) {
  const name =
    String(file || "")
      .split(/[\\/]/)
      .pop() || "";
  return name.replace(/\.jsonl$/, "");
}

/**
 * 提取工作目录末尾两级路径。
 * @param {string | null | undefined} cwd
 * @returns {string}
 */
export function traceFolderTail(cwd) {
  const parts = String(cwd || "")
    .split(/[\\/]/)
    .filter(Boolean);
  return parts.slice(-2).join("/");
}

/**
 * 查找当前选中的 span 与其所属轨道。
 * @param {TraceViewModel | null} view
 * @param {TraceState | null} state
 * @returns {{ span: TraceSpan, track: TraceTrack } | null}
 */
export function traceSelected(view, state) {
  if (!view || !state?.selection) return null;
  for (const track of view.tracks) {
    const span = track.spans.find((candidate) => candidate.id === state.selection);
    if (span) return { span, track };
  }
  return null;
}

/**
 * 记录时间线和 Transcript 滚动条位置以便重绘后恢复。
 */
export function traceRememberScroll() {
  if (!traceStore.state || !traceStore.request) return;
  const timeline = document.querySelector("[data-trace-timeline]");
  const list = document.querySelector("[data-trace-list]");
  if (timeline) traceStore.state.timelineScroll = timeline.scrollTop;
  if (list) traceStore.state.transcriptScroll = list.scrollTop;
}

/**
 * 确保当前会话的交互状态对象存在。
 * @param {string} file
 * @returns {TraceState}
 */
export function traceBegin(file) {
  if (!traceStore.state || traceStore.state.file !== file) {
    traceStore.state = {
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
  return traceStore.state;
}

/**
 * 格式化 Trace 页可空金额，并在未定价时输出 `N/A`。
 * @param {number | null | undefined} value
 * @param {number} unpriced
 * @returns {string}
 */
export const traceEstimatedCost = (value, unpriced) =>
  value === 0 && unpriced > 0 ? "N/A" : fixedCost(value);

export { fixedCost, h };
