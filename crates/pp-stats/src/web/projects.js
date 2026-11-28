/**
 * Projects page: cost, request count, model breakdown and recent request list per working directory.
 */

import {
  analyticsTable,
  cacheRate,
  chartPanel,
  costNote,
  detailsGrid,
  groupRow,
  metric,
  numericCells,
  overviewTable,
  pricingNotice,
  tokenMix,
} from "./analytics-shared.js";
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
  relative,
  short,
} from "./format.js";
import { app } from "./state.js";

/** @typedef {import("../../web-types/api.d.ts").ProjectRow} ProjectRow */
/** @typedef {import("../../web-types/api.d.ts").ProjectsResponse} ProjectsResponse */
/** @typedef {import("../../web-types/api.d.ts").RequestRow} RequestRow */

/** @type {"cost" | "requests" | "tokens" | "last_seen"} */
let insightSort = "cost";

export function resetProjects() {
  insightSort = "cost";
}

/** @param {"cost" | "requests" | "tokens" | "last_seen"} next */
export function setInsightSort(next) {
  insightSort = next;
}

/** @param {RequestRow[]} recent */
function insightRecent(recent) {
  return overviewTable(recent || []);
}

/**
 * @param {ProjectRow[]} projects
 * @param {"cost" | "requests"} field
 * @param {string} title
 * @param {string} color
 */
function projectRank(projects, field, title, color) {
  const payload = /** @type {ProjectsResponse} */ (app.data);
  const rows = [...projects]
    .filter((g) => g[field] != null)
    .sort((a, b) => Number(b[field] ?? 0) - Number(a[field] ?? 0))
    .slice(0, 6);
  const total = payload.summary[field];
  const max = Math.max(...rows.map((g) => Number(g[field] ?? 0)), 1e-12);
  return chartPanel(
    title,
    field === "cost" ? "Share of priced API-equivalent estimate" : "Share of all requests",
    `<div class="insight-ranking">${
      rows
        .map((g) => {
          const val = Number(g[field] ?? 0);
          const key = `project:${g.key}`;
          return `<button data-insight-open="${h(encodeURIComponent(key))}" title="${h(g.key)}" style="--rank-width:${(100 * val) / max}%;--rank-color:${color}"><span>${h(g.project || g.key || "Unknown directory")}</span><span class="num">${field === "cost" ? money(val) : number(val)} · ${percent(val, total)}</span></button>`;
        })
        .join("") || '<div class="empty">No data in this range</div>'
    }</div>`,
  );
}

/** @param {ProjectRow} g */
function projectDetails(g) {
  return `<div class="group-details"><div class="session-path">${h(g.key || "Working directory not recorded")}</div>${detailsGrid(
    [
      ["Sessions", number(g.sessions)],
      ["Requests without session", number(g.requests_without_session)],
      ["Avg TTFT", duration(g.avg_ttft_ms)],
      ["Pricing coverage", costNote(g)],
      ["First active", g.first_seen == null ? "—" : localTime(g.first_seen)],
      ["Last active", g.last_seen == null ? "—" : localTime(g.last_seen)],
    ],
  )}${tokenMix(g)}<h2>Models · select a row for usage and latency</h2>${analyticsTable(
    ["Model", "Requests", "Tokens", "Cost", "Cache rate", "Errors"],
    [...g.models]
      .sort((a, b) => b.requests - a.requests)
      .map((m) =>
        groupRow(
          { ...m, key: JSON.stringify([g.key, m.key]), description: `${m.provider} · ${m.key}` },
          "project-model",
          numericCells([
            number(m.requests),
            detailed(m.tokens),
            detailed(m.cost, money(m.cost), " USD"),
            cacheRate(m),
            percent(m.failed, m.requests),
          ]),
          () =>
            `<div class="group-details">${detailsGrid([
              ["Avg duration", duration(m.avg_duration_ms)],
              ["Avg TTFT", duration(m.avg_ttft_ms)],
              ["Pricing coverage", costNote(m)],
              ["Aborted", number(m.aborted)],
            ])}${tokenMix(m)}</div>`,
          6,
        ),
      )
      .join(""),
  )}<h2>Latest requests · up to 10 in this range</h2>${insightRecent(g.recent)}</div>`;
}

/**
 * @param {string} label
 * @param {string} placeholder
 */
function projectSearch(label, placeholder) {
  return `<div class="search">${icon("search")}<input id="search" type="search" aria-label="${label}" placeholder="${placeholder}" value="${h(app.query)}"></div>`;
}

/** @param {ProjectRow[]} rows */
function projectFooter(rows) {
  return `<div class="table-footer">${number(Math.min(app.visible, rows.length))} of ${number(rows.length)}${app.visible < rows.length ? '<button class="outline-button" data-more>Show more</button>' : ""}</div>`;
}

/**
 * Renders the Projects page.
 * @returns {string}
 */
export function projectsPage() {
  const payload = /** @type {ProjectsResponse | null} */ (app.data);
  if (!payload) return '<div class="loading">Loading projects…</div>';
  const s = payload.summary;
  const q = app.query.trim().toLowerCase();
  const rows = [...payload.projects]
    .filter(
      (g) =>
        !q ||
        [g.project, g.key, ...g.models.map((m) => m.key)].some((v) => v.toLowerCase().includes(q)),
    )
    .sort((a, b) => (b[insightSort] ?? -1) - (a[insightSort] ?? -1) || a.key.localeCompare(b.key));
  return `<h1>Projects</h1><p class="subtitle">Usage by working directory in the ${descriptions[app.range]}.</p><div class="panel metrics">${metric("Folders", number(payload.projects.length))}${metric("Requests", number(s.requests), `${number(s.failed)} failed`)}${metric("API-equivalent cost", money(s.cost), costNote(s))}${metric("Conversation tokens", short(s.tokens))}${metric("Cache rate", cacheRate(s))}</div>
    <div class="graphs project-ranks">${projectRank(payload.projects, "cost", "Top by cost", "var(--pink)")}${projectRank(payload.projects, "requests", "Top by requests", "var(--accent)")}</div>
    <section class="panel analytics-panel"><div class="requests-heading panel-heading"><div><h2>Folders</h2><p class="panel-description">${number(rows.length)} folders · select a row for tokens, models and recent requests</p></div><div class="trace-controls">${projectSearch("Search folders", "Folder, path or model…")}<select class="insight-select" id="insight-sort" aria-label="Sort folders">${[
      ["cost", "Cost"],
      ["requests", "Requests"],
      ["tokens", "Tokens"],
      ["last_seen", "Last active"],
    ]
      .map(
        ([key, label]) =>
          `<option value="${key}"${key === insightSort ? " selected" : ""}>${label}</option>`,
      )
      .join("")}</select></div></div>${analyticsTable(
      [
        "Folder",
        "Requests",
        "Cost",
        "Tokens",
        "Cache rate",
        "Errors",
        "Avg duration",
        "Last active",
      ],
      rows
        .slice(0, app.visible)
        .map((g) =>
          groupRow(
            { ...g, label: g.project || g.key || "Unknown directory", description: g.key },
            "project",
            numericCells([
              number(g.requests),
              detailed(g.cost, money(g.cost), " USD"),
              detailed(g.tokens),
              cacheRate(g),
              percent(g.failed, g.requests),
              detailed(g.avg_duration_ms, duration(g.avg_duration_ms), " ms"),
              g.last_seen == null
                ? "—"
                : `<span title="${h(localTime(g.last_seen))}">${relative(g.last_seen)}</span>`,
            ]),
            () => projectDetails(g),
            8,
          ),
        )
        .join(""),
    )}${projectFooter(rows)}</section><p class="load-note">Folders are grouped by the full recorded working directory. Different paths with the same folder name stay separate. All summaries cover every request in the selected range.</p>${pricingNotice()}`;
}
