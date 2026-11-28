/**
 * Cross-page primitives for `x-table` header sorting, list sorting, paging footer, empty state and
 * progress bars.
 */

import { h, number, TOOL_ARROW_DOWN, TOOL_ARROW_UP, TOOL_INBOX } from "./format.js";

/** @typedef {import("../../web-types/api.d.ts").SortState} SortState */

/**
 * @template T
 * @typedef {{
 *   key: string,
 *   header: string,
 *   align?: "left" | "right",
 *   width?: number,
 *   wrap?: boolean,
 *   title?: string,
 *   sort?: (row: T) => number | string,
 *   render: (row: T) => string,
 * }} TableColumn
 */

/**
 * Renders a sortable `<th>` header cell, emitting markup with the data-attribute convention chosen
 * by the caller.
 * @template T
 * @param {TableColumn<T>} column
 * @param {SortState} sort
 * @param {{
 *   sortAttr: string,
 *   tableAttr?: string,
 *   table?: string,
 *   alignAttr?: string,
 *   focusAttr?: string,
 *   focusPrefix?: string,
 * }} attrs
 * @returns {string}
 */
export function renderSortHeader(column, sort, attrs) {
  const sorted = sort.key === column.key ? sort.dir : "";
  const arrow = sorted === "asc" ? TOOL_ARROW_UP : sorted === "desc" ? TOOL_ARROW_DOWN : "";
  const align = column.align || "left";
  const sortPart = column.sort
    ? ` ${attrs.sortAttr}="${column.key}"${attrs.tableAttr && attrs.table ? ` ${attrs.tableAttr}="${attrs.table}"` : ""}${attrs.alignAttr ? ` ${attrs.alignAttr}="${align}"` : ""}${attrs.focusAttr ? ` ${attrs.focusAttr}="${attrs.focusPrefix || "sort-"}${column.key}"` : ""}`
    : "";
  return `<th data-align="${align}" data-sortable="${Boolean(column.sort)}"${sorted ? ` data-sorted="${sorted}" aria-sort="${sorted === "asc" ? "ascending" : "descending"}"` : ""}${column.width ? ` style="width:${column.width}px"` : ""}${column.title ? ` title="${h(column.title)}"` : ""}${sortPart}>${h(column.header)}${arrow ? `<span class="x-sort-arrow">${arrow}</span>` : ""}</th>`;
}

/**
 * Returns a new sorted array based on the column definitions and the current sort state.
 * @template T
 * @param {T[]} rows
 * @param {TableColumn<T>[]} columns
 * @param {SortState} sort
 * @returns {T[]}
 */
export function sortRows(rows, columns, sort) {
  const column = columns.find((item) => item.key === sort.key && item.sort);
  if (!column?.sort) return rows;
  const getKey = column.sort;
  const direction = sort.dir === "asc" ? 1 : -1;
  return [...rows].sort((a, b) => {
    const left = getKey(a);
    const right = getKey(b);
    const delta =
      typeof left === "number" && typeof right === "number"
        ? left - right
        : String(left).localeCompare(String(right));
    return delta * direction;
  });
}

/**
 * Renders the paging expand/collapse control.
 * @param {{
 *   total: number,
 *   shown: number,
 *   step: number,
 *   moreAttr: string,
 *   lessAttr: string,
 *   focusAttr?: string,
 *   focusMore?: string,
 *   focusLess?: string,
 * }} options
 * @returns {string}
 */
export function renderTableMore(options) {
  const { total, shown, step, moreAttr, lessAttr, focusAttr, focusMore, focusLess } = options;
  if (total <= step) return "";
  return `<div class="x-table-more"><span class="x-micro">${number(shown)} of ${number(total)}</span>${
    total > shown
      ? `<button type="button" class="x-btn" data-size="sm" data-variant="ghost" ${moreAttr}${focusAttr && focusMore ? ` ${focusAttr}="${focusMore}"` : ""}>Show more</button>`
      : ""
  }${
    shown > step
      ? `<button type="button" class="x-btn" data-size="sm" data-variant="ghost" ${lessAttr}${focusAttr && focusLess ? ` ${focusAttr}="${focusLess}"` : ""}>Show fewer</button>`
      : ""
  }</div>`;
}

/**
 * Renders an `x-empty` empty-state block; note that `hint` is plain text and is escaped centrally
 * here.
 * @param {string} title
 * @param {string} [hint]
 * @returns {string}
 */
export function xEmpty(title, hint = "") {
  return `<div class="x-empty">${TOOL_INBOX}<div class="x-empty-title">${h(title)}</div>${hint ? `<div class="x-empty-hint">${h(hint)}</div>` : ""}</div>`;
}

/**
 * Renders an `x-meter-cell` with a right-aligned value and a background progress bar.
 * @param {number} value
 * @param {number} max
 * @param {string} display
 * @param {string} [color]
 * @returns {string}
 */
export function xMeter(value, max, display, color = "") {
  const width = max > 0 ? Math.min(100, (value / max) * 100) : 0;
  return `<div class="x-meter-cell"><span class="x-num">${display}</span><div class="x-meter"><div class="x-meter-fill" style="width:${width}%${color ? `;background:${color}` : ""}"></div></div></div>`;
}
