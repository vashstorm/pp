/**
 * Requests page: full request search, status filtering, column sorting and paged loading.
 */

import { metric, pricingNotice, statusBadge } from "./analytics-shared.js";
import {
  descriptions,
  detailed,
  duration,
  h,
  icon,
  localTime,
  money,
  number,
  percent,
  quantile,
  relative,
  short,
  sum,
  total,
  uncached,
} from "./format.js";
import { app } from "./state.js";

/** @typedef {import("../../web-types/api.d.ts").RequestRow} RequestRow */

const columns = [
  ["model", "Model"],
  ["timestamp", "When"],
  ["project", "Project"],
  ["input", "Input"],
  ["cached_tokens", "Cache read"],
  ["cache_write_tokens", "Cache write"],
  ["output_tokens", "Output"],
  ["cost", "Cost"],
  ["duration_ms", "Duration"],
  ["ttft_ms", "TTFT"],
  ["status", "Status"],
];

/**
 * Extracts the sort key of a request row.
 * @param {RequestRow} r
 * @param {string} key
 */
function sortValue(r, key) {
  if (key === "input") return uncached(r);
  return /** @type {Record<string, unknown>} */ (/** @type {unknown} */ (r))[key];
}

/**
 * Filters the Requests list by search term, status and sort field.
 * @returns {RequestRow[]}
 */
export function filteredRows() {
  if (!app.requestData) return [];
  const q = app.query.trim().toLowerCase();
  return app.requestData.requests
    .filter(
      (r) =>
        (app.status === "all" || r.status === app.status) &&
        (!q ||
          [r.model, r.provider, r.project, r.cwd, r.model_reference].some((v) =>
            v.toLowerCase().includes(q),
          )),
    )
    .sort((a, b) => {
      const x = sortValue(a, app.sortKey);
      const y = sortValue(b, app.sortKey);
      if (x == null) return y == null ? b.id - a.id : 1;
      if (y == null) return -1;
      const cmp =
        typeof x === "string" && typeof y === "string" ? x.localeCompare(y) : Number(x) - Number(y);
      return cmp * app.sortDirection || b.id - a.id;
    });
}

/**
 * Aggregates count, total tokens and latency quantiles of the loaded requests.
 * @param {RequestRow[]} rows
 */
export function requestSummary(rows) {
  return {
    count: rows.length,
    ok: rows.filter((r) => r.status === "ok").length,
    failed: rows.filter((r) => r.status === "failed").length,
    aborted: rows.filter((r) => r.status === "aborted").length,
    tokens: sum(rows.map(total)),
    median: quantile(
      rows.map((r) => r.duration_ms),
      0.5,
    ),
    p95: quantile(
      rows.map((r) => r.duration_ms),
      0.95,
    ),
    ttft: quantile(
      rows.map((r) => r.ttft_ms),
      0.5,
    ),
  };
}

/** @param {RequestRow[]} rows */
function requestTable(rows) {
  return `<div class="table-wrap"><table><thead><tr>${columns
    .map(
      ([key, label], i) =>
        `<th tabindex="0" class="sortable ${i > 2 && i < 10 ? "right" : ""}" data-sort="${key}" ${
          app.sortKey === key
            ? `aria-sort="${app.sortDirection < 0 ? "descending" : "ascending"}"`
            : ""
        }>${label}${app.sortKey === key ? `<span class="sort-arrow">${app.sortDirection < 0 ? "↓" : "↑"}</span>` : ""}</th>`,
    )
    .join("")}</tr></thead><tbody>${
    rows.length
      ? rows
          .slice(0, app.visible)
          .map(
            (r) =>
              `<tr tabindex="0" data-id="${r.id}"${r.has_context ? ' class="first-request-row"' : ""} aria-label="View ${h(r.model)} request ${r.id}${r.has_context ? ", session first request with saved context" : ""}"><td><div class="model-name">${h(r.model)}</div><div class="request-model-meta"><span class="provider">${h(r.provider)}</span>${r.has_context ? `<span class="first-request-mark" role="img" aria-label="Session first request" title="Session first request · full context saved">${icon("flag", 11)}</span>` : ""}</div></td><td title="${h(localTime(r.timestamp))}">${relative(r.timestamp)}</td><td class="project" title="${h(r.cwd)}">${h(r.project)}</td><td class="right num" title="${h(`Total input: ${number(r.input_tokens)}; uncached: ${number(uncached(r))}`)}">${short(uncached(r))}</td><td class="right num" title="Cache read: ${number(r.cached_tokens)}">${detailed(r.cached_tokens)}</td><td class="right num">${detailed(r.cache_write_tokens)}</td><td class="right num">${detailed(r.output_tokens)}</td><td class="right num">${detailed(r.cost, money(r.cost), " USD")}</td><td class="right num">${detailed(r.duration_ms, duration(r.duration_ms), " ms")}</td><td class="right num">${detailed(r.ttft_ms, duration(r.ttft_ms), " ms")}</td><td>${statusBadge(r)}</td></tr>`,
          )
          .join("")
      : '<tr><td colspan="11"><div class="empty">No matching requests</div></td></tr>'
  }</tbody></table></div>`;
}

/**
 * Renders the Requests page.
 * @returns {string}
 */
export function requests() {
  if (!app.requestData) return '<div class="loading">Loading statistics…</div>';
  const all = app.requestData.requests;
  const s = requestSummary(all);
  const rows = filteredRows();
  const oldest = all.at(-1)?.timestamp;
  return `<h1>Requests</h1><p class="subtitle">Every model call pp made in the ${descriptions[app.range]}, newest first. Open a row for its full payload.</p><div class="panel metrics request-metrics">${metric("Requests", number(s.count), oldest ? `since ${relative(oldest)}` : "No requests")}${metric("Failed", number(s.failed), `${s.count ? percent(s.failed, s.count) : "0.0%"} · ${s.aborted} aborted`)}${metric("Tokens", short(s.tokens))}${metric("API-equivalent cost", money(all.some((r) => r.cost != null) ? sum(all.map((r) => r.cost)) : all.length ? null : 0), `${all.filter((r) => r.cost == null).length} unpriced`)}${metric("Median duration", duration(s.median), `p95 ${duration(s.p95)}`)}${metric("Median TTFT", duration(s.ttft))}</div><section class="panel"><div class="requests-heading panel-heading"><div><h2>Request log</h2><p class="panel-description">${number(rows.length)} of the latest ${number(all.length)} requests</p></div><div class="filter-bar"><div class="search">${icon("search")}<input id="search" type="search" aria-label="Search requests" placeholder="Model, provider or project…" value="${h(app.query)}"></div><div class="segmented" role="radiogroup" aria-label="Status">${[
    ["all", "All", s.count],
    ["ok", "OK", s.ok],
    ["aborted", "Aborted", s.aborted],
    ["failed", "Failed", s.failed],
  ]
    .map(
      ([id, label, count]) =>
        `<button role="radio" aria-checked="${id === app.status}" data-status="${id}" aria-label="${label} ${count}">${label} <span class="count">${count}</span></button>`,
    )
    .join(
      "",
    )}</div></div></div>${requestTable(rows)}<div class="table-footer">${Math.min(app.visible, rows.length)} of ${number(rows.length)}${app.visible < rows.length ? '<button class="outline-button" data-more>Show more</button>' : ""}</div></section>${app.requestData.has_more ? `<div class="load-note"><span>Showing the latest ${number(all.length)} requests, back to ${oldest != null ? h(localTime(oldest)) : "—"}. Older requests in the ${descriptions[app.range]} are not loaded.</span>${app.limit === 500 ? '<button class="outline-button" data-load>Load latest 2,000</button>' : ""}</div>` : ""}${pricingNotice()}`;
}
