/**
 * Traces 时间线：Canvas / Minimap / Gutter 渲染、悬停提示与指针/键盘交互。
 */

import { fixedCost, h, icon, number } from "./format.js";
import { app } from "./state.js";
import {
  HEADER_H,
  LANE_H,
  RULER_H,
  traceBounds,
  traceFirstVisible,
  traceLaneAxes,
  traceLayout,
  traceScale,
  traceTicks,
} from "./traces-scale.js";
import {
  traceBridge,
  traceClock,
  traceDurationMs,
  traceElapsed,
  traceOffset,
  traceSelected,
  traceStore,
} from "./traces-state.js";

/** @typedef {import("./traces-state.js").TraceHit} TraceHit */
/** @typedef {import("./traces-state.js").TraceLayout} TraceLayout */
/** @typedef {import("./traces-state.js").TraceSpan} TraceSpan */
/** @typedef {import("./traces-state.js").TraceState} TraceState */
/** @typedef {import("./traces-state.js").TraceTrack} TraceTrack */
/** @typedef {import("./traces-state.js").TraceViewModel} TraceViewModel */

const MIN_WINDOW_U = 10;
const MIN_SPAN_PX = 2;
const HIT_SLOP_PX = 4;
const LABEL_MIN_PX = 56;
const SPAN_RADIUS = 3;
const DIM_ALPHA = 0.3;

export { traceBounds, traceLayout, traceScale };

/**
 * 读取当前 CSS 变量计算画布调色板。
 */
export function traceColors() {
  const style = getComputedStyle(document.documentElement);
  /**
   * @param {string} name
   * @param {string} fallback
   */
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

/**
 * @param {CanvasRenderingContext2D} ctx
 * @param {number} x
 * @param {number} y
 * @param {number} w
 * @param {number} height
 */
function traceSpanPath(ctx, x, y, w, height) {
  ctx.beginPath();
  if (w >= SPAN_RADIUS * 2 + 1) ctx.roundRect(x, y, w, height, SPAN_RADIUS);
  else ctx.rect(x, y, w, height);
}

/** @param {TraceViewModel} view */
function traceMatchIds(view) {
  if (!app.query.trim()) return null;
  return new Set(view.matches.map(({ span }) => span.id));
}

/**
 * 返回当前有效视口区间。
 * @param {TraceState} state
 * @returns {{ u0: number, u1: number }}
 */
export function traceViewport(state) {
  const domain = state.scale?.domain ?? [0, 1];
  if (!state.viewport) return { u0: domain[0], u1: domain[1] };
  return state.viewport;
}

/**
 * 将视口限制在有效时间域和最小窗口宽度内。
 * @param {TraceState} state
 * @param {number} u0
 * @param {number} u1
 */
export function traceClampViewport(state, u0, u1) {
  const [d0, d1] = state.scale?.domain ?? [0, 1];
  let span = Math.max(MIN_WINDOW_U, u1 - u0);
  span = Math.min(span, d1 - d0 || MIN_WINDOW_U);
  let start = u0;
  if (start < d0) start = d0;
  if (start + span > d1) start = d1 - span;
  return { u0: start, u1: start + span };
}

/**
 * 计算指定 span 在当前视口内的像素矩形。
 * @param {TraceState} state
 * @param {TraceLayout} layout
 * @param {TraceSpan} span
 * @param {TraceTrack} track
 */
function traceSpanBox(state, layout, span, track) {
  if (!state.scale) return null;
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

/**
 * @param {TraceState} state
 * @param {TraceViewModel} view
 * @param {TraceLayout} layout
 * @param {ReturnType<typeof traceLaneAxes>} laneAxes
 * @param {ReturnType<typeof traceColors>} colors
 * @returns {TraceHit[]}
 */
export function traceDrawScene(state, view, layout, laneAxes, colors) {
  const canvas = /** @type {HTMLCanvasElement | null} */ (
    document.querySelector("[data-trace-canvas]")
  );
  const scale = state.scale;
  if (!canvas || !scale) return [];
  const width = state.width;
  const ctx = canvas.getContext("2d");
  if (!ctx) return [];
  /** @type {TraceHit[]} */
  const hits = [];
  const height = layout.totalHeight;
  const dpr = window.devicePixelRatio || 1;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, width, height);
  const viewport = traceViewport(state);
  const { u0, u1 } = viewport;
  const spanU = Math.max(u1 - u0, 1e-9);
  /** @param {number} u */
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
  /** @type {number[]} */
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
      if (!gapXs.some((gapX) => Math.abs(gapX - x) < 44)) ctx.fillText(tick.label, x + 4, 11);
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

/**
 * 绘制顶部概览 Minimap 与当前视口选框。
 * @param {TraceState} state
 * @param {TraceViewModel} view
 * @param {ReturnType<typeof traceColors>} colors
 */
export function traceDrawMinimap(state, view, colors) {
  const canvas = /** @type {HTMLCanvasElement | null} */ (
    document.querySelector("[data-trace-minimap]")
  );
  const scale = state.scale;
  if (!canvas || !scale) return;
  const width = Math.max(100, Math.floor(canvas.parentElement?.clientWidth || 800));
  const height = 44;
  const dpr = window.devicePixelRatio || 1;
  canvas.width = Math.floor(width * dpr);
  canvas.height = Math.floor(height * dpr);
  canvas.style.height = `${height}px`;
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, width, height);
  /** @type {Array<"model" | "tool" | "subagent">} */
  const strips = ["model", "tool", "subagent"];
  const [d0, d1] = scale.domain;
  const domain = Math.max(d1 - d0, 1e-9);
  const buckets = strips.map(() => new Float32Array(width));
  for (const track of view.tracks) {
    for (const span of track.spans) {
      const index = strips.indexOf(span.kind);
      if (index === -1) continue;
      const strip = buckets[index];
      const from = Math.max(0, Math.floor(((scale.toU(span.start) - d0) / domain) * width));
      const to = Math.min(width - 1, Math.ceil(((scale.toU(span.end) - d0) / domain) * width));
      for (let x = from; x <= to; x++) strip[x] = Math.min(1, strip[x] + 0.34);
    }
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

/**
 * 更新时间线上的悬停高亮框与选中高亮框叠加层。
 * @param {TraceState} state
 * @param {TraceViewModel} view
 * @param {TraceLayout} layout
 */
export function traceDrawOverlays(state, view, layout) {
  const hover = /** @type {HTMLElement | null} */ (document.querySelector("[data-trace-hover]"));
  const selection = /** @type {HTMLElement | null} */ (
    document.querySelector("[data-trace-selection]")
  );
  const selected = traceSelected(view, state);
  const matchIds = traceMatchIds(view);
  if (hover) {
    const hit = state.hover;
    const spanHit = hit?.kind === "span" ? hit : null;
    const box = spanHit ? traceSpanBox(state, layout, spanHit.span, spanHit.track) : null;
    if (!box || !spanHit) {
      hover.hidden = true;
    } else {
      hover.hidden = false;
      hover.style.left = `${box.x}px`;
      hover.style.top = `${box.y}px`;
      hover.style.width = `${box.w}px`;
      hover.style.height = `${LANE_H}px`;
      hover.style.opacity =
        matchIds === null || matchIds.has(spanHit.span.id) ? "1" : String(DIM_ALPHA);
    }
  }
  if (selection) {
    const box = selected ? traceSpanBox(state, layout, selected.span, selected.track) : null;
    if (!box) {
      selection.hidden = true;
    } else {
      selection.hidden = false;
      selection.style.left = `${box.x - 1}px`;
      selection.style.top = `${box.y - 1}px`;
      selection.style.width = `${box.w + 2}px`;
      selection.style.height = `${LANE_H + 2}px`;
      selection.style.borderColor = traceColors().selection;
    }
  }
}

/**
 * 重绘主画布、Minimap、叠加框与搜索匹配计数。
 */
export function traceRedraw() {
  if (!traceStore.state || !traceStore.view || app.page !== "traces" || !traceStore.request) return;
  const state = traceStore.state;
  const layout = state.layout;
  if (!layout || !state.laneAxes) return;
  state.hits = traceDrawScene(state, traceStore.view, layout, state.laneAxes, traceColors());
  traceDrawMinimap(state, traceStore.view, traceColors());
  traceDrawOverlays(state, traceStore.view, layout);
  const count = document.querySelector("[data-trace-match-count]");
  if (count) {
    const matches = traceStore.view.matches.length;
    const index = state.matchIndex % Math.max(matches, 1);
    count.textContent = matches === 0 ? "0" : `${index + 1}/${matches}`;
  }
}

/**
 * 渲染左侧轨道标签与折叠按钮。
 * @param {TraceState} state
 * @param {TraceLayout} layout
 */
export function traceRenderGutter(state, layout) {
  const gutter = /** @type {HTMLElement | null} */ (document.querySelector("[data-trace-gutter]"));
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

export function traceTooltipHide() {
  const tooltip = document.getElementById("trace-tooltip");
  if (tooltip) tooltip.hidden = true;
}

/**
 * 展示时间线 span 或 marker 的悬停气泡。
 * @param {TraceHit} hit
 * @param {TraceViewModel} view
 * @param {number} clientX
 * @param {number} clientY
 */
function traceShowTooltip(hit, view, clientX, clientY) {
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
        ? `<div class="x-muted x-num">${number(span.tokens ?? 0)} tok · ${fixedCost(
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
  tooltip.style.left = `${Math.max(8, Math.min(clientX + 14, window.innerWidth - box.width - 8))}px`;
  tooltip.style.top = `${
    clientY + 16 + box.height > window.innerHeight - 8 ? clientY - box.height - 12 : clientY + 16
  }px`;
}

/**
 * 将视口居中对齐到超出视野的选中 span。
 * @param {TraceState} state
 * @param {TraceSpan} span
 */
export function traceCenterOnSpan(state, span) {
  if (!state.scale) return;
  const u = state.scale.toU(span.start);
  const end = state.scale.toU(span.end);
  const viewport = traceViewport(state);
  if (u >= viewport.u0 && end <= viewport.u1) return;
  const win = viewport.u1 - viewport.u0;
  const mid = (u + end) / 2;
  const [d0, d1] = state.scale.domain;
  let start = mid - win / 2;
  if (start < d0) start = d0;
  if (start + win > d1) start = Math.max(d0, d1 - win);
  state.viewport = { u0: start, u1: start + win };
  traceRedraw();
}

/**
 * @param {TraceState} state
 * @param {number} factor
 * @param {number | null} anchorPx
 * @param {number} width
 */
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

/**
 * @param {TraceState} state
 * @param {number} delta
 */
function tracePanBy(state, delta) {
  const { u0, u1 } = traceViewport(state);
  state.viewport = traceClampViewport(state, u0 + delta, u1 + delta);
  traceRedraw();
}

/**
 * @param {TraceState} state
 * @param {TraceSpan} span
 */
function traceZoomToSpan(state, span) {
  if (!state.scale) return;
  const from = state.scale.toU(span.start);
  const to = state.scale.toU(span.end);
  const pad = Math.max((to - from) * 0.1, MIN_WINDOW_U / 2);
  state.viewport = traceClampViewport(state, from - pad, to + pad);
  traceRedraw();
}

/**
 * 绑定主画布与 Minimap 的指针、滚轮、键盘与自适应尺寸事件。
 * @param {TraceViewModel} view
 * @param {TraceState} state
 * @param {TraceLayout} layout
 */
export function traceWireCanvas(view, state, layout) {
  const canvas = /** @type {HTMLCanvasElement | null} */ (
    document.querySelector("[data-trace-canvas]")
  );
  const stack = /** @type {HTMLElement | null} */ (document.querySelector("[data-trace-stack]"));
  if (!canvas || !stack || !state.scale) return;
  const colors = traceColors();
  const width = Math.max(100, Math.floor(stack.clientWidth));
  state.width = width;
  state.layout = layout;
  canvas.style.width = `${width}px`;
  canvas.style.height = `${layout.totalHeight}px`;
  canvas.width = Math.floor(width * (window.devicePixelRatio || 1));
  canvas.height = Math.floor(layout.totalHeight * (window.devicePixelRatio || 1));
  state.laneAxes = traceLaneAxes(layout, state.scale);
  state.hits = traceDrawScene(state, view, layout, state.laneAxes, colors);
  /** @type {{ pointerId: number, lastX: number, moved: boolean } | null} */
  let drag = null;
  /** @param {MouseEvent} event */
  const localX = (event) => event.clientX - canvas.getBoundingClientRect().left;
  /** @param {MouseEvent} event */
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
        tracePanBy(state, (-delta / (state.width || 1)) * span);
      }
      state.hover = null;
      traceTooltipHide();
      return;
    }
    const hit = hitAt(event);
    state.hover = hit;
    traceDrawOverlays(state, view, layout);
    if (hit) traceShowTooltip(hit, view, event.clientX, event.clientY);
    else traceTooltipHide();
  };
  canvas.onpointerup = (event) => {
    const current = drag;
    drag = null;
    if (current?.moved) return;
    const hit = hitAt(event);
    if (hit?.kind === "span") traceBridge.select(view, state, hit.span.id, true);
    else traceBridge.select(view, state, null, false);
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
        traceBridge.select(view, state, null, false);
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
    if (next === state.width || !state.laneAxes) return;
    state.width = next;
    canvas.style.width = `${next}px`;
    canvas.width = Math.floor(next * (window.devicePixelRatio || 1));
    state.hits = traceDrawScene(state, view, layout, state.laneAxes, traceColors());
    traceDrawMinimap(state, view, traceColors());
    traceDrawOverlays(state, view, layout);
  });
  state.observer.observe(stack);
  const minimap = /** @type {HTMLCanvasElement | null} */ (
    document.querySelector("[data-trace-minimap]")
  );
  if (minimap) {
    /** @type {{
     *   mode: "create" | "left" | "right" | "move",
     *   startX: number,
     *   startU: number,
     *   start: { u0: number, u1: number },
     *   width: number,
     * } | null} */
    let minimapDrag = null;
    /** @param {PointerEvent} event */
    const point = (event) => {
      const box = minimap.getBoundingClientRect();
      return { x: event.clientX - box.left, width: box.width };
    };
    /**
     * @param {number} x
     * @param {number} mw
     */
    const toU = (x, mw) => {
      const [d0, d1] = state.scale?.domain ?? [0, 1];
      return d0 + (x / Math.max(mw, 1)) * (d1 - d0);
    };
    minimap.onpointerdown = (event) => {
      const { x, width: mw } = point(event);
      const [d0, d1] = state.scale?.domain ?? [0, 1];
      const viewport = traceViewport(state);
      const x0 = ((viewport.u0 - d0) / Math.max(d1 - d0, 1e-9)) * mw;
      const x1 = ((viewport.u1 - d0) / Math.max(d1 - d0, 1e-9)) * mw;
      /** @type {"create" | "left" | "right" | "move"} */
      let mode = "create";
      if (Math.abs(x - x0) <= 6) mode = "left";
      else if (Math.abs(x - x1) <= 6) mode = "right";
      else if (x > x0 && x < x1) mode = "move";
      else {
        const u = d0 + (x / Math.max(mw, 1)) * (d1 - d0);
        state.viewport = traceClampViewport(state, u, u + MIN_WINDOW_U);
        traceRedraw();
      }
      minimapDrag = {
        mode,
        startX: x,
        startU: d0 + (x / Math.max(mw, 1)) * (d1 - d0),
        start: viewport,
        width: mw,
      };
      minimap.setPointerCapture(event.pointerId);
    };
    minimap.onpointermove = (event) => {
      if (!minimapDrag) return;
      const { x, width: mw } = point(event);
      const [d0, d1] = state.scale?.domain ?? [0, 1];
      const currentU = d0 + (x / Math.max(mw, 1)) * (d1 - d0);
      const delta = currentU - minimapDrag.startU;
      const { u0, u1 } = minimapDrag.start;
      let next;
      if (minimapDrag.mode === "move") next = { u0: u0 + delta, u1: u1 + delta };
      else if (minimapDrag.mode === "left") {
        next = { u0: Math.min(u0 + delta, u1 - MIN_WINDOW_U), u1 };
      } else if (minimapDrag.mode === "right") {
        next = { u0, u1: Math.max(u1 + delta, u0 + MIN_WINDOW_U) };
      } else {
        const anchor = d0 + (minimapDrag.startX / Math.max(mw, 1)) * (d1 - d0);
        const current = toU(x, mw);
        next = {
          u0: Math.min(anchor, current),
          u1: Math.max(anchor, current, Math.min(anchor, current) + MIN_WINDOW_U),
        };
      }
      state.viewport = traceClampViewport(state, next.u0, Math.min(next.u1, d1));
      state.viewport = traceClampViewport(
        state,
        state.viewport.u0,
        Math.max(state.viewport.u1, state.viewport.u0 + MIN_WINDOW_U),
      );
      traceRedraw();
    };
    minimap.onpointerup = () => {
      minimapDrag = null;
    };
  }
}
