/**
 * Traces 纯数学与布局计算：轨道纵向布局、分段线性时间轴压缩与刻度生成（无 DOM 依赖）。
 */

import { TRACE_LANES, traceClock, traceOffset } from "./traces-state.js";

/** @typedef {import("./traces-state.js").TraceLayout} TraceLayout */
/** @typedef {import("./traces-state.js").TraceScale} TraceScale */
/** @typedef {import("./traces-state.js").TraceSpan} TraceSpan */
/** @typedef {import("./traces-state.js").TraceTrack} TraceTrack */

export const RULER_H = 28;
export const HEADER_H = 22;
export const LANE_H = 18;
export const LANE_GAP = 3;
export const TRACK_GAP = 14;

/**
 * 计算轨道与泳道纵向布局。
 * @param {TraceTrack[]} tracks
 * @param {Set<string>} collapsed
 * @returns {TraceLayout}
 */
export function traceLayout(tracks, collapsed) {
  const byId = new Map(tracks.map((track) => [track.id, track]));
  /** @param {TraceTrack} track */
  const hidden = (track) => {
    let parent = track.parentId;
    while (parent) {
      if (collapsed.has(parent)) return true;
      parent = byId.get(parent)?.parentId ?? null;
    }
    return false;
  };
  /** @type {import("./traces-state.js").TraceLayoutBlock[]} */
  const blocks = [];
  /** @type {import("./traces-state.js").TraceLayoutLane[]} */
  const lanes = [];
  let y = RULER_H + 6;
  for (const track of tracks) {
    if (hidden(track)) continue;
    /** @type {Record<"model" | "tool" | "subagent", TraceSpan[]>} */
    const byKind = { model: [], tool: [], subagent: [] };
    for (const span of track.spans) byKind[span.kind].push(span);
    /** @type {import("./traces-state.js").TraceLayoutBlock} */
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
      /** @type {import("./traces-state.js").TraceLayoutLane} */
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

/**
 * 预计算每条泳道在虚拟轴上的边界与前缀最大终点，供视口二分剔除。
 * @param {TraceLayout} layout
 * @param {TraceScale} scale
 */
export function traceLaneAxes(layout, scale) {
  return layout.lanes.map((lane) => {
    /** @type {number[]} */
    const startU = [];
    /** @type {number[]} */
    const endU = [];
    /** @type {number[]} */
    const maxEndU = [];
    let max = Number.NEGATIVE_INFINITY;
    for (const span of lane.spans) {
      startU.push(scale.toU(span.start));
      endU.push(scale.toU(span.end));
      max = Math.max(max, scale.toU(span.end));
      maxEndU.push(max);
    }
    return { startU, endU, maxEndU };
  });
}

/**
 * 二分查找第一个与视口左界 `u0` 相交或在其右侧的 span 下标。
 * @param {number[]} maxEndU
 * @param {number} u0
 * @returns {number}
 */
export function traceFirstVisible(maxEndU, u0) {
  let lo = 0;
  let hi = maxEndU.length;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (maxEndU[mid] < u0) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

/**
 * 构造单调分段线性双向映射 `toU` / `toT`。
 * @param {number[]} tPoints
 * @param {number[]} uPoints
 * @returns {TraceScale}
 */
export function tracePiecewise(tPoints, uPoints) {
  const count = tPoints.length;
  /** @type {[number, number]} */
  const domain = [uPoints[0], uPoints[count - 1]];
  /**
   * @param {number[]} points
   * @param {number} value
   */
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

/**
 * 计算全部轨道跨度的最小与最大时间戳。
 * @param {TraceTrack[]} tracks
 * @returns {[number, number]}
 */
export function traceBounds(tracks) {
  let min = Number.POSITIVE_INFINITY;
  let max = Number.NEGATIVE_INFINITY;
  for (const track of tracks) {
    for (const span of track.spans) {
      if (span.start < min) min = span.start;
      if (span.end > max) max = span.end;
    }
  }
  if (!Number.isFinite(min)) return [0, 1];
  return [min, Math.max(max, min + 1)];
}

/**
 * 为每个锚点分配等宽虚拟坐标。
 * @param {number[]} anchors
 * @param {number} tMin
 * @param {number} tMax
 * @returns {TraceScale}
 */
export function traceEqualWidth(anchors, tMin, tMax) {
  const unique = [...new Set(anchors.filter((t) => t >= tMin && t <= tMax))].sort((a, b) => a - b);
  if (unique[0] !== tMin) unique.unshift(tMin);
  if (unique[unique.length - 1] !== tMax) unique.push(tMax);
  if (unique.length < 2) return tracePiecewise([tMin, tMax], [tMin, tMax]);
  const step = (tMax - tMin) / (unique.length - 1);
  return tracePiecewise(
    unique,
    unique.map((_, index) => tMin + index * step),
  );
}

/**
 * 折叠超过 30 秒的空白间隙并保留间隙摘要标记。
 * @param {TraceTrack[]} tracks
 * @param {number} tMin
 * @param {number} tMax
 * @returns {TraceScale}
 */
export function traceCompressed(tracks, tMin, tMax) {
  /** @type {Array<[number, number]>} */
  const intervals = [];
  for (const track of tracks) {
    for (const span of track.spans) intervals.push([span.start, span.end]);
  }
  intervals.sort((a, b) => a[0] - b[0]);
  /** @type {Array<[number, number]>} */
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
  /** @type {number[]} */
  const tPoints = [];
  /** @type {number[]} */
  const uPoints = [];
  /** @type {Array<{ t0: number, t1: number, uMid: number }>} */
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

/**
 * 按当前轴模式构建时间轴比例尺。
 * @param {TraceTrack[]} tracks
 * @param {"duration" | "requests" | "calls"} mode
 * @param {boolean} compress
 * @returns {TraceScale}
 */
export function traceScale(tracks, mode, compress) {
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
    /** @type {number[]} */
    const anchors = [];
    for (const track of tracks) {
      for (const span of track.spans) {
        if (span.kind === "model" || span.kind === "tool" || span.kind === "subagent") {
          anchors.push(span.start, span.end);
        }
      }
    }
    return traceEqualWidth(anchors, tMin, tMax);
  }
  if (compress) return traceCompressed(tracks, tMin, tMax);
  return tracePiecewise([tMin, tMax], [tMin, tMax]);
}

/** @param {number} raw */
function traceNiceStep(raw) {
  const pow = 10 ** Math.floor(Math.log10(Math.max(raw, 1)));
  for (const multiple of [1, 2, 5, 10]) if (pow * multiple >= raw) return pow * multiple;
  return pow * 10;
}

/**
 * 生成时间轴标尺刻度列表。
 * @param {TraceScale} scale
 * @param {number} u0
 * @param {number} u1
 * @param {number} width
 */
export function traceTicks(scale, u0, u1, width) {
  const span = u1 - u0;
  if (span <= 0 || width <= 0) return [];
  /** @type {Array<{ u: number, major: boolean, label: string }>} */
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
