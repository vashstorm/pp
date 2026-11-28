/**
 * Global hover/focus tooltip and table cell detail enhancement.
 */

import { $ } from "./format.js";

/** @type {HTMLDivElement | null} */
let tooltip = null;
/** @type {HTMLElement | SVGElement | null} */
let tooltipTarget = null;

/**
 * Ensures the tooltip node is mounted and returns it.
 * @returns {HTMLDivElement}
 */
function ensureTooltip() {
  if (!tooltip) {
    tooltip = document.createElement("div");
    tooltip.id = "stats-tooltip";
    tooltip.role = "tooltip";
    tooltip.hidden = true;
    document.body.append(tooltip);
  }
  return tooltip;
}

/**
 * Hides the current tooltip and clears `aria-describedby`.
 */
export function hideTooltip() {
  tooltipTarget?.removeAttribute("aria-describedby");
  tooltipTarget = null;
  if (tooltip) tooltip.hidden = true;
}

/**
 * Shows the target element's `data-tooltip` content at the given viewport coordinates.
 * @param {HTMLElement | SVGElement} target
 * @param {number} x
 * @param {number} y
 */
export function showTooltip(target, x, y) {
  const boxEl = ensureTooltip();
  if (tooltipTarget !== target) {
    hideTooltip();
    tooltipTarget = target;
    boxEl.textContent = target.dataset.tooltip ?? "";
    target.setAttribute("aria-describedby", boxEl.id);
    boxEl.hidden = false;
  }
  const box = boxEl.getBoundingClientRect();
  boxEl.style.left = `${Math.max(8, Math.min(x + 12, window.innerWidth - box.width - 8))}px`;
  boxEl.style.top = `${Math.max(8, y + 16 + box.height > window.innerHeight - 8 ? y - box.height - 12 : y + 16)}px`;
}

/**
 * Promotes every `title` in the page and every table cell to a unified `data-tooltip` bubble.
 */
export function prepareDetails() {
  hideTooltip();
  const root = $("page");
  if (!root) return;
  for (const title of root.querySelectorAll("svg title")) {
    const parent = title.parentElement;
    if (parent) parent.dataset.tooltip = title.textContent ?? "";
    title.remove();
  }
  for (const el of root.querySelectorAll("[title]")) {
    if (el instanceof HTMLElement || el instanceof SVGElement) {
      el.dataset.tooltip = el.getAttribute("title") ?? "";
      el.removeAttribute("title");
    }
  }
  /** @type {Record<string, string>} */
  const meanings = {
    Tokens: "Total input (including cache reads and writes) + output tokens",
    "Cache rate": "Cache reads / total input",
    "Tokens/s": "Output tokens / request duration in seconds",
    TTFT: "Time from request start to the first output token",
    Cost: "API-equivalent estimate in USD; excludes unpriced requests",
    "Attr. tokens": "Invoking request tokens split evenly across its tool calls",
    "Attr. cost": "Invoking request estimate split evenly across its tool calls",
  };
  for (const cell of root.querySelectorAll("th,td:not([colspan])")) {
    if (!(cell instanceof HTMLTableCellElement) || cell.querySelector("table")) continue;
    const table = cell.closest("table");
    const header = table?.tHead?.rows?.[0]?.cells?.[cell.cellIndex];
    const values = [...cell.querySelectorAll("[data-tooltip]:not(.chart-hit)")].map((el) =>
      el instanceof HTMLElement || el instanceof SVGElement ? (el.dataset.tooltip ?? "") : "",
    );
    const cellText = (cell.textContent || "").trim();
    const text =
      (cell.tagName === "TH" && meanings[(cell.textContent || "").trim()]) ||
      (cell.querySelector("svg.sparkline")
        ? "Move across the trend or focus a time bucket to inspect values."
        : [...new Set([cellText, ...values].filter(Boolean))].join("\n"));
    cell.dataset.tooltip = `${header?.textContent?.trim() || "Details"}\n${cell.dataset.tooltip || text || "Not reported"}`;
    if (!cell.hasAttribute("tabindex")) cell.tabIndex = 0;
  }
}

let tooltipBound = false;

/**
 * Binds the global tooltip pointer, focus and scroll listeners (idempotent).
 */
export function initTooltip() {
  if (tooltipBound) return;
  tooltipBound = true;
  ensureTooltip();
  const pageEl = $("page");
  pageEl?.addEventListener("pointermove", (e) => {
    if (e.pointerType === "touch") return;
    const target = e.target instanceof Element ? e.target.closest("[data-tooltip]") : null;
    if (target instanceof HTMLElement || target instanceof SVGElement) {
      showTooltip(target, e.clientX, e.clientY);
    } else {
      hideTooltip();
    }
  });
  pageEl?.addEventListener("pointerleave", hideTooltip);
  pageEl?.addEventListener("focusin", (e) => {
    const target = e.target instanceof Element ? e.target.closest("[data-tooltip]") : null;
    if (!(target instanceof HTMLElement || target instanceof SVGElement)) return;
    const box = target.getBoundingClientRect();
    showTooltip(target, box.left, box.bottom);
  });
  pageEl?.addEventListener("focusout", hideTooltip);
  pageEl?.addEventListener("pointerdown", hideTooltip);
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") hideTooltip();
  });
  document.addEventListener("scroll", hideTooltip, true);
  window.addEventListener("resize", hideTooltip);
}
