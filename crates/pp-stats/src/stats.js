/**
 * Entry point of the pp-stats Web console: shell, theme/style persistence, routing, refresh, detail
 * drawer and central event handling.
 */

import {
  activityClick,
  activityInput,
  errorsPage,
  getErrorLimit,
  resetActivity,
} from "./activity.js";
import {
  closeContextReader,
  contextReaderKeydown,
  initContextReader,
  openContextReader,
  requestContextButton,
} from "./context.js";
import {
  hooksPage,
  initInsights,
  insightsChange,
  insightsClick,
  projectsPage,
  resetInsights,
} from "./insights.js";
import {
  getTraceRenderedSignature,
  getTraceRequest,
  initTraces,
  resetTraces,
  traceRedraw,
  traceRemoveDrawer,
  traceSignature,
  tracesClick,
  tracesPage,
  wireTraces,
} from "./traces.js";
import { detailsGrid, statusBadge } from "./web/analytics-shared.js";
import { wireToolsCharts } from "./web/charts.js";
import { costsPage } from "./web/costs.js";
import {
  $,
  descriptions,
  duration,
  h,
  icon,
  liveAge,
  localTime,
  money,
  number,
  ranges,
  rangeWindows,
  relative,
  total,
  uncached,
} from "./web/format.js";
import { modelsClick, modelsPage, resetModels } from "./web/models.js";
import { overview } from "./web/overview.js";
import { providersChange, providersClick, providersPage, resetProviders } from "./web/providers.js";
import { requests } from "./web/requests.js";
import { app, getJSON, registerHooks } from "./web/state.js";
import { hideTooltip, initTooltip, prepareDetails } from "./web/tooltip.js";

/** @typedef {import("../web-types/api.d.ts").RequestDetailResponse} RequestDetailResponse */
/** @typedef {import("../web-types/api.d.ts").RequestsResponse} RequestsResponse */
/** @typedef {import("../web-types/api.d.ts").StatsPage} StatsPage */
/** @typedef {import("../web-types/api.d.ts").StatsRange} StatsRange */
/** @typedef {import("../web-types/api.d.ts").StatsStyle} StatsStyle */
/** @typedef {import("../web-types/api.d.ts").StatsTheme} StatsTheme */
/** @typedef {import("../web-types/api.d.ts").StyleSettingsResponse} StyleSettingsResponse */

/** @type {Array<[string, Array<[StatsPage, string, string]>]>} */
const groups = [
  [
    "Usage",
    [
      ["overview", "Overview", "O"],
      ["models", "Models", "M"],
      ["providers", "Providers", "P"],
      ["costs", "Costs", "C"],
    ],
  ],
  [
    "Activity",
    [
      ["requests", "Requests", "R"],
      ["errors", "Errors", "E"],
      ["traces", "Traces", "T"],
    ],
  ],
  [
    "Insights",
    [
      ["tools", "Tools", "L"],
      ["hooks", "Hooks", "H"],
      ["frustration", "Frustration", "F"],
      ["projects", "Projects", "J"],
    ],
  ],
];
const pages = groups.flatMap((g) => g[1]);

let generation = 0;
/** @type {AbortController | null} */
let controller = null;
/** @type {AbortController | null} */
let detailController = null;
let detailGeneration = 0;
/** @type {number | null} */
let selectedId = null;
/** @type {Element | null} */
let returnFocus = null;
/** @type {number | null} */
let lastUpdated = null;
let gTime = 0;
/** @type {StatsPage | null} */
let renderedPage = null;
/** @type {StatsRange | null} */
let renderedRange = null;
/** @type {ReturnType<typeof setTimeout> | undefined} */
let toastTimer;

/** @type {StatsTheme} */
let theme = "system";
let styleReady = false;

function readSavedTheme() {
  let saved = "system";
  try {
    saved = localStorage.getItem("pp-stats-theme") || "system";
  } catch {}
  return /** @type {StatsTheme} */ (["system", "light", "dark"].includes(saved) ? saved : "system");
}

function applyTheme() {
  const dark = window.matchMedia("(prefers-color-scheme: dark)").matches;
  const resolved = theme === "system" ? (dark ? "dark" : "light") : theme;
  document.documentElement.dataset.theme = resolved;
  document.documentElement.style.colorScheme = resolved;
  const themeEl = $("theme");
  if (themeEl) {
    themeEl.innerHTML = icon(theme === "light" ? "sun" : theme === "dark" ? "moon" : "system", 15);
    themeEl.setAttribute(
      "aria-label",
      `${theme[0].toUpperCase() + theme.slice(1)} theme (click to switch)`,
    );
    themeEl.title = `${theme[0].toUpperCase() + theme.slice(1)} theme — click to switch`;
  }
  traceRedraw();
  if (
    document.documentElement.dataset.style === "modern" &&
    (app.data || app.requestData) &&
    !(app.page === "traces" && getTraceRequest())
  ) {
    render();
  }
}

function applyStyle() {
  document.documentElement.dataset.style = app.style;
  const label = app.style === "modern" ? "Modern" : "Default";
  const next = app.style === "modern" ? "Default" : "Modern";
  const styleEl = $("style");
  if (styleEl) {
    styleEl.innerHTML = icon(app.style === "modern" ? "styleModern" : "styleDefault", 15);
    styleEl.setAttribute("aria-label", `${label} style (click to switch to ${next})`);
    styleEl.title = `${label} style — click to switch to ${next}`;
  }
  traceRedraw();
  if ((app.data || app.requestData) && !(app.page === "traces" && getTraceRequest())) render();
}

/** @param {StatsStyle} [next] */
async function requestStyle(next) {
  const response = await fetch("/api/stats/style", {
    cache: "no-store",
    ...(next
      ? {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ style: next }),
        }
      : {}),
  });
  if (!response.ok) throw new Error("Style settings are unavailable. Click Style to retry.");
  const settings = /** @type {StyleSettingsResponse} */ (await response.json());
  if (!["default", "modern"].includes(settings.style)) {
    throw new Error("Invalid style settings. Click Style to retry.");
  }
  app.style = settings.style;
  styleReady = true;
  applyStyle();
}

async function loadStyle() {
  const styleBtn = /** @type {HTMLButtonElement | null} */ ($("style"));
  try {
    await requestStyle();
  } catch (error) {
    toast(error instanceof Error ? error.message : String(error));
  } finally {
    if (styleBtn) styleBtn.disabled = false;
  }
}

/** @param {StatsPage} next */
function navigate(next) {
  window.location.hash = `/${next}?range=${app.range}`;
}

export function syncSegmented() {
  for (const root of document.querySelectorAll(".x-segmented")) {
    const active = root.querySelector('[aria-checked="true"]');
    const thumb = root.querySelector(".x-segmented-thumb");
    if (!(active instanceof HTMLElement) || !(thumb instanceof HTMLElement)) continue;
    thumb.style.left = `${active.offsetLeft}px`;
    thumb.style.width = `${active.offsetWidth}px`;
  }
}

function shell() {
  const rangesEl = $("ranges");
  if (rangesEl) {
    rangesEl.innerHTML = `<span class="x-segmented-thumb"></span>${ranges
      .map(
        (r, index) =>
          `<button type="button" class="x-segmented-option" role="radio" aria-checked="${r === app.range}" data-active="${r === app.range}" data-range="${r}" title="${rangeWindows[r]} (${index + 1})">${r === "all" ? "All" : r}</button>`,
      )
      .join("")}`;
  }
  const navEl = $("navigation");
  if (navEl) {
    navEl.innerHTML = groups
      .map(
        ([name, items]) =>
          `<div class="nav-group"><div class="nav-heading">${h(name)}</div>${items
            .map(
              ([id, label, key]) =>
                `<button type="button" class="nav-row" data-page="${id}" data-active="${id === app.page}" ${id === app.page ? 'aria-current="page"' : ""} title="${h(label)}" aria-label="${h(label)} G ${key}">${icon(id)}<span class="nav-row-label">${h(label)}</span><kbd>G ${key}</kbd></button>`,
            )
            .join("")}</div>`,
      )
      .join("");
  }
  syncSegmented();
}

function closeMenu() {
  const sidebar = $("sidebar");
  const scrim = $("menu-scrim");
  if (sidebar) sidebar.dataset.open = "false";
  if (scrim) scrim.hidden = true;
}

export async function refresh() {
  const token = ++generation;
  controller?.abort();
  controller = new AbortController();
  if (
    ![
      "overview",
      "requests",
      "models",
      "providers",
      "costs",
      "errors",
      "traces",
      "tools",
      "hooks",
      "projects",
    ].includes(app.page)
  ) {
    lastUpdated = Date.now();
    render();
    return;
  }
  try {
    const traceId = getTraceRequest();
    const url =
      app.page === "traces" && traceId
        ? `/api/stats/traces/${traceId}`
        : `/api/stats/${app.page}?range=${app.range}${
            ["requests", "errors", "traces"].includes(app.page)
              ? `&limit=${app.page === "errors" ? getErrorLimit() : app.limit}`
              : ""
          }`;
    const result = await getJSON(url, controller.signal);
    if (token !== generation) return;
    const signature =
      app.page === "traces"
        ? traceSignature(
            /** @type {import("../web-types/api.d.ts").TracesListResponse | import("../web-types/api.d.ts").TraceDetailResponse} */ (
              result
            ),
          )
        : null;
    if (app.page !== "requests") app.data = /** @type {typeof app.data} */ (result);
    else app.requestData = /** @type {RequestsResponse} */ (result);
    lastUpdated = /** @type {{ updated_at?: number }} */ (result).updated_at ?? Date.now();
    const errorEl = $("error");
    const liveEl = $("live");
    if (errorEl) errorEl.hidden = true;
    liveEl?.classList.remove("error");
    // The Traces page keeps its canvas viewport, drawer and scroll position: when the data is unchanged
    // only the Live count is refreshed and nothing is redrawn.
    if (app.page === "traces" && signature === getTraceRenderedSignature()) {
      updateLive();
      return;
    }
    render();
  } catch (error) {
    if ((error instanceof Error && error.name === "AbortError") || token !== generation) return;
    const errorEl = $("error");
    const liveEl = $("live");
    const pageEl = $("page");
    const message = error instanceof Error ? error.message : String(error);
    if (errorEl) {
      errorEl.textContent = `${message} Click Live to retry.`;
      errorEl.hidden = false;
    }
    liveEl?.classList.add("error");
    if (pageEl && !pageEl.hasChildNodes()) {
      pageEl.innerHTML = '<div class="empty">Unable to load statistics.</div>';
    }
  }
  updateLive();
}

function updateLive() {
  const age = lastUpdated ? liveAge(lastUpdated) : "not updated";
  const countEl = $("live")?.querySelector(".live-chip-count");
  if (countEl) countEl.textContent = age;
}

export function onRoute() {
  hideTooltip();
  const url = new URL(window.location.hash.slice(1) || "/overview?range=24h", "http://localhost");
  const next = /** @type {StatsPage} */ (url.pathname.slice(1));
  app.page = pages.some((p) => p[0] === next) ? next : "overview";
  const candidateRange = /** @type {StatsRange | null} */ (url.searchParams.get("range"));
  app.range = candidateRange && ranges.includes(candidateRange) ? candidateRange : "24h";
  app.limit = 500;
  app.visible = 100;
  app.query = "";
  app.status = "all";
  app.sortKey = "timestamp";
  app.sortDirection = -1;
  app.data = null;
  app.requestData = null;
  resetActivity();
  resetTraces(url);
  resetInsights();
  app.expandedGroups.clear();
  app.hiddenSeries.clear();
  resetProviders();
  resetModels();
  closeDrawer();
  shell();
  const pageEl = $("page");
  const errorEl = $("error");
  if (pageEl) pageEl.innerHTML = '<div class="loading">Loading statistics…</div>';
  if (errorEl) errorEl.hidden = true;
  window.scrollTo(0, 0);
  refresh();
}

/**
 * Renders the currently active page and preserves table cell and filter control focus.
 */
export function render() {
  const pageEl = $("page");
  if (!pageEl) return;
  // The enter animation plays only when switching pages or time ranges; in-page interactions and the
  // periodic refresh update the content directly.
  const enter = app.page !== renderedPage || app.range !== renderedRange;
  renderedPage = app.page;
  renderedRange = app.range;
  const focused = document.activeElement;
  const searchFocused = focused?.id === "search";
  const selection =
    searchFocused && focused instanceof HTMLInputElement
      ? [focused.selectionStart, focused.selectionEnd]
      : null;
  const focusedEl = focused instanceof HTMLElement ? focused : null;
  const focusedId = focusedEl?.dataset?.id;
  const providerSelectFocused = focusedEl?.id === "provider-peak";
  const insightFocused =
    focusedEl && ["tool-filter", "insight-sort"].includes(focusedEl.id) ? focusedEl.id : null;
  const focusedStatus = focusedEl?.dataset?.status;
  const expandHost = focusedEl?.closest("[data-expand]");
  const focusedExpand = expandHost instanceof HTMLElement ? expandHost.dataset.expand : undefined;
  const toolsFocus = focusedEl?.dataset?.toolsFocus;
  const hooksFocus = focusedEl?.dataset?.hooksFocus;
  const providerFocus = focusedEl?.dataset?.providerFocus;
  const modelsFocus = focusedEl?.dataset?.modelsFocus;
  const errorsFocus = focusedEl?.dataset?.errorsFocus;
  const focusControl = /** @type {Array<"share" | "costSplit" | "series">} */ ([
    "share",
    "costSplit",
    "series",
  ]).find((k) => focusedEl?.dataset?.[k]);
  const focusValue = focusControl && focusedEl ? focusedEl.dataset[focusControl] : undefined;
  const label = pages.find((p) => p[0] === app.page)?.[1] || app.page;
  const cell = focused?.closest("th,td");
  const cellTable = cell?.closest("table");
  const cellPosition =
    cell instanceof HTMLTableCellElement &&
    cell.parentElement instanceof HTMLTableRowElement &&
    cellTable
      ? [
          [...pageEl.querySelectorAll("table")].indexOf(cellTable),
          cell.parentElement.rowIndex,
          cell.cellIndex,
        ]
      : null;

  /** @type {Partial<Record<StatsPage, () => string>>} */
  const renderers = {
    overview,
    requests,
    errors: errorsPage,
    traces: tracesPage,
    models: modelsPage,
    providers: providersPage,
    costs: costsPage,
    tools: toolsPageWrapper,
    hooks: hooksPage,
    projects: projectsPage,
  };
  pageEl.dataset.enter = enter ? "true" : "false";
  const pageRenderer = renderers[app.page];
  pageEl.innerHTML = pageRenderer
    ? pageRenderer()
    : `<h1>${h(label)}</h1><p class="subtitle">${h(label)} across your sessions in the ${descriptions[app.range]}.</p><section class="panel placeholder"><div class="empty">${h(label)} is coming soon.</div></section>`;
  if (app.page !== "tools" && app.page !== "hooks") prepareDetails();
  syncSegmented();
  if (["tools", "hooks", "providers", "models"].includes(app.page)) wireToolsCharts();
  if (cellPosition) {
    const [tableIdx, rowIdx, colIdx] = cellPosition;
    pageEl.querySelectorAll("table")[tableIdx]?.rows[rowIdx]?.cells[colIdx]?.focus({
      preventScroll: true,
    });
  } else if (toolsFocus) {
    /** @type {HTMLElement | null} */ (
      pageEl.querySelector(`[data-tools-focus="${CSS.escape(toolsFocus)}"]`)
    )?.focus({ preventScroll: true });
  } else if (hooksFocus) {
    /** @type {HTMLElement | null} */ (
      pageEl.querySelector(`[data-hooks-focus="${CSS.escape(hooksFocus)}"]`)
    )?.focus({ preventScroll: true });
  } else if (providerFocus) {
    /** @type {HTMLElement | null} */ (
      pageEl.querySelector(`[data-provider-focus="${CSS.escape(providerFocus)}"]`)
    )?.focus({ preventScroll: true });
  } else if (modelsFocus) {
    /** @type {HTMLElement | null} */ (
      pageEl.querySelector(`[data-models-focus="${CSS.escape(modelsFocus)}"]`)
    )?.focus({ preventScroll: true });
  } else if (errorsFocus) {
    /** @type {HTMLElement | null} */ (
      pageEl.querySelector(`[data-errors-focus="${CSS.escape(errorsFocus)}"]`)
    )?.focus({ preventScroll: true });
  } else if (insightFocused) {
    $(insightFocused)?.focus({ preventScroll: true });
  } else if (providerSelectFocused) {
    $("provider-peak")?.focus({ preventScroll: true });
  } else if (focusControl) {
    const btn = [...pageEl.querySelectorAll("button")].find(
      (el) => el.dataset[focusControl] === focusValue,
    );
    btn?.focus({ preventScroll: true });
  } else if (focusedExpand) {
    const expandRow = [...pageEl.querySelectorAll("[data-expand]")].find(
      (el) => el instanceof HTMLElement && el.dataset.expand === focusedExpand,
    );
    if (expandRow instanceof HTMLElement) expandRow.focus({ preventScroll: true });
  } else if (searchFocused) {
    const searchEl = $("search");
    if (searchEl instanceof HTMLInputElement) {
      searchEl.focus({ preventScroll: true });
      try {
        if (selection) searchEl.setSelectionRange(selection[0], selection[1]);
      } catch {}
    }
  } else if (focusedId) {
    /** @type {HTMLElement | null} */ (pageEl.querySelector(`[data-id="${focusedId}"]`))?.focus({
      preventScroll: true,
    });
  } else if (focusedStatus) {
    /** @type {HTMLElement | null} */ (
      pageEl.querySelector(`[data-status="${focusedStatus}"]`)
    )?.focus({ preventScroll: true });
  }
  if (app.page === "traces") wireTraces();
  else traceRemoveDrawer();
}

/** @returns {string} */
function toolsPageWrapper() {
  return toolsPage();
}

import { toolsPage } from "./insights.js";

/** @type {Record<string, unknown>} */
let payloads = {};

/**
 * @param {string} label
 * @param {string} key
 * @param {unknown} value
 * @param {boolean} [expanded]
 */
function payload(label, key, value, expanded = false) {
  payloads[key] = value;
  const recorded = value != null;
  const copy = recorded
    ? `<button class="copy" data-copy="${key}">${icon("copy", 12)} Copy</button>`
    : "";
  const body = recorded
    ? `<pre><code>${h(JSON.stringify(value, null, 2))}</code></pre>`
    : '<div class="unavailable">Not recorded — pp stores request metrics and the first-request context only.</div>';
  return `<div class="payload"><div class="payload-top"><button class="payload-toggle" data-payload="${key}" aria-expanded="${expanded}" aria-controls="payload-${key}">${icon(expanded ? "down" : "chevron", 12)}${label}</button>${copy}</div><div id="payload-${key}" ${expanded ? "" : "hidden"}>${body}</div></div>`;
}

/** @param {RequestDetailResponse} detail */
function drawerContent(detail) {
  const r = detail.request;
  const trace = new URLSearchParams({
    range: app.range,
    s: r.session_file || "",
    request: String(r.id),
  });
  return `<div class="drawer-header"><div class="drawer-heading"><div class="drawer-identity"><h2>${h(r.model)}</h2><p class="panel-description">${h(r.provider)} · ${relative(r.timestamp)}</p></div><div class="drawer-actions">${requestContextButton(detail)}<a class="drawer-link" href="#/traces?${trace}">${icon("branch", 14)} Trace</a>${statusBadge(r, true)}<button class="close" aria-label="Close">${icon("close", 18)}</button></div></div></div><div class="drawer-body">${r.error_message ? `<div class="detail-error">${h(r.error_message)}</div>` : ""}<div class="detail-title">Timing</div>${detailsGrid(
    [
      ["Started", localTime(r.timestamp)],
      ["Duration", duration(r.duration_ms)],
      ["Time to first token", duration(r.ttft_ms)],
      [
        "Output tokens/s",
        r.output_tokens != null && r.duration_ms > 0
          ? ((r.output_tokens * 1000) / r.duration_ms).toFixed(1)
          : "—",
      ],
    ],
  )}<div class="detail-title">Tokens</div>${detailsGrid([
    ["Uncached input", number(uncached(r))],
    ["Cache read", number(r.cached_tokens)],
    ["Cache write", number(r.cache_write_tokens)],
    ["Output", number(r.output_tokens)],
    ["Total", number(total(r))],
    ["Premium requests", "—"],
  ])}<div class="detail-title">API-equivalent cost</div>${detailsGrid([
    ["Total", money(r.cost)],
    ["Input", money(r.cost_components?.input)],
    ["Cache read", money(r.cost_components?.cache_read)],
    ["Cache write", money(r.cost_components?.cache_write)],
    ["Output", money(r.cost_components?.output)],
  ])}<div class="detail-title">Identity</div>${detailsGrid([
    ["Request id", r.id],
    ["Entry id", r.entry_id || "—"],
    ["Stop reason", r.stop_reason || "—"],
    ["API", r.api],
    ["Project", r.project],
    ["Purpose", r.purpose],
    ["Model reference", r.model_reference],
  ])}<div class="detail-title">Session file</div><div class="session-path">${h(r.session_file || "Not recorded for this request.")}</div>${payload("Stats row", "stats", r)}</div>`;
}

/**
 * Opens the right-side request detail drawer and loads the request detail.
 * @param {number} id
 * @param {Element | null} [element]
 */
export async function openDrawer(id, element = null) {
  hideTooltip();
  returnFocus = element;
  selectedId = id;
  const token = ++detailGeneration;
  detailController?.abort();
  detailController = new AbortController();
  const overlay = $("overlay");
  const drawer = $("drawer");
  if (!overlay || !drawer) return;
  overlay.hidden = false;
  document.body.style.overflow = "hidden";
  for (const el of document.querySelectorAll("header,#sidebar,main")) {
    if (el instanceof HTMLElement) el.inert = true;
  }
  drawer.innerHTML = `<div class="drawer-header"><h2>Request details</h2><button class="close" aria-label="Close">${icon("close", 18)}</button></div><div class="loading">Loading request…</div>`;
  drawer.focus();
  try {
    const detail = /** @type {RequestDetailResponse} */ (
      await getJSON(`/api/stats/requests/${id}`, detailController.signal)
    );
    if (token !== detailGeneration) return;
    payloads = {};
    app.drawerContextDetail = detail;
    drawer.innerHTML = drawerContent(detail);
    const closeBtn = drawer.querySelector(".close");
    if (closeBtn instanceof HTMLElement) closeBtn.focus({ preventScroll: true });
  } catch (error) {
    if ((error instanceof Error && error.name === "AbortError") || token !== detailGeneration)
      return;
    const message = error instanceof Error ? error.message : String(error);
    drawer.innerHTML = `<div class="drawer-header"><h2>Request details</h2><button class="close" aria-label="Close">${icon("close", 18)}</button></div><div class="empty">${h(message)}</div>`;
    const closeBtn = drawer.querySelector(".close");
    if (closeBtn instanceof HTMLElement) closeBtn.focus();
  }
}

export function closeDrawer() {
  closeContextReader();
  const overlay = $("overlay");
  if (!overlay || overlay.hidden) return;
  detailGeneration++;
  detailController?.abort();
  overlay.hidden = true;
  document.body.style.overflow = "";
  for (const el of document.querySelectorAll("header,#sidebar,main")) {
    if (el instanceof HTMLElement) el.inert = false;
  }
  const target = returnFocus?.isConnected
    ? returnFocus
    : $("page")?.querySelector(`[data-id="${selectedId}"]`);
  if (target instanceof HTMLElement) target.focus({ preventScroll: true });
  selectedId = null;
  payloads = {};
  app.drawerContextDetail = null;
}

/** @param {string} text */
export function toast(text) {
  clearTimeout(toastTimer);
  const el = $("toast");
  if (!el) return;
  el.textContent = text;
  el.hidden = false;
  toastTimer = setTimeout(() => {
    el.hidden = true;
  }, 3500);
}

let booted = false;

/**
 * Assembles the shell, event delegation and periodic refresh, then starts the first-screen route.
 */
export function boot() {
  registerHooks({ render, refresh, openDrawer, toast, hideTooltip });
  initTooltip();
  initContextReader();
  initTraces();
  initInsights();
  if (booted) return;
  booted = true;

  theme = readSavedTheme();
  applyTheme();
  window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
    if (theme === "system") applyTheme();
  });

  const themeBtn = $("theme");
  if (themeBtn) {
    themeBtn.onclick = () => {
      /** @type {StatsTheme[]} */
      const order = ["system", "light", "dark"];
      theme = order[(order.indexOf(theme) + 1) % order.length];
      try {
        localStorage.setItem("pp-stats-theme", theme);
      } catch {}
      applyTheme();
    };
  }

  const styleBtn = /** @type {HTMLButtonElement | null} */ ($("style"));
  if (styleBtn) {
    styleBtn.onclick = async () => {
      styleBtn.disabled = true;
      if (!styleReady) return loadStyle();
      try {
        await requestStyle(app.style === "default" ? "modern" : "default");
        toast(app.style === "modern" ? "Modern style saved." : "Default style saved.");
      } catch (error) {
        toast(error instanceof Error ? error.message : String(error));
      } finally {
        styleBtn.disabled = false;
      }
    };
  }
  applyStyle();
  loadStyle();

  const menuBtn = $("menu");
  if (menuBtn) {
    menuBtn.onclick = () => {
      const sidebar = $("sidebar");
      const scrim = $("menu-scrim");
      if (sidebar) sidebar.dataset.open = "true";
      if (scrim) scrim.hidden = false;
    };
  }
  const scrimBtn = $("menu-scrim");
  if (scrimBtn) scrimBtn.onclick = closeMenu;

  window.addEventListener(
    "scroll",
    () => {
      const topbar = $("topbar");
      if (topbar) topbar.dataset.scrolled = String(window.scrollY > 4);
    },
    { passive: true },
  );
  window.addEventListener("resize", syncSegmented);

  const rangesEl = $("ranges");
  if (rangesEl) {
    rangesEl.onclick = (e) => {
      const button = e.target instanceof Element ? e.target.closest("[data-range]") : null;
      if (button instanceof HTMLElement && button.dataset.range) {
        app.range = /** @type {StatsRange} */ (button.dataset.range);
        navigate(app.page);
      }
    };
  }
  const navEl = $("navigation");
  if (navEl) {
    navEl.onclick = (e) => {
      const button = e.target instanceof Element ? e.target.closest("[data-page]") : null;
      if (button instanceof HTMLElement && button.dataset.page) {
        closeMenu();
        navigate(/** @type {StatsPage} */ (button.dataset.page));
      }
    };
  }
  const liveBtn = $("live");
  if (liveBtn) liveBtn.onclick = refresh;

  window.addEventListener("hashchange", onRoute);

  const pageEl = $("page");
  pageEl?.addEventListener("input", (e) => {
    if (activityInput(e)) return;
    if (e.target instanceof HTMLInputElement && e.target.id === "search") {
      app.query = e.target.value;
      app.visible = 100;
      render();
    }
  });
  pageEl?.addEventListener("click", (e) => {
    if (tracesClick(e)) return;
    if (insightsClick(e)) return;
    if (providersClick(e)) return;
    if (modelsClick(e)) return;
    if (activityClick(e)) return;
    if (!(e.target instanceof Element)) return;
    const expand = e.target.closest("[data-expand]");
    if (expand instanceof HTMLElement && expand.dataset.expand) {
      const key = expand.dataset.expand;
      if (app.expandedGroups.has(key)) app.expandedGroups.delete(key);
      else app.expandedGroups.add(key);
      render();
      return;
    }
    const control = e.target.closest("button");
    if (
      control instanceof HTMLElement &&
      ["share", "costSplit", "series"].some((k) => control.dataset[k])
    ) {
      const d = control.dataset;
      if (d.share) app.shareMode = d.share === "requests" ? "requests" : "share";
      if (d.costSplit) app.costMode = d.costSplit === "component" ? "component" : "model";
      if (d.series) {
        if (app.hiddenSeries.has(d.series)) app.hiddenSeries.delete(d.series);
        else app.hiddenSeries.add(d.series);
      }
      render();
      return;
    }
    const row = e.target.closest("[data-id]");
    if (row instanceof HTMLElement && row.dataset.id) {
      openDrawer(Number(row.dataset.id), row);
      return;
    }
    const button = e.target.closest("button,th");
    if (!(button instanceof HTMLElement)) return;
    if (button.dataset.activity) {
      app.activity = /** @type {"requests" | "tokens" | "cost"} */ (button.dataset.activity);
      render();
    } else if (button.dataset.status) {
      app.status = button.dataset.status;
      app.visible = 100;
      render();
    } else if (button.dataset.sort) {
      if (app.sortKey === button.dataset.sort) app.sortDirection *= -1;
      else {
        app.sortKey = button.dataset.sort;
        app.sortDirection = ["model", "project", "status"].includes(app.sortKey) ? 1 : -1;
      }
      render();
    } else if (button.hasAttribute("data-more")) {
      app.visible += 100;
      render();
    } else if (button.hasAttribute("data-load")) {
      app.limit = 2000;
      app.visible = 100;
      refresh();
    }
  });
  pageEl?.addEventListener("keydown", (e) => {
    if (!(e.target instanceof Element)) return;
    const action = e.target.closest(
      "th[data-sort],tr[data-id],tr[data-expand],tr[data-error-signature],th[data-errorsort],tr[data-trace],th[data-toolsort],tr[data-toolselect],th[data-hooksort],tr[data-hookevent],th[data-provider-sort],tr[data-provider-row],tr[data-model-row]",
    );
    if (
      (e.key === "Enter" || e.key === " ") &&
      action instanceof HTMLElement &&
      !e.target.closest("button,a,input,select,textarea")
    ) {
      e.preventDefault();
      action.click();
    }
  });
  pageEl?.addEventListener("change", (e) => {
    if (insightsChange(e)) return;
    if (providersChange(e)) return;
  });

  const overlayEl = $("overlay");
  if (overlayEl) {
    overlayEl.onclick = (e) => {
      if (e.target === overlayEl || (e.target instanceof Element && e.target.closest(".close"))) {
        closeDrawer();
      }
    };
  }
  const drawerEl = $("drawer");
  drawerEl?.addEventListener("click", async (e) => {
    if (!(e.target instanceof Element)) return;
    const context = e.target.closest("[data-context-id]");
    if (context) {
      openContextReader(app.drawerContextDetail, context);
      return;
    }
    const toggle = e.target.closest("[data-payload]");
    if (toggle instanceof HTMLElement && toggle.dataset.payload) {
      const open = toggle.getAttribute("aria-expanded") !== "true";
      toggle.setAttribute("aria-expanded", String(open));
      const svg = toggle.querySelector("svg");
      if (svg) svg.outerHTML = icon(open ? "down" : "chevron", 12);
      const targetPayload = $(`payload-${toggle.dataset.payload}`);
      if (targetPayload) targetPayload.hidden = !open;
    }
    const copy = e.target.closest("[data-copy]");
    if (copy instanceof HTMLElement && copy.dataset.copy) {
      const value = payloads[copy.dataset.copy];
      if (value == null) {
        toast("No payload recorded.");
        return;
      }
      try {
        await navigator.clipboard.writeText(JSON.stringify(value, null, 2));
        toast("Copied to clipboard");
      } catch {
        toast("Copy failed. Select the text and copy manually.");
      }
    }
  });

  document.addEventListener("keydown", (e) => {
    if (contextReaderKeydown(e)) return;
    const overlay = $("overlay");
    if (overlay && !overlay.hidden) {
      if (e.key === "Escape") {
        e.preventDefault();
        closeDrawer();
      }
      if (e.key === "Tab") {
        const focusable = [
          ...($("drawer")?.querySelectorAll('button,a[href],input,[tabindex="0"]') ?? []),
        ].filter((el) => el instanceof HTMLElement && !el.closest("[hidden]"));
        const first = focusable[0];
        const last = focusable.at(-1);
        if (
          e.shiftKey &&
          (document.activeElement === first || document.activeElement === $("drawer")) &&
          last instanceof HTMLElement
        ) {
          e.preventDefault();
          last.focus();
        } else if (!e.shiftKey && document.activeElement === last && first instanceof HTMLElement) {
          e.preventDefault();
          first.focus();
        }
      }
      return;
    }
    if (
      (e.target instanceof Element &&
        e.target.closest('input,textarea,select,[contenteditable="true"]')) ||
      e.metaKey ||
      e.ctrlKey ||
      e.altKey
    ) {
      return;
    }
    const key = e.key.toLowerCase();
    if (gTime && Date.now() - gTime < 1000) {
      gTime = 0;
      const found = pages.find((p) => p[2].toLowerCase() === key);
      if (found) {
        e.preventDefault();
        navigate(found[0]);
        return;
      }
    }
    if (key === "g") {
      gTime = Date.now();
      return;
    }
    if (/^[1-6]$/.test(key)) {
      e.preventDefault();
      app.range = ranges[Number(key) - 1];
      navigate(app.page);
    }
  });

  setInterval(() => {
    if (!document.hidden) refresh();
  }, 5000);
  setInterval(updateLive, 1000);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) refresh();
  });
  onRoute();
}

if (typeof document !== "undefined" && document.getElementById("ranges")) {
  boot();
}
