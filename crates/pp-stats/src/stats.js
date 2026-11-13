"use strict";
// 所有动态内容在插入页面前转义；模型输出只作为文本展示。
const $ = (id) => document.getElementById(id);
const h = (value) =>
  String(value ?? "").replace(
    /[&<>"']/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        c
      ],
  );
const paths = {
  overview: "M3 3h6v6H3z M15 3h6v6h-6z M3 15h6v6H3z M15 15h6v6h-6z",
  models:
    "M7 3v3 M17 3v3 M7 18v3 M17 18v3 M3 7h3 M18 7h3 M3 17h3 M18 17h3 M6 6h12v12H6z M9 9h6v6H9z",
  providers:
    "m14 4 6 6 m-9 3 6-6 M4 14l6 6 m-3-3 6-6 M4 10 2 8l6-6 2 2 M14 20l2 2 6-6-2-2",
  costs:
    "M14 3a7 7 0 1 1-7 7 M3 14a7 7 0 0 0 7 7 M14 7v6 m2-5h-3a1 1 0 0 0 0 2h2a1 1 0 0 1 0 2h-3",
  requests: "M2 12h5l3-9 4 18 3-9h5",
  errors: "M12 8v5 M12 16h.01 M22 12a10 10 0 1 1-20 0 10 10 0 0 1 20 0",
  traces: "M4 3h16v18H4z M8 7h8 M8 11h8 M8 15h5",
  branch: "M6 3v12 M6 15a3 3 0 1 1 0 6 3 3 0 0 1 0-6 M18 3a3 3 0 1 1 0 6 3 3 0 0 1 0-6 M6 15a9 9 0 0 1 9-9",
  tools:
    "m14 7-4 4 3 3 4-4 M14 7a6 6 0 0 0 7 7L9 22l-5-5 8-10a6 6 0 0 1 7-7l-4 4z",
  frustration: "M4 12a8 8 0 0 1 16 0v7h-4v-7h4 M4 12v7h4v-7z M16 19v3h-4",
  projects: "M3 7V4h6l2 3h10v8 M3 7v13h7 M16 14v8 m-4-4h8",
  gain: "m3 17 5-6 4 3 8-10 M16 4h4v4 M5 3v4 M3 5h4 M17 17v4 M15 19h4",
  arrow: "M4 12h16 m-6-6 6 6-6 6",
  search: "m21 21-5-5 M18 10a8 8 0 1 1-16 0 8 8 0 0 1 16 0",
  close: "m6 6 12 12 M6 18 18 6",
  copy: "M9 9h12v12H9z M15 9V3H3v12h6",
  chevron: "m9 5 7 7-7 7",
  down: "m5 9 7 7 7-7",
  sun: "M12 3v2 M12 19v2 M3 12h2 M19 12h2 m-15-6 2 2 m10 10 2 2 M6 18l2-2 M16 8l2-2 M16 12a4 4 0 1 1-8 0 4 4 0 0 1 8 0",
  moon: "M21 13A9 9 0 0 1 11 3a9 9 0 1 0 10 10",
  system: "M3 3h18v13H3z M12 16v5 M8 21h8",
  styleDefault: "M3 4h18v16H3z M3 9h18 M9 9v11",
  styleModern: "m12 3 2.5 6.5L21 12l-6.5 2.5L12 21l-2.5-6.5L3 12l6.5-2.5z M20 2v4 M18 4h4",
  flag: "M5 21V4 M5 4h12l-3 4 3 4H5",
  refresh: "M21 12a9 9 0 1 1-2.6-6.4 M21 3v6h-6",
  alert: "M12 9v4 M12 17h.01 M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z",
  check: "m20 6-11 11-5-5",
  inbox: "M22 12h-6l-2 3h-4l-2-3H2 M5.5 5h13l3.5 7v7H2v-7z",
};
const icon = (name, size = 15) =>
  `<svg width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="${paths[name] || paths.traces}"/></svg>`;
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
      ["frustration", "Frustration", "F"],
      ["projects", "Projects", "J"],
      ["gain", "Gain", "G"],
    ],
  ],
];
const pages = groups.flatMap((g) => g[1]);
const ranges = ["1h", "24h", "7d", "30d", "90d", "all"];
const descriptions = {
  "1h": "last hour",
  "24h": "last 24 hours",
  "7d": "last 7 days",
  "30d": "last 30 days",
  "90d": "last 90 days",
  all: "entire history",
};
const rangeWindows = {
  "1h": "the last hour",
  "24h": "the last 24 hours",
  "7d": "the last 7 days",
  "30d": "the last 30 days",
  "90d": "the last 90 days",
  all: "all time",
};
const number = (n) => (n == null ? "—" : Number(n).toLocaleString("en-US"));
const short = (n) => {
  if (n == null) return "—";
  if (n < 1000) return number(n);
  let unit = n >= 1e9 ? 1e9 : n >= 1e6 ? 1e6 : 1e3;
  return `${Number((n / unit).toFixed(n / unit < 10 ? 1 : 0))}${unit === 1e9 ? "B" : unit === 1e6 ? "M" : "K"}`;
};
const duration = (ms) =>
  ms == null
    ? "—"
    : ms < 1000
      ? `${(ms / 1000).toFixed(2)}s`
      : `${(ms / 1000).toFixed(1)}s`;
const percent = (n, d) =>
  n != null && d > 0
    ? `${((100 * n) / d).toFixed((100 * n) / d < 0.1 ? 2 : 1)}%`
    : "—";
const total = (r) =>
  r.input_tokens == null || r.output_tokens == null
    ? null
    : r.input_tokens + r.output_tokens;
const uncached = (r) =>
  r.input_tokens != null &&
  r.cached_tokens != null &&
  r.cache_write_tokens != null &&
  r.cached_tokens >= 0 &&
  r.cache_write_tokens >= 0 &&
  r.cached_tokens <= r.input_tokens &&
  r.cache_write_tokens <= r.input_tokens - r.cached_tokens
    ? r.input_tokens - r.cached_tokens - r.cache_write_tokens
    : null;
const sum = (values) => {
  const known = values.filter((v) => v != null);
  return values.length && !known.length
    ? null
    : known.reduce((a, b) => a + b, 0);
};
const quantile = (values, q) => {
  const sorted = values.filter((v) => v != null).sort((a, b) => a - b);
  if (!sorted.length) return null;
  const x = (sorted.length - 1) * q,
    i = Math.floor(x);
  return sorted[i] + (sorted[Math.ceil(x)] - sorted[i]) * (x - i);
};
// 相对时间与参考实现一致：30 秒以内不按分钟计，超过 30 天按月折算。
const relative = (ms) => {
  const minutes = Math.max(0, (Date.now() - ms) / 60000);
  const seconds = minutes * 60;
  if (seconds < 30) return "less than a minute ago";
  if (minutes < 1.5) return "1 minute ago";
  if (minutes < 45) return `${Math.round(minutes)} minutes ago`;
  if (minutes < 90) return "about 1 hour ago";
  if (minutes < 1440) return `about ${Math.round(minutes / 60)} hours ago`;
  if (minutes < 2520) return "1 day ago";
  if (minutes < 43200) return `${Math.round(minutes / 1440)} days ago`;
  if (minutes < 86400) return "about 1 month ago";
  const months = Math.round(minutes / 43200);
  if (months < 12) return `${months} months ago`;
  const years = Math.round(months / 12);
  if (years === 1) return "about 1 year ago";
  if (years < 2) return "over 1 year ago";
  return `about ${years} years ago`;
};
// Live 计数用短写法，与参考实现的 topbar 一致。
const liveAge = (ms) => {
  const minutes = Math.floor(Math.max(0, Date.now() - ms) / 60000);
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
};
const localTime = (ms) =>
  new Date(ms).toLocaleString("en-US", {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
// 详情统一使用纯文本，避免模型名、目录或错误消息被解释为 HTML。
const detailed = (value, display = short(value), unit = "") =>
  `<span title="${h(value == null ? "Not reported" : `${Number(value).toLocaleString("en-US", { maximumSignificantDigits: 21 })}${unit}`)}">${display}</span>`;
const chartHit = (x, y, width, height, text) =>
  `<rect class="chart-hit" x="${x}" y="${y}" width="${width}" height="${height}" tabindex="0" role="graphics-symbol" aria-label="${h(text)}" data-tooltip="${h(text)}"/>`;
const tooltip = document.createElement("div");
tooltip.id = "stats-tooltip";
tooltip.role = "tooltip";
tooltip.hidden = true;
document.body.append(tooltip);
let tooltipTarget = null;
function hideTooltip() {
  tooltipTarget?.removeAttribute("aria-describedby");
  tooltipTarget = null;
  tooltip.hidden = true;
}
function showTooltip(target, x, y) {
  if (tooltipTarget !== target) {
    hideTooltip();
    tooltipTarget = target;
    tooltip.textContent = target.dataset.tooltip;
    target.setAttribute("aria-describedby", tooltip.id);
    tooltip.hidden = false;
  }
  const box = tooltip.getBoundingClientRect();
  tooltip.style.left = `${Math.max(8, Math.min(x + 12, innerWidth - box.width - 8))}px`;
  tooltip.style.top = `${Math.max(8, y + 16 + box.height > innerHeight - 8 ? y - box.height - 12 : y + 16)}px`;
}
function prepareDetails() {
  hideTooltip();
  const root = $("page");
  for (const title of root.querySelectorAll("svg title")) {
    title.parentElement.dataset.tooltip = title.textContent;
    title.remove();
  }
  for (const el of root.querySelectorAll("[title]")) {
    el.dataset.tooltip = el.title;
    el.removeAttribute("title");
  }
  const meanings = {
    Tokens: "Total input (including cache reads and writes) + output tokens",
    "Cache rate": "Cache reads / total input",
    "Tokens/s": "Output tokens / request duration in seconds",
    TTFT: "Time from request start to the first output token",
    Cost: "API-equivalent estimate in USD; excludes unpriced requests",
    "Attr. tokens": "Invoking request tokens split evenly across its tool calls",
    "Attr. cost": "Invoking request estimate split evenly across its tool calls",
  };
  // 覆盖所有表格（包括展开的嵌套表格），保留已有的精确数值和完整路径。
  for (const cell of root.querySelectorAll("th,td:not([colspan])")) {
    if (cell.querySelector("table")) continue;
    const header = cell.closest("table").tHead?.rows[0]?.cells[cell.cellIndex];
    const values = [...cell.querySelectorAll("[data-tooltip]:not(.chart-hit)")]
      .map((el) => el.dataset.tooltip);
    const text = cell.tagName === "TH" && meanings[cell.textContent.trim()]
      || (cell.querySelector("svg.sparkline")
        ? "Move across the trend or focus a time bucket to inspect values."
        : [...new Set([cell.innerText.trim(), ...values].filter(Boolean))].join("\n"));
    cell.dataset.tooltip = `${header?.textContent.trim() || "Details"}\n${cell.dataset.tooltip || text || "Not reported"}`;
    if (!cell.hasAttribute("tabindex")) cell.tabIndex = 0;
  }
}
$("page").addEventListener("pointermove", (e) => {
  if (e.pointerType === "touch") return;
  const target = e.target.closest("[data-tooltip]");
  if (target) showTooltip(target, e.clientX, e.clientY);
  else hideTooltip();
});
$("page").addEventListener("pointerleave", hideTooltip);
$("page").addEventListener("focusin", (e) => {
  const target = e.target.closest("[data-tooltip]");
  if (!target) return;
  const box = target.getBoundingClientRect();
  showTooltip(target, box.left, box.bottom);
});
$("page").addEventListener("focusout", hideTooltip);
$("page").addEventListener("pointerdown", hideTooltip);
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") hideTooltip();
});
document.addEventListener("scroll", hideTooltip, true);
window.addEventListener("resize", hideTooltip);
let page = "overview",
  range = "24h",
  data = null,
  requestData = null,
  limit = 500,
  visible = 100,
  query = "",
  status = "all",
  sortKey = "timestamp",
  sortDirection = -1,
  activity = "requests",
  generation = 0,
  controller = null,
  detailController = null,
  detailGeneration = 0,
  selectedId = null,
  returnFocus = null,
  lastUpdated = null,
  gTime = 0,
  renderedPage = null,
  renderedRange = null,
  toastTimer;
let theme = "system";
try {
  theme = localStorage.getItem("pp-stats-theme") || "system";
} catch {}
if (!["system", "light", "dark"].includes(theme)) theme = "system";
function applyTheme() {
  const dark = matchMedia("(prefers-color-scheme: dark)").matches;
  const resolved = theme === "system" ? (dark ? "dark" : "light") : theme;
  document.documentElement.dataset.theme = resolved;
  document.documentElement.style.colorScheme = resolved;
  $("theme").innerHTML = icon(
    theme === "light" ? "sun" : theme === "dark" ? "moon" : "system",
    15,
  );
  $("theme").setAttribute(
    "aria-label",
    `${theme[0].toUpperCase() + theme.slice(1)} theme (click to switch)`,
  );
  $("theme").title = `${theme[0].toUpperCase() + theme.slice(1)} theme — click to switch`;
}
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
  if (theme === "system") applyTheme();
});
$("theme").onclick = () => {
  theme = ["system", "light", "dark"][
    (["system", "light", "dark"].indexOf(theme) + 1) % 3
  ];
  try {
    localStorage.setItem("pp-stats-theme", theme);
  } catch {}
  applyTheme();
};
applyTheme();
let style = "default", styleReady = false;
function applyStyle() {
  document.documentElement.dataset.style = style;
  const label = style === "modern" ? "Modern" : "Default";
  const next = style === "modern" ? "Default" : "Modern";
  $("style").innerHTML = icon(style === "modern" ? "styleModern" : "styleDefault", 15);
  $("style").setAttribute("aria-label", `${label} style (click to switch to ${next})`);
  $("style").title = `${label} style — click to switch to ${next}. Modern appearance is coming soon.`;
}
async function requestStyle(next) {
  const response = await fetch("/api/stats/style", {
    cache: "no-store",
    ...(next ? {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ style: next }),
    } : {}),
  });
  if (!response.ok) throw new Error("Style settings are unavailable. Click Style to retry.");
  const settings = await response.json();
  if (!["default", "modern"].includes(settings.style))
    throw new Error("Invalid style settings. Click Style to retry.");
  style = settings.style;
  styleReady = true;
  applyStyle();
}
async function loadStyle() {
  try {
    await requestStyle();
  } catch (error) {
    toast(error.message);
  } finally {
    $("style").disabled = false;
  }
}
$("style").onclick = async () => {
  $("style").disabled = true;
  if (!styleReady) return loadStyle();
  try {
    await requestStyle(style === "default" ? "modern" : "default");
    toast(style === "modern" ? "Modern selected. Its appearance is coming soon; using Default for now." : "Default style saved.");
  } catch (error) {
    toast(error.message);
  } finally {
    $("style").disabled = false;
  }
};
applyStyle();
loadStyle();
function navigate(next) {
  location.hash = `/${next}?range=${range}`;
}
function syncSegmented() {
  for (const root of document.querySelectorAll(".x-segmented")) {
    const active = root.querySelector('[aria-checked="true"]');
    const thumb = root.querySelector(".x-segmented-thumb");
    if (!active || !thumb) continue;
    thumb.style.left = `${active.offsetLeft}px`;
    thumb.style.width = `${active.offsetWidth}px`;
  }
}
function shell() {
  $("ranges").innerHTML = `<span class="x-segmented-thumb"></span>${ranges
    .map(
      (r, index) =>
        `<button type="button" class="x-segmented-option" role="radio" aria-checked="${r === range}" data-active="${r === range}" data-range="${r}" title="${rangeWindows[r]} (${index + 1})">${r === "all" ? "All" : r}</button>`,
    )
    .join("")}`;
  $("navigation").innerHTML = groups
    .map(
      ([name, items]) =>
        `<div class="nav-group"><div class="nav-heading">${h(name)}</div>${items
          .map(
            ([id, label, key]) =>
              `<button type="button" class="nav-row" data-page="${id}" data-active="${id === page}" ${id === page ? 'aria-current="page"' : ""} title="${h(label)}" aria-label="${h(label)} G ${key}">${icon(id)}<span class="nav-row-label">${h(label)}</span><kbd>G ${key}</kbd></button>`,
          )
          .join("")}</div>`,
    )
    .join("");
  syncSegmented();
}
function closeMenu() {
  $("sidebar").dataset.open = "false";
  $("menu-scrim").hidden = true;
}
$("menu").onclick = () => {
  $("sidebar").dataset.open = "true";
  $("menu-scrim").hidden = false;
};
$("menu-scrim").onclick = closeMenu;
window.addEventListener(
  "scroll",
  () => ($("topbar").dataset.scrolled = String(scrollY > 4)),
  { passive: true },
);
window.addEventListener("resize", syncSegmented);
$("ranges").onclick = (e) => {
  const button = e.target.closest("[data-range]");
  if (button) {
    range = button.dataset.range;
    navigate(page);
  }
};
$("navigation").onclick = (e) => {
  const button = e.target.closest("[data-page]");
  if (button) {
    closeMenu();
    navigate(button.dataset.page);
  }
};
async function getJSON(url, signal) {
  const response = await fetch(url, { signal, cache: "no-store" });
  if (!response.ok)
    throw new Error(
      response.status === 404
        ? "Request no longer exists."
        : response.status === 503
          ? "Statistics database is temporarily unavailable."
          : `Statistics request failed (${response.status}).`,
    );
  return response.json();
}
async function refresh() {
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
      "projects",
    ].includes(page)
  ) {
    lastUpdated = Date.now();
    render();
    return;
  }
  try {
    const result = await getJSON(
      page === "traces" && traceRequest
        ? `/api/stats/traces/${traceRequest}`
        : `/api/stats/${page}?range=${range}${
            ["requests", "errors", "traces"].includes(page)
              ? `&limit=${page === "errors" ? errorLimit : limit}`
              : ""
          }`,
      controller.signal,
    );
    if (token !== generation) return;
    const signature = page === "traces" ? traceSignature(result) : null;
    if (page !== "requests") data = result;
    else requestData = result;
    lastUpdated = result.updated_at;
    $("error").hidden = true;
    $("live").classList.remove("error");
    // Traces 页保留画布视口、抽屉与滚动位置：数据未变时只刷新 Live 计数，不重绘。
    if (page === "traces" && signature === traceRenderedSignature) {
      updateLive();
      return;
    }
    render();
  } catch (error) {
    if (error.name === "AbortError" || token !== generation) return;
    $("error").textContent = `${error.message} Click Live to retry.`;
    $("error").hidden = false;
    $("live").classList.add("error");
    if (!$("page").hasChildNodes())
      $("page").innerHTML =
        '<div class="empty">Unable to load statistics.</div>';
  }
  updateLive();
}
function updateLive() {
  const age = lastUpdated ? liveAge(lastUpdated) : "not updated";
  $("live").querySelector(".live-chip-count").textContent = age;
}
$("live").onclick = refresh;
function onRoute() {
  hideTooltip();
  const url = new URL(
    location.hash.slice(1) || "/overview?range=24h",
    "http://localhost",
  );
  const next = url.pathname.slice(1);
  page = pages.some((p) => p[0] === next) ? next : "overview";
  range = ranges.includes(url.searchParams.get("range"))
    ? url.searchParams.get("range")
    : "24h";
  limit = 500;
  visible = 100;
  query = "";
  status = "all";
  sortKey = "timestamp";
  sortDirection = -1;
  data = null;
  requestData = null;
  resetActivity();
  resetTraces(url);
  resetInsights();
  expandedGroups.clear();
  hiddenSeries.clear();
  providerPeak = "";
  providerMetric = "tokens";
  providerSort = { key: "tokens", dir: "desc" };
  providerSelected = null;
  providerShown = PROVIDER_LIMIT;
  resetModels();
  closeDrawer();
  shell();
  $("page").innerHTML = '<div class="loading">Loading statistics…</div>';
  $("error").hidden = true;
  window.scrollTo(0, 0);
  refresh();
}
window.addEventListener("hashchange", onRoute);
function metric(label, value, note = "", spark = "", color = "var(--accent)") {
  return `<div class="metric"><div class="metric-label">${h(label)}</div><div class="metric-value" title="${h(value)}">${h(value)}</div>${note ? `<div class="metric-note">${h(note)}</div>` : ""}${sparkline(spark, color, label, fillBuckets(data?.buckets || []).map((b) => b.timestamp))}</div>`;
}
function sparkline(values, color, label = "Requests", timestamps = []) {
  if (!Array.isArray(values)) return "";
  if (!values.length) return '<div class="sparkline"></div>';
  const max = Math.max(...values, 1),
    points = values
      .map(
        (v, i) =>
          `${values.length === 1 ? 100 : (i * 260) / (values.length - 1)},${35 - (v / max) * 30}`,
      )
      .join(" ");
  return `<svg class="sparkline" viewBox="0 0 260 38" preserveAspectRatio="none"><polygon points="0,38 ${points} 260,38" fill="${color}" opacity=".12"/><polyline points="${points}" fill="none" stroke="${color}" stroke-width="1.6"/>${values.map((v, i) => chartHit(i * 260 / values.length, 0, 260 / values.length, 38, `${timestamps[i] == null ? `Bucket ${i + 1}` : localTime(timestamps[i])}\n${label}: ${label.includes("cost") ? money(v) : label === "Error rate" ? percent(v, 1) : number(v)}`)).join("")}</svg>`;
}
function statusBadge(r, compact = false) {
  const label =
    { ok: "OK", failed: "Failed", aborted: "Aborted" }[r.status] || "Unknown";
  return compact
    ? `<span class="badge ${h(r.status)}">${label}</span>`
    : `<div class="status-text ${h(r.status)}"><i></i><div>${label}<div class="stop">${h(r.stop_reason || "")}</div></div></div>`;
}
function overviewTable(rows) {
  return `<div class="table-wrap"><table><thead><tr>${["Model", "When", "Tokens", "Cost", "Duration", "Status"].map((s, i) => `<th${i > 1 ? ' class="right"' : ""}>${s}</th>`).join("")}</tr></thead><tbody>${rows.length ? rows.map((r) => `<tr tabindex="0" data-id="${r.id}" aria-label="View ${h(r.model)} request ${r.id}"><td><div class="model-name">${h(r.model)}</div><div class="provider">${h(r.provider)}</div></td><td title="${h(localTime(r.timestamp))}">${relative(r.timestamp)}</td><td class="right num">${number(total(r))}</td><td class="right num">${detailed(r.cost, money(r.cost), " USD")}</td><td class="right num">${detailed(r.duration_ms, duration(r.duration_ms), " ms")}</td><td class="right">${statusBadge(r, true)}</td></tr>`).join("") : '<tr><td colspan="6"><div class="empty">No requests in this range</div></td></tr>'}</tbody></table></div>`;
}
function fillBuckets(buckets) {
  if (!buckets.length) return [];
  const now = data.updated_at;
  let start;
  if (range === "1h") start = now - 3600000;
  else if (range === "24h") start = now - 86400000;
  else if (range === "all") start = buckets[0].timestamp;
  else start = now - Number.parseInt(range) * 86400000;
  const fixed = range === "1h" ? 300000 : range === "24h" ? 3600000 : null;
  let t;
  if (fixed) t = Math.floor(start / fixed) * fixed;
  else {
    const d = new Date(start);
    d.setHours(0, 0, 0, 0);
    t = d.getTime();
  }
  const map = new Map(buckets.map((b) => [b.timestamp, b]));
  const result = [];
  while (t <= now) {
    result.push(
      map.get(t) || { timestamp: t, requests: 0, tokens: 0, failed: 0 },
    );
    if (fixed) t += fixed;
    else {
      const d = new Date(t);
      d.setDate(d.getDate() + 1);
      t = d.getTime();
    }
  }
  return result;
}
function chart(buckets, mode = activity) {
  const label =
    { tokens_per_second: "Tokens/s", avg_ttft_ms: "TTFT" }[mode] || mode;
  const formatted = (value) =>
    mode === "cost"
      ? money(value)
      : mode === "avg_ttft_ms"
        ? duration(value)
        : number(value);
  if (!buckets.some((b) => (b[mode] ?? 0) > 0))
    return '<div class="empty">No data in this range</div>';
  const width = 900,
    height = 270,
    left = 42,
    right = 7,
    top = 16,
    bottom = 34,
    plotW = width - left - right,
    plotH = height - top - bottom;
  let max = Math.max(...buckets.map((b) => b[mode] ?? 0), 0.000001);
  const magnitude = 10 ** Math.floor(Math.log10(max));
  max = Math.ceil(max / magnitude) * magnitude;
  const step = plotW / buckets.length,
    bar = Math.min(60, step * 0.65);
  const labelEvery = Math.max(1, Math.ceil(buckets.length / 12));
  return `<svg viewBox="0 0 ${width} ${height}" preserveAspectRatio="none" role="img" aria-label="${h(label)} over time">${Array.from(
    { length: 5 },
    (_, i) => {
      const y = top + plotH - (i * plotH) / 4;
      return `<line x1="${left}" y1="${y}" x2="${width - right}" y2="${y}"/><text x="${left - 8}" y="${y + 4}" text-anchor="end">${mode === "cost" ? money((max * i) / 4) : mode === "avg_ttft_ms" ? duration((max * i) / 4) : short((max * i) / 4)}</text>`;
    },
  ).join("")}${buckets
    .map((b, i) => {
      const value = b[mode] ?? 0,
        bh = (value / max) * plotH,
        x = left + step * (i + 0.5) - bar / 2,
        y = top + plotH - bh;
      const bucketLabel = new Date(b.timestamp).toLocaleString(
        "en-US",
        range === "1h" || range === "24h"
          ? { hour: "2-digit", minute: "2-digit", hour12: false }
          : { month: "short", day: "numeric" },
      );
      const detail = `${localTime(b.timestamp)}\n${label}: ${b[mode] == null ? "Not reported" : formatted(value)}${mode === "requests" ? `\nFailed: ${number(b.failed)}` : ""}`;
      return `<rect x="${x}" y="${y}" width="${bar}" height="${bh}" fill="var(--accent)"/>${mode === "requests" && b.failed ? `<rect x="${x}" y="${y}" width="${bar}" height="${(b.failed / max) * plotH}" fill="var(--red)"/>` : ""}${chartHit(left + step * i, top, step, plotH, detail)}${i % labelEvery === 0 ? `<text x="${x + bar / 2}" y="${height - 12}" text-anchor="middle">${h(bucketLabel)}</text>` : ""}`;
    })
    .join("")}</svg>`;
}
function overview() {
  if (!data) return '<div class="loading">Loading statistics…</div>';
  const s = data.summary,
    buckets = fillBuckets(data.buckets),
    tokenTotal =
      s.input_tokens == null || s.output_tokens == null
        ? null
        : s.input_tokens + s.output_tokens;
  const mix = [
    ["Uncached input", s.uncached_tokens, "var(--blue)"],
    ["Cache read", s.cached_tokens, "var(--accent)"],
    ["Cache write", s.cache_write_tokens, "var(--yellow)"],
    ["Output", s.output_tokens, "var(--pink)"],
  ];
  const mixTotal = sum(mix.map((m) => m[1]));
  return `<h1>Overview</h1><p class="subtitle">Everything pp did across your sessions in the ${descriptions[range]}.</p><div class="panel metrics">${metric(
    "API-equivalent cost",
    money(s.cost),
    costNote(s),
    buckets.map((b) => b.cost ?? 0),
    "var(--pink)",
  )}${metric(
    "Requests",
    number(s.requests),
    `${number(s.failed)} failed`,
    buckets.map((b) => b.requests),
  )}${metric(
    "Conversation tokens",
    short(tokenTotal),
    `${short(s.output_tokens)} output`,
    buckets.map((b) => b.tokens ?? 0),
  )}${metric("Cache rate", cacheRate(s), "— saved")}${metric(
    "Error rate",
    s.requests ? percent(s.failed, s.requests) : "0.0%",
    `${number(s.ok)} succeeded`,
    buckets.map((b) => (b.requests ? b.failed / b.requests : 0)),
    "var(--red)",
  )}</div><div class="panel metrics small-metrics">${[
    ["Uncached input", short(s.uncached_tokens)],
    ["Cache read", short(s.cached_tokens)],
    ["Cache write", short(s.cache_write_tokens)],
    ["Output", short(s.output_tokens)],
    ["Premium requests", "—"],
    [
      "Tokens/s",
      s.tokens_per_second == null ? "—" : s.tokens_per_second.toFixed(1),
    ],
    ["Avg latency", duration(s.avg_duration_ms)],
    ["Avg TTFT", duration(s.avg_ttft_ms)],
  ]
    .map(([label, value]) => metric(label, value))
    .join(
      "",
    )}</div><div class="graphs"><section class="panel graph"><div class="panel-heading"><div><h2>Activity</h2><p class="panel-description">${range === "1h" ? "Per 5 minutes" : range === "24h" ? "Per hour" : "Per day"}</p></div><div class="segmented" role="radiogroup" aria-label="Activity metric">${["requests", "tokens", "cost"].map((v) => `<button role="radio" aria-checked="${activity === v}" data-activity="${v}">${v[0].toUpperCase() + v.slice(1)}</button>`).join("")}</div></div><div class="chart">${chart(buckets)}</div></section><section class="panel graph"><h2>Token mix</h2><p class="panel-description">Where conversation tokens went</p><div class="token-bar">${mix.map((m) => `<span style="width:${mixTotal ? (100 * (m[1] ?? 0)) / mixTotal : 0}%;background:${m[2]}" title="${h(m[0])}: ${number(m[1])}"></span>`).join("")}</div><div class="legend">${mix.map((m) => `<span><i class="dot" style="background:${m[2]}"></i>${m[0]} ${m[1] == null ? "—" : mixTotal ? `${Math.round((100 * m[1]) / mixTotal)}%` : "—"}</span>`).join("")}</div>${s.usage_known < s.requests || s.cache_known < s.requests || s.cache_write_known < s.requests ? '<div class="unavailable">Some requests did not report token or cache usage.</div>' : ""}<div class="agent-label">By agent</div><div class="token-bar"><span style="width:${s.requests ? 100 : 0}%;background:var(--accent)"></span></div><div class="agent-row"><i class="dot" style="background:var(--accent)"></i>Main agent <span class="muted num">${number(s.requests)} req</span><span class="num">${short(tokenTotal)}</span><span class="percentage">${s.requests ? "100%" : "—"}</span></div><div class="agent-row"><i class="dot" style="background:var(--pink)"></i>Subagents <span class="muted num">0 req</span><span class="num">0</span><span class="percentage">${s.requests ? "0%" : "—"}</span></div></section></div><section class="panel"><div class="requests-heading panel-heading"><div><h2><i class="live-dot"></i> &nbsp;Latest requests</h2><p class="panel-description">Most recent model calls across every session</p></div><a class="all-requests" href="#/requests?range=${range}">All requests ${icon("arrow")}</a></div>${overviewTable(data.latest)}</section>${pricingNotice()}`;
}
const money = (n) =>
  n == null
    ? "—"
    : n === 0
      ? "$0"
      : `$${n.toLocaleString("en-US", { minimumFractionDigits: n < 0.01 ? 4 : 2, maximumFractionDigits: n < 0.01 ? 6 : 2 })}`;
const colors = [
  "var(--accent)",
  "var(--pink)",
  "var(--blue)",
  "var(--yellow)",
  "var(--green)",
  "var(--red)",
  "var(--muted)",
];
const PROVIDER_TOP = 6,
  PROVIDER_LIMIT = 12,
  PROVIDER_MIX = [
    ["uncached_tokens", "Uncached input", "#5b8cff"],
    ["cached_tokens", "Cache read", "var(--chart-primary)"],
    ["cache_write_tokens", "Cache write", "#f5b54a"],
    ["output_tokens", "Output", "var(--chart-secondary)"],
  ];
const MODEL_SHARE_LIMIT = 6,
  MODEL_LIMIT = 25;
let expandedGroups = new Set(),
  hiddenSeries = new Set(),
  shareMode = "share",
  costMode = "model",
  providerMetric = "tokens",
  providerPeak = "",
  providerSort = { key: "tokens", dir: "desc" },
  providerSelected = null,
  providerShown = PROVIDER_LIMIT,
  modelSort = { key: "requests", dir: "desc" },
  modelShown = MODEL_LIMIT,
  modelSelected = null,
  modelDetailHidden = new Set();
const averageSpeed = (n) => (n == null ? "—" : n.toFixed(1));
const cacheRate = (g) =>
  percent(
    g.cached_tokens,
    g.input_tokens,
  );
const costNote = (g) =>
  `${number(g.priced)} priced · ${number(g.unpriced)} unpriced`;
const costPerRequest = (g) => (g.priced ? money(g.cost / g.priced) : "—");
const groupKey = (kind, key) => encodeURIComponent(`${kind}:${key}`);
const componentNames = [
  ["input", "Input", colors[2]],
  ["cache_read", "Cache read", colors[0]],
  ["cache_write", "Cache write", colors[3]],
  ["output", "Output", colors[1]],
];
function pricingNotice() {
  return `<div class="pricing-note" role="note">Costs and rates are computed and stored with each request at record time, using the model configuration in effect then. Unpriced requests are excluded; missing prices or usage are never treated as zero or repriced at current rates. Cache writes require reported usage and a configured cacheWrite rate when nonzero. Subscription quotas are not reported by pp.</div>`;
}
function segmentControls(key, options, selected, label) {
  return `<div class="segmented" role="radiogroup" aria-label="${h(label)}">${options.map(([id, name]) => `<button role="radio" aria-checked="${id === selected}" data-${key}="${id}">${h(name)}</button>`).join("")}</div>`;
}
function chartPanel(title, description, content, controls = "") {
  return `<section class="panel graph analytics-panel"><div class="panel-heading"><div><h2>${h(title)}</h2><p class="panel-description">${h(description)}</p></div>${controls}</div>${content}</section>`;
}
function tokenMix(g) {
  const parts = [
    ["Uncached input", g.uncached_tokens, colors[2]],
    ["Cache read", g.cached_tokens, colors[0]],
    ["Cache write", g.cache_write_tokens, colors[3]],
    ["Output", g.output_tokens, colors[1]],
  ];
  const known = sum(parts.map((p) => p[1]));
  return `<div class="token-bar">${parts.map(([name, value, color]) => `<span style="width:${known ? (100 * (value ?? 0)) / known : 0}%;background:${color}" title="${h(name)}: ${number(value)}"></span>`).join("")}</div><div class="legend">${parts.map(([name, value, color]) => `<span><i class="dot" style="background:${color}"></i>${h(name)} ${short(value)} · ${percent(value, known)}</span>`).join("")}</div><p class="panel-description">${g.usage_known < g.requests || g.cache_known < g.requests || g.cache_write_known < g.requests ? "Some token or cache usage is unknown. " : ""}</p>`;
}
function splitBar(g) {
  return `<div class="cost-split" title="Input · cache read · cache write · output">${componentNames.map(([key, name, color]) => `<span style="width:${g.cost ? (100 * (g.cost_components?.[key] ?? 0)) / g.cost : 0}%;background:${color}" title="${name}: ${money(g.cost_components?.[key])}"></span>`).join("")}</div>`;
}
function breakdown(g) {
  return `<div class="component-list">${componentNames.map(([key, name, color]) => `<div><span><i class="dot" style="background:${color}"></i>${name}</span><b class="num">${money(g.cost_components?.[key])}</b><span class="num muted">${percent(g.cost_components?.[key], g.cost)}</span></div>`).join("")}<div><span>Total</span><b class="num">${money(g.cost)}</b><span></span></div></div>${splitBar(g)}<p class="panel-description">${costNote(g)}</p>`;
}
function stackedChart(groups, metric, share = false, daily = false) {
  if (!groups.length) return '<div class="empty">No data in this range</div>';
  const visibleGroups = groups.filter((g) => !hiddenSeries.has(g.key));
  if (!visibleGroups.length)
    return '<div class="empty">Select a legend item to show its series.</div>';
  const timestamps = [
    ...new Set(
      groups.flatMap((g) =>
        (daily ? g.daily : g.buckets).map((b) => b.timestamp),
      ),
    ),
  ].sort((a, b) => a - b);
  const timeline = [];
  if (daily && timestamps.length) {
    const start =
      range === "all"
        ? timestamps[0]
        : Math.floor(
            (data.updated_at -
              (range === "1h"
                ? 3600000
                : range === "24h"
                  ? 86400000
                  : parseInt(range) * 86400000)) /
              86400000,
          ) * 86400000;
    for (
      let timestamp = start;
      timestamp <= data.updated_at;
      timestamp += 86400000
    )
      timeline.push({ timestamp });
  } else if (!daily)
    timeline.push(
      ...fillBuckets(timestamps.map((timestamp) => ({ timestamp }))),
    );
  const series = visibleGroups.map(
    (g) =>
      new Map(
        (daily ? g.daily : g.buckets).map((b) => [b.timestamp, b[metric]]),
      ),
  );
  const totals = timeline.map((b) =>
    series.reduce((s, m) => s + (m.get(b.timestamp) ?? 0), 0),
  );
  if (!totals.some((v) => v > 0))
    return `<div class="empty">${metric === "cost" && !data.summary.priced && data.summary.requests ? "No priced usage in this range" : metric === "cost" && data.summary.priced ? "No estimated cost for selected series" : "No data in this range"}</div>`;
  const width = 900,
    height = 265,
    left = 48,
    top = 16,
    plotH = 210,
    plotW = 840,
    step = plotW / timeline.length,
    bar = Math.min(60, step * 0.72);
  const max = share ? 100 : Math.max(...totals) * 1.05;
  const format = (v) =>
    share ? `${Math.round(v)}%` : metric === "cost" ? money(v) : short(v);
  let svg = `<svg viewBox="0 0 ${width} ${height}" preserveAspectRatio="none" role="img" aria-label="${h(metric)} by ${daily ? "UTC day" : "time bucket"}">`;
  for (let i = 0; i < 5; i++) {
    const y = top + plotH * (1 - i / 4);
    svg += `<line x1="${left}" y1="${y}" x2="890" y2="${y}"/><text x="${left - 7}" y="${y + 4}" text-anchor="end">${h(format((max * i) / 4))}</text>`;
  }
  timeline.forEach((b, i) => {
    let offset = 0;
    visibleGroups.forEach((g, j) => {
      const raw = series[j].get(b.timestamp) ?? 0,
        value = share ? (totals[i] ? (100 * raw) / totals[i] : 0) : raw,
        bh = (value / max) * plotH;
      const color = g.color || colors[groups.indexOf(g) % colors.length];
      svg += `<rect x="${left + step * (i + 0.5) - bar / 2}" y="${top + plotH - offset - bh}" width="${bar}" height="${bh}" fill="${color}"><title>${h(g.label)} · ${daily ? new Date(b.timestamp).toISOString().slice(0, 10) : h(localTime(b.timestamp))}: ${h(metric === "cost" ? money(raw) : number(raw))}${share ? ` · ${format(value)}` : ""}</title></rect>`;
      offset += bh;
    });
    const bucketTime = daily
      ? `${new Date(b.timestamp).toISOString().slice(0, 10)} UTC`
      : localTime(b.timestamp);
    const details = visibleGroups.map((g, j) => {
      const raw = series[j].has(b.timestamp) ? series[j].get(b.timestamp) : 0;
      return `${g.label}: ${raw == null ? "Not reported" : metric === "cost" ? money(raw) : number(raw)}${share && raw != null ? ` · ${totals[i] ? percent(raw, totals[i]) : "0%"}` : ""}`;
    });
    const partial = series.some((s) => s.has(b.timestamp) && s.get(b.timestamp) == null);
    svg += chartHit(left + step * i, top, step, plotH,
      [bucketTime, ...details, `Total${partial ? " (known)" : ""}: ${metric === "cost" ? money(totals[i]) : number(totals[i])}`].join("\n"));
    if (i % Math.max(1, Math.ceil(timeline.length / 10)) === 0) {
      const label = new Date(b.timestamp).toLocaleString(
        "en-US",
        daily
          ? { month: "short", day: "numeric", timeZone: "UTC" }
          : range === "1h" || range === "24h"
            ? { hour: "2-digit", minute: "2-digit", hour12: false }
            : { month: "short", day: "numeric" },
      );
      svg += `<text x="${left + step * (i + 0.5)}" y="${height - 10}" text-anchor="middle">${h(label)}</text>`;
    }
  });
  return `<div class="chart">${svg}</svg></div>`;
}
function seriesLegend(groups, metric, share = false) {
  const denominator = sum(groups.map((g) => g[metric]));
  return `<div class="legend series-legend">${groups.map((g, i) => `<button data-series="${h(g.key)}" aria-pressed="${!hiddenSeries.has(g.key)}"><i class="dot" style="background:${g.color || colors[i % colors.length]}"></i>${h(g.label)} <span class="num">${share ? percent(g[metric], denominator) : metric === "cost" ? money(g[metric]) : short(g[metric])}</span></button>`).join("")}</div>`;
}
function topSeries(groups, label) {
  const result = groups.slice(0, 6).map((g) => ({ ...g, label: label(g) }));
  if (groups.length > 6) {
    const rest = groups.slice(6),
      buckets = new Map(),
      daily = new Map();
    for (const g of rest)
      for (const [name, map] of [
        ["buckets", buckets],
        ["daily", daily],
      ])
        for (const b of g[name]) {
          const v = map.get(b.timestamp) || {
            timestamp: b.timestamp,
            requests: 0,
            tokens: 0,
            cost: 0,
          };
          for (const field of ["requests", "tokens", "cost"])
            v[field] += b[field] ?? 0;
          map.set(b.timestamp, v);
        }
    result.push({
      key: "other",
      label: `Other (${rest.length})`,
      requests: sum(rest.map((g) => g.requests)),
      tokens: sum(rest.map((g) => g.tokens)),
      cost: sum(rest.map((g) => g.cost)),
      buckets: [...buckets.values()],
      daily: [...daily.values()],
    });
  }
  return result;
}
// Models 页复刻 omp stats 的 ModelsRoute：指标、请求占比图、可展开的模型表与吞吐／延迟详情。
const modelKeyOf = (group) => `${group.model}::${group.provider}`;
const modelCacheRate = (group) =>
  (group.input_tokens ?? 0) > 0 ? (group.cached_tokens ?? 0) / group.input_tokens : 0;
const modelErrorRate = (group) =>
  (group.requests ?? 0) > 0 ? (group.failed ?? 0) / group.requests : 0;
const modelSpeed = (value) => (value == null ? "-" : value.toFixed(1));
const modelDuration = (ms) =>
  ms == null ? "-" : ms < 1000 ? `${(ms / 1000).toFixed(2)}s` : `${(ms / 1000).toFixed(1)}s`;
/** 缓存节省需要能对账的写入用量与一致费率；任一缺失时只能显示未知。 */
function modelCacheSavings(group) {
  const rates = group.rates,
    components = group.cost_components;
  if (!rates || !(rates.input > 0) || !components) return null;
  if (!(group.requests > 0) || group.priced !== group.requests) return null;
  if (group.cache_write_known !== group.requests) return null;
  if (!(group.input_tokens > 0)) return null;
  const counterfactual = (group.input_tokens * rates.input) / 1e6;
  if (!(counterfactual > 0)) return null;
  const actual =
    (components.input ?? 0) + (components.cache_read ?? 0) + (components.cache_write ?? 0);
  return 1 - actual / counterfactual;
}
function resetModels() {
  modelSort = { key: "requests", dir: "desc" };
  modelShown = MODEL_LIMIT;
  modelSelected = null;
  modelDetailHidden = new Set();
}
/** 颜色按总请求数降序固定分配，排序与筛选不改变色带。 */
function modelColors(groups) {
  return new Map(
    [...groups]
      .sort((a, b) => (b.requests ?? 0) - (a.requests ?? 0) || modelKeyOf(a).localeCompare(modelKeyOf(b)))
      .map((group, index) => [
        modelKeyOf(group),
        TOOL_SERIES_COLORS[index % TOOL_SERIES_COLORS.length],
      ]),
  );
}
/** 同一模型被多个 provider 提供时才附加 provider，避免图例重复。 */
function modelLabels(groups) {
  const providersPerModel = new Map();
  for (const group of groups)
    providersPerModel.set(group.model, (providersPerModel.get(group.model) ?? 0) + 1);
  return new Map(
    groups.map((group) => [
      modelKeyOf(group),
      (providersPerModel.get(group.model) ?? 0) > 1
        ? `${group.model} · ${group.provider}`
        : group.model,
    ]),
  );
}
/** 前 N 个模型单列，其余折叠为 Other；阈值与图例顺序一致。 */
function modelPivot(points, buckets, colors, labels, limit) {
  const index = new Map(buckets.map((timestamp, i) => [timestamp, i]));
  const byKey = new Map();
  for (const point of points) {
    const i = index.get(point.timestamp);
    if (i === undefined) continue;
    let values = byKey.get(point.key);
    if (!values) {
      values = Array.from({ length: buckets.length }, () => 0);
      byKey.set(point.key, values);
    }
    values[i] += point.value;
  }
  const ranked = [...byKey.entries()]
    .map(([key, values]) => ({
      key,
      values,
      total: values.reduce((sum, value) => sum + value, 0),
    }))
    .filter((entry) => entry.total !== 0)
    .sort((a, b) => b.total - a.total || a.key.localeCompare(b.key));
  const head = ranked.slice(0, limit),
    tail = ranked.slice(limit);
  const series = head.map((entry) => ({
    key: entry.key,
    label: labels.get(entry.key) ?? entry.key,
    color: colors.get(entry.key) ?? TOOL_OTHER_COLOR,
    values: entry.values,
  }));
  if (tail.length) {
    const other = Array.from({ length: buckets.length }, () => 0);
    for (const entry of tail) for (let i = 0; i < other.length; i++) other[i] += entry.values[i];
    series.push({
      key: "__other__",
      label: `Other (${tail.length})`,
      color: TOOL_OTHER_COLOR,
      values: other,
    });
  }
  return series;
}
function modelView() {
  const groups = [...data.models].sort(
    (a, b) => (b.requests ?? 0) - (a.requests ?? 0) || modelKeyOf(a).localeCompare(modelKeyOf(b)),
  );
  const colors = modelColors(groups),
    labels = modelLabels(groups);
  const buckets = toolBucketAxis(
    groups.flatMap((group) => (group.buckets || []).map((bucket) => bucket.timestamp)),
  );
  const points = groups.flatMap((group) =>
    (group.buckets || []).map((bucket) => ({
      timestamp: bucket.timestamp,
      key: modelKeyOf(group),
      value: bucket.requests ?? 0,
    })),
  );
  const requestTotals = toolDensify(points, buckets);
  // 零值转 null，tooltip 只列出该时间桶内实际活跃的模型。
  const countSeries = modelPivot(points, buckets, colors, labels, MODEL_SHARE_LIMIT).map(
    (series) => ({ ...series, values: series.values.map((value) => value || null) }),
  );
  const shareSeries = countSeries.map((series) => ({
    ...series,
    values: series.values.map((value, index) =>
      value === null || !(requestTotals[index] > 0) ? null : value / requestTotals[index],
    ),
  }));
  const trends = new Map(
    groups.map((group) => [
      modelKeyOf(group),
      toolDensify(
        (group.buckets || []).map((bucket) => ({
          timestamp: bucket.timestamp,
          value: bucket.requests ?? 0,
        })),
        buckets,
      ),
    ]),
  );
  let top,
    totalRequests = 0,
    failedRequests = 0,
    totalCost = 0,
    unpricedRequests = 0;
  for (const group of groups) {
    if (!top || (group.requests ?? 0) > (top.requests ?? 0)) top = group;
    totalRequests += group.requests ?? 0;
    failedRequests += group.failed ?? 0;
    totalCost += group.cost ?? 0;
    unpricedRequests += group.unpriced ?? 0;
  }
  return {
    groups,
    colors,
    labels,
    buckets,
    requestTotals,
    countSeries,
    shareSeries,
    trends,
    top,
    totalRequests,
    failedRequests,
    totalCost,
    unpricedRequests,
    maxRequests: top?.requests ?? 0,
    providerCount: new Set(groups.map((group) => group.provider)).size,
  };
}
function modelKeyValues(items) {
  return `<dl class="x-kv" style="margin:0">${items
    .map(
      ([key, label, value]) =>
        `<div class="x-kv-item" data-key="${h(key)}"><dt class="x-kv-key">${h(label)}</dt><dd class="x-kv-value" style="margin:0">${value}</dd></div>`,
    )
    .join("")}</dl>`;
}
function modelErrorsCell(group) {
  if (!(group.failed > 0)) return '<span class="x-num x-dim">0%</span>';
  const rate = modelErrorRate(group);
  return `<span title="${h(`${toolInteger(group.failed)} failed`)}"><span class="x-badge" data-tone="${toolErrorTone(rate)}" data-mono="true">${toolErrorRate(rate)}</span></span>`;
}
function modelColumns(view) {
  const bucketWord = toolBucketWord(toolRangeMeta().bucketMs);
  return [
    {
      key: "expand",
      header: "",
      width: 28,
      render: (group) =>
        `<span class="x-models-chevron" data-open="${modelKeyOf(group) === modelSelected}">${icon("chevron", 14)}</span>`,
    },
    {
      key: "model",
      header: "Model",
      sort: (group) => group.model,
      render: (group) => {
        const key = modelKeyOf(group);
        return `<div class="x-row" style="gap:10px"><span class="x-swatch" style="background:${view.colors.get(key) || TOOL_OTHER_COLOR}"></span><div class="x-stack" style="gap:0"><span class="x-cell-primary x-truncate"><span class="x-mono">${h(group.model)}</span></span><span class="x-cell-secondary x-truncate">${h(group.provider)}</span></div></div>`;
      },
    },
    {
      key: "requests",
      header: "Requests",
      align: "right",
      sort: (group) => group.requests ?? 0,
      render: (group) =>
        toolMeter(
          group.requests ?? 0,
          view.maxRequests,
          toolInteger(group.requests),
          view.colors.get(modelKeyOf(group)) || TOOL_OTHER_COLOR,
        ),
    },
    {
      key: "cost",
      header: "Cost",
      title: "API-equivalent estimate at public rates",
      align: "right",
      sort: (group) => group.cost ?? -1,
      render: (group) =>
        `<span class="x-num">${toolEstimatedCost(group.cost ?? 0, group.unpriced ?? 0)}</span>`,
    },
    {
      key: "tokens",
      header: "Tokens",
      title: "Uncached input + cache reads + cache writes + output",
      align: "right",
      sort: (group) => group.tokens ?? 0,
      render: (group) =>
        `<span class="x-num" title="${h(toolInteger(group.tokens))}">${toolCompact(group.tokens)}</span>`,
    },
    {
      key: "cache",
      header: "Cache rate",
      title: "Cache reads ÷ (uncached input + cache reads)",
      align: "right",
      sort: (group) => modelCacheRate(group),
      render: (group) => `<span class="x-num">${toolPercent(modelCacheRate(group))}</span>`,
    },
    {
      key: "errors",
      header: "Errors",
      align: "right",
      sort: (group) => modelErrorRate(group),
      render: (group) => modelErrorsCell(group),
    },
    {
      key: "tps",
      header: "Tokens/s",
      title: "Average output tokens per second",
      align: "right",
      sort: (group) => group.tokens_per_second ?? -1,
      render: (group) => `<span class="x-num">${modelSpeed(group.tokens_per_second)}</span>`,
    },
    {
      key: "ttft",
      header: "TTFT",
      title: "Average time to first token",
      align: "right",
      sort: (group) => group.avg_ttft_ms ?? -1,
      render: (group) => `<span class="x-num">${modelDuration(group.avg_ttft_ms)}</span>`,
    },
    {
      key: "trend",
      header: "Trend",
      title: `Requests per ${bucketWord}`,
      width: 112,
      render: (group) => {
        const key = modelKeyOf(group),
          values = view.trends.get(key);
        return values
          ? `<span style="display:inline-block;width:96px;height:22px">${toolSparkline(values, 96, 22, view.colors.get(key) || TOOL_OTHER_COLOR)}</span>`
          : '<span class="x-dim">–</span>';
      },
    },
  ];
}
function modelSortHeader(column) {
  const sorted = modelSort.key === column.key ? modelSort.dir : undefined;
  const arrow = sorted === "asc" ? TOOL_ARROW_UP : sorted === "desc" ? TOOL_ARROW_DOWN : "";
  return `<th data-align="${column.align || "left"}" data-sortable="${Boolean(column.sort)}"${sorted ? ` data-sorted="${sorted}" aria-sort="${sorted === "asc" ? "ascending" : "descending"}"` : ""}${column.title ? ` title="${h(column.title)}"` : ""}${column.width !== undefined ? ` style="width:${column.width}px"` : ""}${column.sort ? ` data-model-sort="${column.key}" data-model-align="${column.align || "left"}" data-models-focus="sort-${column.key}"` : ""}>${h(column.header)}${arrow ? `<span class="x-sort-arrow">${arrow}</span>` : ""}</th>`;
}
function modelSorted(rows, columns) {
  const column = columns.find((item) => item.key === modelSort.key && item.sort);
  if (!column) return rows;
  const direction = modelSort.dir === "asc" ? 1 : -1;
  return [...rows].sort((a, b) => {
    const left = column.sort(a),
      right = column.sort(b);
    const delta =
      typeof left === "number" && typeof right === "number"
        ? left - right
        : String(left).localeCompare(String(right));
    return delta * direction;
  });
}
function modelTableMore(total, shown) {
  if (total <= MODEL_LIMIT) return "";
  return `<div class="x-table-more"><span class="x-micro">${toolInteger(shown)} of ${toolInteger(total)}</span>${total > shown ? `<button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-model-page="more" data-models-focus="more">Show more</button>` : ""}${shown > MODEL_LIMIT ? `<button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-model-page="less" data-models-focus="less">Show fewer</button>` : ""}</div>`;
}
function modelDetail(view, group) {
  const key = modelKeyOf(group),
    color = view.colors.get(key) || "var(--chart-primary)";
  const points = (group.buckets || []).map((bucket) => ({
    timestamp: bucket.timestamp,
    tps: bucket.tokens_per_second ?? null,
    ttft: bucket.avg_ttft_ms == null ? null : bucket.avg_ttft_ms / 1000,
    requests: bucket.requests ?? 0,
  }));
  const line = points.length > 1 ? "line" : "bars";
  const series = [
    {
      key: "tps",
      label: "Tokens/s",
      color,
      kind: line,
      values: points.map((point) => point.tps),
    },
    {
      key: "ttft",
      label: "TTFT",
      color: TOOL_OTHER_COLOR,
      kind: line,
      axis: "right",
      dashed: true,
      values: points.map((point) => point.ttft),
    },
  ];
  const errorRate = modelErrorRate(group),
    savings = modelCacheSavings(group);
  const chart = points.length
    ? toolChart(
        { buckets: points.map((point) => point.timestamp), bucketMs: toolRangeMeta().bucketMs },
        series,
        "No data in this range",
        {
          height: 240,
          stacked: false,
          hidden: modelDetailHidden,
          showTotal: false,
          format: toolCompact,
          formatTooltip: (value) => `${modelSpeed(value)} tok/s`,
          formatRight: (value) => `${Number(value.toFixed(2))}s`,
          extraRows: (index) =>
            `<div class="x-chart-tooltip-row x-chart-tooltip-total"><span class="x-chart-tooltip-label">Requests</span><span class="x-chart-tooltip-value">${toolInteger(points[index].requests)}</span></div>`,
        },
      )
    : toolEmpty("No performance samples", "Timing is recorded for streamed responses.");
  return `<div class="x-models-detail x-rise"><div class="x-models-detail-facts"><div><div class="x-section-label">Efficiency</div>${modelKeyValues([
    [
      "error",
      "Error rate",
      `<span class="x-tone-${toolErrorTone(errorRate)}">${toolErrorRate(errorRate)} <span class="x-dim">(${toolInteger(group.failed ?? 0)} failed)</span></span>`,
    ],
    ["cache", "Cache rate", toolPercent(modelCacheRate(group))],
    [
      "savings",
      "Cache savings",
      savings == null
        ? '<span class="x-dim" title="Not recorded: cache savings need reported cache-write usage, consistent recorded rates and priced requests">—</span>'
        : `<span${savings < 0 ? ' style="color:var(--bad)"' : ""}>${toolPercent(savings)}</span>`,
    ],
    [
      "premium",
      "Premium requests",
      '<span class="x-dim" title="Premium request units are not recorded by pp">—</span>',
    ],
  ])}</div><div><div class="x-section-label">Latency</div>${modelKeyValues([
    ["duration", "Avg duration", modelDuration(group.avg_duration_ms)],
    ["ttft", "Avg TTFT", modelDuration(group.avg_ttft_ms)],
    ["speed", "Tokens/s", modelSpeed(group.tokens_per_second)],
  ])}</div><div><div class="x-section-label">Tokens</div>${modelKeyValues([
    ["uncached", "Uncached input", toolCompact(group.uncached_tokens)],
    ["cache-read", "Cache read", toolCompact(group.cached_tokens)],
    ["cache-write", "Cache write", toolCompact(group.cache_write_tokens)],
    ["output", "Output", toolCompact(group.output_tokens)],
  ])}</div><div class="x-micro x-dim">First seen ${h(toolRelative(group.first_seen))} · last seen ${h(toolRelative(group.last_seen))}</div></div><div class="x-models-detail-chart"><div class="x-row" style="justify-content:space-between"><div class="x-section-label" style="margin-bottom:0">Performance per active ${h(toolBucketWord(toolRangeMeta().bucketMs))}</div>${toolLegend(series, null, { hidden: modelDetailHidden, attribute: "data-model-detail-series", values: false, focusAttribute: "data-models-focus" })}</div>${chart}</div></div>`;
}
function modelTable(view) {
  const columns = modelColumns(view),
    rows = modelSorted(view.groups, columns),
    visible = rows.slice(0, modelShown);
  const head = columns.map(modelSortHeader).join("");
  const body = visible
    .map((group) => {
      const key = modelKeyOf(group),
        encoded = encodeURIComponent(key),
        open = key === modelSelected;
      const cells = columns
        .map(
          (column) =>
            `<td data-align="${column.align || "left"}" data-wrap="false">${column.render(group)}</td>`,
        )
        .join("");
      return `<tr data-clickable="true" data-selected="${open}" data-model-row="${h(encoded)}" data-models-focus="row-${h(encoded)}" tabindex="0">${cells}</tr>${
        open
          ? `<tr><td colspan="${columns.length}" data-wrap="true" style="padding:0">${modelDetail(view, group)}</td></tr>`
          : ""
      }`;
    })
    .join("");
  return `<div class="x-table-wrap"><table class="x-table" data-dense="false"><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>${modelTableMore(rows.length, visible.length)}`;
}
function modelsEmpty() {
  return toolEmpty("No model usage in this range", "Pick a wider range in the top bar.");
}
function modelsClick(e) {
  const share = e.target.closest("[data-share]");
  if (share) {
    shareMode = share.dataset.share;
    render();
    return true;
  }
  const header = e.target.closest("[data-model-sort]");
  if (header) {
    const key = header.dataset.modelSort;
    modelSort = {
      key,
      dir:
        modelSort.key === key
          ? modelSort.dir === "asc"
            ? "desc"
            : "asc"
          : header.dataset.modelAlign === "right"
            ? "desc"
            : "asc",
    };
    render();
    return true;
  }
  const series = e.target.closest("[data-model-detail-series]");
  if (series) {
    const key = series.dataset.modelDetailSeries;
    if (modelDetailHidden.has(key)) modelDetailHidden.delete(key);
    else modelDetailHidden.add(key);
    render();
    return true;
  }
  const row = e.target.closest("[data-model-row]");
  if (row) {
    const key = decodeURIComponent(row.dataset.modelRow);
    modelSelected = modelSelected === key ? null : key;
    modelDetailHidden = new Set();
    render();
    return true;
  }
  const pageButton = e.target.closest("[data-model-page]");
  if (pageButton) {
    modelShown =
      pageButton.dataset.modelPage === "more" ? modelShown + MODEL_LIMIT * 4 : MODEL_LIMIT;
    render();
    return true;
  }
  return false;
}
function groupRow(g, kind, cells, detail, columns) {
  const key = groupKey(kind, g.key),
    open = expandedGroups.has(key);
  const name = g.label ?? (kind === "provider" ? g.provider : g.model);
  const description = g.description ?? (kind !== "provider" ? `${g.provider} · ${g.key}` : "");
  return `<tr class="group-row${open ? " expanded" : ""}" tabindex="0" data-expand="${h(key)}" aria-expanded="${open}" aria-controls="details-${h(key)}" aria-label="${open ? "Collapse" : "Expand"} ${h(name)}"><td><div class="model-name">${icon(open ? "down" : "chevron", 12)} ${h(name)}</div>${description ? `<div class="provider insight-path" title="${h(description)}">${h(description)}</div>` : ""}</td>${cells}</tr><tr class="expanded-detail" id="details-${h(key)}"${open ? "" : " hidden"}><td colspan="${columns}">${open ? detail(key) : ""}</td></tr>`;
}
function analyticsTable(headers, rows) {
  return `<div class="table-wrap"><table class="analytics-table"><thead><tr>${headers.map((name, i) => `<th${i ? ' class="right"' : ""}>${h(name)}</th>`).join("")}</tr></thead><tbody>${rows || `<tr><td colspan="${headers.length}"><div class="empty">No requests in this range</div></td></tr>`}</tbody></table></div>`;
}
const numericCells = (values) =>
  values.map((v) => `<td class="right num">${v}</td>`).join("");
function modelsPage() {
  if (!data) return '<div class="loading">Loading statistics…</div>';
  toolChartModels = new Map();
  toolChartSeq = 0;
  const view = modelView(),
    bucketWord = toolBucketWord(toolRangeMeta().bucketMs),
    empty = modelsEmpty();
  const share = shareMode === "share",
    totals = Math.max(1, view.totalRequests);
  const statGrid = `<div class="x-stat-grid x-rise" style="--stat-min:200px">${toolStat(
    "Models used",
    toolInteger(view.groups.length),
    `across ${toolInteger(view.providerCount)} provider${view.providerCount === 1 ? "" : "s"}`,
  )}${toolStat(
    "Most used",
    view.top ? view.top.model : "–",
    view.top
      ? `${toolPercent(view.top.requests / totals)} of requests · ${view.top.provider}`
      : undefined,
    { title: view.top ? `${view.top.model} (${view.top.provider})` : undefined },
  )}${toolStat("Requests", toolInteger(view.totalRequests), `${toolInteger(view.failedRequests)} failed`, {
    spark: view.requestTotals,
  })}${toolStat(
    "API-equivalent cost",
    toolEstimatedCost(view.totalCost, view.unpricedRequests),
    view.unpricedRequests > 0 ? `${toolInteger(view.unpricedRequests)} unpriced` : undefined,
    { title: "What this usage would cost at public API rates" },
  )}</div>`;
  const actions = `<div class="x-segmented" data-size="sm" role="radiogroup" aria-label="Share chart mode"><span class="x-segmented-thumb"></span>${[
    ["share", "Share"],
    ["requests", "Requests"],
  ]
    .map(
      ([value, label]) =>
        `<button type="button" class="x-segmented-option" role="radio" aria-checked="${shareMode === value}" data-active="${shareMode === value}" data-share="${value}" data-models-focus="share-${value}">${label}</button>`,
    )
    .join("")}</div>`;
  const chart = toolChart(view, share ? view.shareSeries : view.countSeries, "No model activity in this range", {
    height: 260,
    yMax: share ? 1 : undefined,
    format: share ? (value) => toolPercent(value, 0) : toolCompact,
    formatTooltip: share ? (value) => toolPercent(value) : toolInteger,
    showTotal: !share,
  });
  const shareBody = `<div class="x-stack" style="gap:12px">${chart}${toolLegend(
    view.countSeries,
    (value) => toolPercent(value / totals),
    { focusAttribute: "data-models-focus" },
  )}</div>`;
  return `<div class="x-page"><header class="x-page-header x-rise"><div><h1 class="x-page-title">Models</h1><p class="x-page-description">Which models did the work in ${h(rangeWindows[range])}, and how fast they answered.</p></div></header>${view.groups.length ? statGrid : empty}${toolCard(
    1,
    "Request share",
    share
      ? `Each model's share of requests per ${bucketWord}`
      : `Requests per ${bucketWord}, stacked by model`,
    actions,
    view.groups.length ? shareBody : empty,
  )}${toolCard(2, "All models", "Click a row for latency and throughput over time", "", view.groups.length ? modelTable(view) : empty, true)}</div>`;
}
// Providers 页复刻 omp stats 的 ProvidersRoute：指标、按 provider 的总表、堆叠燃烧图与本地小时峰值。
// 图表/表格原语（toolCard、toolChart 等）与 Tools 页共用，定义在 insights.js。
const providerPad = (value) => String(value).padStart(2, "0");
const providerBucketMs = () =>
  range === "1h" ? 300000 : range === "24h" ? 3600000 : 86400000;
const providerBucketUnit = () =>
  range === "1h" ? "5 minutes" : range === "24h" ? "hour" : "day";
/** 颜色按总 tokens 降序固定分配，排序与筛选不改变色带。 */
function providerColors(groups) {
  return new Map(
    [...groups]
      .sort((a, b) => (b.tokens ?? 0) - (a.tokens ?? 0) || a.provider.localeCompare(b.provider))
      .map((group, index) => [
        group.provider,
        TOOL_SERIES_COLORS[index % TOOL_SERIES_COLORS.length],
      ]),
  );
}
function providerSpark(field) {
  return fillBuckets(data.buckets).map((bucket) => bucket[field] ?? 0);
}
function providerStatGrid(groups) {
  const s = data.summary,
    unpriced = s.unpriced ?? 0,
    top = groups.reduce(
      (best, group) => (best === null || (group.tokens ?? 0) > (best.tokens ?? 0) ? group : best),
      null,
    );
  return `<div class="x-stat-grid x-rise" style="--stat-min:190px">${toolStat(
    "Providers",
    number(groups.length),
    top ? `Most tokens: ${top.provider}` : undefined,
  )}${toolStat("Requests", number(s.requests), `${number(s.failed)} failed`, {
    spark: providerSpark("requests"),
  })}${toolStat("Tokens", short(s.tokens), undefined, {
    title: "Uncached input + cache reads + cache writes + output",
    spark: providerSpark("tokens"),
  })}${toolStat(
    "API-equivalent cost",
    toolEstimatedCost(s.cost, unpriced),
    unpriced > 0 ? `${number(unpriced)} unpriced` : undefined,
    {
      title: "What this usage would cost at public API rates",
      spark: providerSpark("cost"),
      sparkColor: "var(--chart-secondary)",
    },
  )}${toolStat(
    "Error rate",
    toolErrorRate(s.requests ? s.failed / s.requests : 0),
    `${number(s.requests - s.failed)} succeeded`,
  )}</div>`;
}
function providerErrorCell(group) {
  const rate = group.requests > 0 ? (group.failed ?? 0) / group.requests : 0;
  const tone = (group.failed ?? 0) > 0 ? toolErrorTone(rate) : "neutral";
  return `<span class="x-row" style="gap:8px;justify-content:flex-end"><span class="x-num x-dim">${number(group.failed)}</span><span class="x-badge" data-tone="${tone}" data-mono="true">${toolErrorRate(rate)}</span></span>`;
}
function providerColumns(groups, colors) {
  const maxRequests = Math.max(0, ...groups.map((group) => group.requests ?? 0)),
    maxTokens = Math.max(0, ...groups.map((group) => group.tokens ?? 0)),
    totalRequests = sum(groups.map((group) => group.requests)),
    totalTokens = sum(groups.map((group) => group.tokens));
  return [
    {
      key: "provider",
      header: "Provider",
      sort: (group) => group.provider,
      render: (group) =>
        `<span class="x-row" style="gap:8px"><span class="x-swatch" style="background:${colors.get(group.provider) ?? TOOL_OTHER_COLOR}"></span><span class="x-mono x-cell-primary">${h(group.provider)}</span></span>`,
    },
    {
      key: "requests",
      header: "Requests",
      align: "right",
      sort: (group) => group.requests ?? 0,
      render: (group) =>
        `<span title="${toolPercent(totalRequests > 0 ? (group.requests ?? 0) / totalRequests : 0)} of all requests">${toolMeter(group.requests ?? 0, maxRequests, number(group.requests))}</span>`,
    },
    {
      key: "errors",
      header: "Error rate",
      align: "right",
      sort: (group) => (group.requests > 0 ? (group.failed ?? 0) / group.requests : 0),
      render: (group) => providerErrorCell(group),
    },
    {
      key: "models",
      header: "Models",
      title: "Distinct models used through this provider",
      align: "right",
      sort: (group) => group.models ?? 0,
      render: (group) => `<span class="x-num">${number(group.models)}</span>`,
    },
    {
      key: "tokens",
      header: "Tokens",
      title: "Uncached input + cache reads + cache writes + output",
      align: "right",
      sort: (group) => group.tokens ?? 0,
      render: (group) =>
        toolMeter(
          group.tokens ?? 0,
          maxTokens,
          short(group.tokens),
          colors.get(group.provider) ?? TOOL_OTHER_COLOR,
        ),
    },
    {
      key: "share",
      header: "Share",
      title: "Fraction of all tokens in range",
      align: "right",
      sort: (group) => group.tokens ?? 0,
      render: (group) =>
        `<span class="x-num x-muted">${toolPercent(totalTokens > 0 ? (group.tokens ?? 0) / totalTokens : 0)}</span>`,
    },
    {
      key: "cost",
      header: "Cost",
      title: "API-equivalent estimate",
      align: "right",
      sort: (group) => group.cost ?? -1,
      render: (group) =>
        `<span class="x-num"${(group.unpriced ?? 0) > 0 ? ` title="${number(group.unpriced)} unpriced"` : ""}>${toolEstimatedCost(group.cost, group.unpriced ?? 0)}</span>`,
    },
    {
      key: "tps",
      header: "Tokens/s",
      title: "Average output throughput",
      align: "right",
      sort: (group) => group.tokens_per_second ?? -1,
      render: (group) => `<span class="x-num">${averageSpeed(group.tokens_per_second)}</span>`,
    },
    {
      // pp 不采集订阅倍数单位，缺失值不补零。
      key: "premium",
      header: "Premium",
      title: "Premium requests (subscription multiplier units)",
      align: "right",
      sort: () => 0,
      render: () =>
        '<span class="x-num x-dim" title="Premium request units are not recorded by pp">—</span>',
    },
  ];
}
function providerSortHeader(column) {
  const sorted = providerSort.key === column.key ? providerSort.dir : undefined;
  const arrow = sorted === "asc" ? TOOL_ARROW_UP : sorted === "desc" ? TOOL_ARROW_DOWN : "";
  return `<th data-align="${column.align || "left"}" data-sortable="${Boolean(column.sort)}"${sorted ? ` data-sorted="${sorted}" aria-sort="${sorted === "asc" ? "ascending" : "descending"}"` : ""}${column.title ? ` title="${h(column.title)}"` : ""}${column.sort ? ` data-provider-sort="${column.key}" data-provider-align="${column.align || "left"}" data-provider-focus="sort-${column.key}"` : ""}>${h(column.header)}${arrow ? `<span class="x-sort-arrow">${arrow}</span>` : ""}</th>`;
}
function providerSortedRows(rows, columns) {
  const column = columns.find((item) => item.key === providerSort.key && item.sort);
  if (!column) return rows;
  const direction = providerSort.dir === "asc" ? 1 : -1;
  return [...rows].sort((a, b) => {
    const left = column.sort(a),
      right = column.sort(b);
    const delta =
      typeof left === "number" && typeof right === "number"
        ? left - right
        : String(left).localeCompare(String(right));
    return delta * direction;
  });
}
function providerMix(group) {
  const known = sum(PROVIDER_MIX.map(([key]) => group[key] ?? 0)) ?? 0;
  const segments = PROVIDER_MIX.filter(([key]) => (group[key] ?? 0) > 0)
    .map(
      ([key, label, color]) =>
        `<div class="x-share-bar-seg" style="flex-grow:${(group[key] ?? 0) / known};background:${color}" title="${h(label)}: ${((100 * (group[key] ?? 0)) / known).toFixed(1)}%"></div>`,
    )
    .join("");
  const items = PROVIDER_MIX.map(
    ([key, label, color]) =>
      `<span class="x-legend-item" title="${h(label)}"><span class="x-swatch" style="background:${color}"></span><span class="x-truncate">${h(label)}</span><span class="x-legend-value">${short(group[key])} · ${(group.tokens ?? 0) > 0 ? toolPercent((group[key] ?? 0) / group.tokens, 0) : "–"}</span></span>`,
  ).join("");
  return `<div class="x-providers-mix"><div class="x-share-bar" style="height:8px" role="img">${segments}</div><div class="x-legend">${items}</div></div>`;
}
function providerTableMore(total, shown) {
  if (total <= PROVIDER_LIMIT) return "";
  return `<div class="x-table-more"><span class="x-micro">${number(shown)} of ${number(total)}</span>${total > shown ? `<button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-providerpage="more" data-provider-focus="more">Show more</button>` : ""}${shown > PROVIDER_LIMIT ? `<button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-providerpage="less" data-provider-focus="less">Show fewer</button>` : ""}</div>`;
}
function providerTable(groups, colors) {
  if (!groups.length) return toolEmpty("No requests in this range");
  const columns = providerColumns(groups, colors),
    rows = providerSortedRows(groups, columns),
    shown = rows.slice(0, providerShown);
  const head = columns.map(providerSortHeader).join("");
  const body = shown
    .map((group) => {
      const open = group.provider === providerSelected;
      const cells = columns
        .map(
          (column) =>
            `<td data-align="${column.align || "left"}" data-wrap="false">${column.render(group)}</td>`,
        )
        .join("");
      const detail = open
        ? `<tr><td colspan="${columns.length}" data-wrap="true" style="padding:0">${providerMix(group)}</td></tr>`
        : "";
      return `<tr data-clickable="true" data-selected="${open}" data-provider-row="${h(group.provider)}" data-provider-focus="row-${h(group.provider)}" tabindex="0">${cells}</tr>${detail}`;
    })
    .join("");
  return `<div class="x-table-wrap"><table class="x-table" data-dense="false"><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>${providerTableMore(rows.length, shown.length)}`;
}
function providerMetricSegmented() {
  return `<div class="x-segmented" data-size="sm" role="radiogroup" aria-label="Provider burn metric"><span class="x-segmented-thumb"></span>${[
    ["tokens", "Tokens"],
    ["cost", "Cost", "API-equivalent estimate"],
    ["requests", "Requests"],
  ]
    .map(
      ([value, label, title]) =>
        `<button type="button" class="x-segmented-option" role="radio" aria-checked="${providerMetric === value}" data-active="${providerMetric === value}"${title ? ` title="${h(title)}"` : ""} data-provider-metric="${value}" data-provider-focus="metric-${value}">${h(label)}</button>`,
    )
    .join("")}</div>`;
}
function providerBurn(groups, colors) {
  const buckets = fillBuckets(data.buckets).map((bucket) => bucket.timestamp);
  const points = groups.flatMap((group) =>
    group.buckets.map((bucket) => ({
      timestamp: bucket.timestamp,
      provider: group.provider,
      value: bucket[providerMetric] ?? 0,
    })),
  );
  const series = toolPivot(points, buckets, PROVIDER_TOP, colors, (point) => point.provider).map(
    (item) => ({ ...item, values: item.values.map((value) => value || null) }),
  );
  return { buckets, series };
}
function providerBurnCard(groups, colors) {
  const burn = providerBurn(groups, colors),
    cost = providerMetric === "cost",
    unpriced = data.summary.unpriced ?? 0,
    format = cost ? money : short;
  const description =
    cost && unpriced > 0
      ? `API-equivalent estimate per ${providerBucketUnit()}; excludes unpriced subscription requests`
      : `Per ${providerBucketUnit()}, top ${PROVIDER_TOP} providers stacked`;
  const chart = toolChart(
    { buckets: burn.buckets, bucketMs: providerBucketMs() },
    burn.series,
    "No provider activity in this range",
    { format, formatTooltip: cost ? money : number },
  );
  return toolCard(
    2,
    "Burn by provider",
    description,
    providerMetricSegmented(),
    `<div class="x-stack" style="gap:12px">${chart}${toolLegend(burn.series, format)}</div>`,
  );
}
function providerPeakHours(groups) {
  const selected = groups.some((group) => group.provider === providerPeak) ? providerPeak : "";
  const tokens = Array.from({ length: 24 }, () => 0),
    output = Array.from({ length: 24 }, () => 0),
    requests = Array.from({ length: 24 }, () => 0);
  for (const group of groups) {
    if (selected && group.provider !== selected) continue;
    for (const bucket of group.hours ?? []) {
      const hour = bucket.hour;
      if (!(hour >= 0 && hour < 24)) continue;
      tokens[hour] += bucket.tokens ?? 0;
      output[hour] += bucket.output_tokens ?? 0;
      requests[hour] += bucket.requests ?? 0;
    }
  }
  let peak = 0;
  for (let hour = 1; hour < 24; hour++) if (tokens[hour] > tokens[peak]) peak = hour;
  return { selected, tokens, output, requests, peak, hasData: tokens[peak] > 0 };
}
function providerPeakCard(groups) {
  const peak = providerPeakHours(groups);
  const series = [
    {
      key: "tokens",
      label: "Tokens",
      color: "var(--chart-primary)",
      values: peak.tokens.map((value, hour) => (peak.hasData && hour === peak.peak ? null : value)),
    },
    {
      key: "peak",
      label: "Tokens (peak hour)",
      color: "var(--chart-secondary)",
      values: peak.tokens.map((value, hour) => (peak.hasData && hour === peak.peak ? value : null)),
    },
  ];
  const chart = toolChart(
    { buckets: Array.from({ length: 24 }, (_, hour) => hour) },
    series,
    "No activity in this range",
    {
      format: short,
      formatTooltip: number,
      showTotal: false,
      tickLabel: (hour) => providerPad(hour),
      tooltipTitle: (hour) => `${providerPad(hour)}:00–${providerPad((hour + 1) % 24)}:00`,
      extraRows: (hour) =>
        `<div class="x-chart-tooltip-row"><span class="x-chart-tooltip-label">Output</span><span class="x-chart-tooltip-value">${number(peak.output[hour])}</span></div><div class="x-chart-tooltip-row"><span class="x-chart-tooltip-label">Requests</span><span class="x-chart-tooltip-value">${number(peak.requests[hour])}</span></div>`,
    },
  );
  const select = `<select class="x-input x-providers-select" id="provider-peak" aria-label="Provider"><option value="">All providers</option>${groups.map((group) => `<option value="${h(group.provider)}"${group.provider === peak.selected ? " selected" : ""}>${h(group.provider)}</option>`).join("")}</select>`;
  return toolCard(
    3,
    "Peak burn hours",
    peak.hasData
      ? `Tokens by local hour of day; peak at ${providerPad(peak.peak)}:00`
      : "Tokens by local hour of day",
    select,
    chart,
  );
}
function providersPage() {
  if (!data) return '<div class="loading">Loading statistics…</div>';
  toolChartModels = new Map();
  toolChartSeq = 0;
  const groups = [...data.providers].sort(
    (a, b) => (b.tokens ?? 0) - (a.tokens ?? 0) || a.provider.localeCompare(b.provider),
  );
  const colors = providerColors(groups),
    unpriced = data.summary.unpriced ?? 0;
  const description =
    unpriced > 0
      ? `Cost is an API-equivalent estimate and excludes ${number(unpriced)} unpriced subscription request${unpriced === 1 ? "" : "s"}. Select a row for its token mix.`
      : "Cost is an API-equivalent estimate. Select a row for its token mix.";
  return `<div class="x-page"><header class="x-page-header x-rise"><div><h1 class="x-page-title">Providers</h1><p class="x-page-description">Burn, reliability and subscription headroom per provider over ${h(rangeWindows[range])}.</p></div></header>${providerStatGrid(groups)}${toolCard(1, "Provider totals", description, "", providerTable(groups, colors), true)}<div class="x-grid x-grid-main-side">${providerBurnCard(groups, colors)}${providerPeakCard(groups)}</div></div>`;
}
function providersClick(e) {
  const metric = e.target.closest("[data-provider-metric]");
  if (metric) {
    providerMetric = metric.dataset.providerMetric;
    render();
    return true;
  }
  const header = e.target.closest("[data-provider-sort]");
  if (header) {
    const key = header.dataset.providerSort;
    providerSort = {
      key,
      dir:
        providerSort.key === key
          ? providerSort.dir === "asc"
            ? "desc"
            : "asc"
          : header.dataset.providerAlign === "right"
            ? "desc"
            : "asc",
    };
    render();
    return true;
  }
  const row = e.target.closest("[data-provider-row]");
  if (row) {
    const key = row.dataset.providerRow;
    providerSelected = providerSelected === key ? null : key;
    render();
    return true;
  }
  const pageButton = e.target.closest("[data-providerpage]");
  if (pageButton) {
    providerShown =
      pageButton.dataset.providerpage === "more"
        ? providerShown + PROVIDER_LIMIT * 4
        : PROVIDER_LIMIT;
    render();
    return true;
  }
  return false;
}
function providersChange(e) {
  if (e.target.id !== "provider-peak") return false;
  providerPeak = e.target.value;
  render();
  $("provider-peak")?.focus({ preventScroll: true });
  return true;
}
function costsPage() {
  if (!data) return '<div class="loading">Loading statistics…</div>';
  const s = data.summary,
    groups = [...data.models].sort((a, b) => (b.cost ?? -1) - (a.cost ?? -1)),
    top = groups.find((g) => g.priced);
  const series =
    costMode === "model"
      ? topSeries(groups, (g) => `${g.model} · ${g.key}`)
      : componentNames.map(([key, label, color]) => ({
          color,
          key,
          label,
          cost: s.cost_components?.[key],
          daily: data.daily.map((b) => ({
            timestamp: b.timestamp,
            cost: b.cost_components?.[key],
          })),
          buckets: [],
        }));
  return `<h1>Costs</h1><p class="subtitle">What the ${descriptions[range]} of usage cost at the API rates recorded with each request. Estimates are not subscription bills.</p><div class="panel metrics">${metric("API-equivalent estimate", money(s.cost), `${number(s.requests)} requests`)}${metric("Average per day", data.active_days && s.cost != null ? money(s.cost / data.active_days) : "—", `over ${number(data.active_days)} active UTC days`)}${metric("Top model", top?.model || "—", top ? `${money(top.cost)} · ${percent(top.cost, s.cost)} of estimate` : "")}${metric("Per priced request", costPerRequest(s), `${number(s.priced)} priced`)}${metric("Unpriced requests", number(s.unpriced), s.unpriced ? "excluded from estimate" : "all usage priced")}</div><div class="graphs">${chartPanel(
    "Daily estimate",
    "Per UTC day, stacked by model or billing component",
    stackedChart(series, "cost", false, true) + seriesLegend(series, "cost"),
    segmentControls(
      "cost-split",
      [
        ["model", "By model"],
        ["component", "By component"],
      ],
      costMode,
      "Split daily estimate",
    ),
  )}${chartPanel("Where it went", "Estimate by billing component", breakdown(s))}</div><section class="panel analytics-panel"><div class="requests-heading"><h2>By model</h2><p class="panel-description">Select a row for configured prices, token usage and pricing coverage.</p></div>${analyticsTable(
    [
      "Model",
      "Requests",
      "Estimate",
      "Share",
      "Split",
      "Input",
      "Output",
      "Cache read",
      "Cache write",
      "Per request",
      "Unpriced",
    ],
    groups
      .map((g) =>
        groupRow(
          g,
          "cost",
          numericCells([
            number(g.requests),
            detailed(g.cost, money(g.cost), " USD"),
            percent(g.cost, s.cost),
          ]) +
            `<td>${splitBar(g)}</td>` +
            numericCells([
              detailed(g.cost_components?.input, money(g.cost_components?.input), " USD"),
              detailed(g.cost_components?.output, money(g.cost_components?.output), " USD"),
              detailed(g.cost_components?.cache_read, money(g.cost_components?.cache_read), " USD"),
              detailed(g.cost_components?.cache_write, money(g.cost_components?.cache_write), " USD"),
              costPerRequest(g),
              number(g.unpriced),
            ]),
          () =>
            `<div class="group-details"><h2>Rates · USD per million tokens</h2>${detailsGrid(
              [
                ["Uncached input", money(g.rates?.input)],
                ["Cache read", money(g.rates?.cached_input)],
                ["Cache write", money(g.rates?.cache_write)],
                ["Output", money(g.rates?.output)],
                ["Pricing coverage", costNote(g)],
              ],
            )}${tokenMix(g)}${breakdown(g)}</div>`,
          11,
        ),
      )
      .join(""),
  )}</section>${pricingNotice()}`;
}

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
function sortValue(r, key) {
  return key === "input" ? uncached(r) : r[key];
}
function filteredRows() {
  if (!requestData) return [];
  const q = query.trim().toLowerCase();
  return requestData.requests
    .filter(
      (r) =>
        (status === "all" || r.status === status) &&
        (!q ||
          [r.model, r.provider, r.project, r.cwd, r.model_reference].some((v) =>
            v.toLowerCase().includes(q),
          )),
    )
    .sort((a, b) => {
      const x = sortValue(a, sortKey),
        y = sortValue(b, sortKey);
      if (x == null) return y == null ? b.id - a.id : 1;
      if (y == null) return -1;
      return (
        (typeof x === "string" ? x.localeCompare(y) : x - y) * sortDirection ||
        b.id - a.id
      );
    });
}
function requestSummary(rows) {
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
function requestTable(rows) {
  return `<div class="table-wrap"><table><thead><tr>${columns.map(([key, label], i) => `<th tabindex="0" class="sortable ${i > 2 && i < 10 ? "right" : ""}" data-sort="${key}" ${sortKey === key ? `aria-sort="${sortDirection < 0 ? "descending" : "ascending"}"` : ""}>${label}${sortKey === key ? `<span class="sort-arrow">${sortDirection < 0 ? "↓" : "↑"}</span>` : ""}</th>`).join("")}</tr></thead><tbody>${
    rows.length
      ? rows
          .slice(0, visible)
          .map(
            (r) =>
              `<tr tabindex="0" data-id="${r.id}"${r.has_context ? ' class="first-request-row"' : ""} aria-label="View ${h(r.model)} request ${r.id}${r.has_context ? ", session first request with saved context" : ""}"><td><div class="model-name">${h(r.model)}</div><div class="request-model-meta"><span class="provider">${h(r.provider)}</span>${r.has_context ? `<span class="first-request-mark" role="img" aria-label="Session first request" title="Session first request · full context saved">${icon("flag", 11)}</span>` : ""}</div></td><td title="${h(localTime(r.timestamp))}">${relative(r.timestamp)}</td><td class="project" title="${h(r.cwd)}">${h(r.project)}</td><td class="right num" title="${h(`Total input: ${number(r.input_tokens)}; uncached: ${number(uncached(r))}`)}">${short(uncached(r))}</td><td class="right num" title="Cache read: ${number(r.cached_tokens)}">${detailed(r.cached_tokens)}</td><td class="right num">${detailed(r.cache_write_tokens)}</td><td class="right num">${detailed(r.output_tokens)}</td><td class="right num">${detailed(r.cost, money(r.cost), " USD")}</td><td class="right num">${detailed(r.duration_ms, duration(r.duration_ms), " ms")}</td><td class="right num">${detailed(r.ttft_ms, duration(r.ttft_ms), " ms")}</td><td>${statusBadge(r)}</td></tr>`,
          )
          .join("")
      : '<tr><td colspan="11"><div class="empty">No matching requests</div></td></tr>'
  }</tbody></table></div>`;
}
function requests() {
  if (!requestData) return '<div class="loading">Loading statistics…</div>';
  const all = requestData.requests,
    s = requestSummary(all),
    rows = filteredRows(),
    oldest = all.at(-1)?.timestamp;
  return `<h1>Requests</h1><p class="subtitle">Every model call pp made in the ${descriptions[range]}, newest first. Open a row for its full payload.</p><div class="panel metrics request-metrics">${metric("Requests", number(s.count), oldest ? `since ${relative(oldest)}` : "No requests")}${metric("Failed", number(s.failed), `${s.count ? percent(s.failed, s.count) : "0.0%"} · ${s.aborted} aborted`)}${metric("Tokens", short(s.tokens))}${metric("API-equivalent cost", money(all.some((r) => r.cost != null) ? sum(all.map((r) => r.cost)) : all.length ? null : 0), `${all.filter((r) => r.cost == null).length} unpriced`)}${metric("Median duration", duration(s.median), `p95 ${duration(s.p95)}`)}${metric("Median TTFT", duration(s.ttft))}</div><section class="panel"><div class="requests-heading panel-heading"><div><h2>Request log</h2><p class="panel-description">${number(rows.length)} of the latest ${number(all.length)} requests</p></div><div class="filter-bar"><div class="search">${icon("search")}<input id="search" type="search" aria-label="Search requests" placeholder="Model, provider or project…" value="${h(query)}"></div><div class="segmented" role="radiogroup" aria-label="Status">${[
    ["all", "All", s.count],
    ["ok", "OK", s.ok],
    ["aborted", "Aborted", s.aborted],
    ["failed", "Failed", s.failed],
  ]
    .map(
      ([id, label, count]) =>
        `<button role="radio" aria-checked="${id === status}" data-status="${id}" aria-label="${label} ${count}">${label} <span class="count">${count}</span></button>`,
    )
    .join(
      "",
    )}</div></div></div>${requestTable(rows)}<div class="table-footer">${Math.min(visible, rows.length)} of ${number(rows.length)}${visible < rows.length ? '<button class="outline-button" data-more>Show more</button>' : ""}</div></section>${requestData.has_more ? `<div class="load-note"><span>Showing the latest ${number(all.length)} requests, back to ${h(localTime(oldest))}. Older requests in the ${descriptions[range]} are not loaded.</span>${limit === 500 ? '<button class="outline-button" data-load>Load latest 2,000</button>' : ""}</div>` : ""}${pricingNotice()}`;
}
function render() {
  // 进入动画只在切页或切时间范围时播放；页内交互与定时刷新直接更新内容。
  const enter = page !== renderedPage || range !== renderedRange;
  renderedPage = page;
  renderedRange = range;
  const focused = document.activeElement;
  const searchFocused = focused?.id === "search";
  const selection = searchFocused
    ? [focused.selectionStart, focused.selectionEnd]
    : null;
  const focusedId = focused?.dataset?.id;
  const providerSelectFocused = focused?.id === "provider-peak";
  const insightFocused = ["tool-filter", "insight-sort"].includes(focused?.id) ? focused.id : null;
  const focusedStatus = focused?.dataset?.status;
  const focusedExpand = focused?.closest("[data-expand]")?.dataset.expand;
  const toolsFocus = focused?.dataset?.toolsFocus;
  const providerFocus = focused?.dataset?.providerFocus;
  const modelsFocus = focused?.dataset?.modelsFocus;
  const errorsFocus = focused?.dataset?.errorsFocus;
  const focusControl = ["share", "costSplit", "series"].find((k) => focused?.dataset?.[k]);
  const focusValue = focusControl && focused.dataset[focusControl];
  const label = pages.find((p) => p[0] === page)?.[1] || page;
  const cell = focused?.closest("th,td");
  const cellPosition = cell && [
    [...$("page").querySelectorAll("table")].indexOf(cell.closest("table")),
    cell.parentElement.rowIndex,
    cell.cellIndex,
  ];
  $("page").dataset.enter = enter ? "true" : "false";
  $("page").innerHTML =
    page === "overview"
      ? overview()
      : page === "requests"
        ? requests()
        : page === "errors"
          ? errorsPage()
          : page === "traces"
            ? tracesPage()
            : ["models", "providers", "costs", "tools", "projects"].includes(page)
              ? {
                  models: modelsPage,
                  providers: providersPage,
                  costs: costsPage,
                  tools: toolsPage,
                  projects: projectsPage,
                }[page]()
              : `<h1>${h(label)}</h1><p class="subtitle">${h(label)} across your sessions in the ${descriptions[range]}.</p><section class="panel placeholder"><div class="empty">${h(label)} is coming soon.</div></section>`;
  if (page !== "tools") prepareDetails();
  syncSegmented();
  if (["tools", "providers", "models"].includes(page)) {
    if (typeof wireToolsCharts === "function") wireToolsCharts();
  }
  if (cellPosition) {
    const [table, row, column] = cellPosition;
    $("page").querySelectorAll("table")[table]?.rows[row]?.cells[column]
      ?.focus({ preventScroll: true });
  } else if (toolsFocus) {
    $("page")
      .querySelector(`[data-tools-focus="${CSS.escape(toolsFocus)}"]`)
      ?.focus({ preventScroll: true });
  } else if (providerFocus) {
    $("page")
      .querySelector(`[data-provider-focus="${CSS.escape(providerFocus)}"]`)
      ?.focus({ preventScroll: true });
  } else if (modelsFocus) {
    $("page")
      .querySelector(`[data-models-focus="${CSS.escape(modelsFocus)}"]`)
      ?.focus({ preventScroll: true });
  } else if (errorsFocus) {
    $("page")
      .querySelector(`[data-errors-focus="${CSS.escape(errorsFocus)}"]`)
      ?.focus({ preventScroll: true });
  } else if (insightFocused) {
    $(insightFocused)?.focus({ preventScroll: true });
  } else if (providerSelectFocused) {
    $("provider-peak")?.focus({ preventScroll: true });
  } else if (focusControl) {
    [...$("page").querySelectorAll("button")]
      .find((el) => el.dataset[focusControl] === focusValue)
      ?.focus({ preventScroll: true });
  } else if (focusedExpand) {
    [...$("page").querySelectorAll("[data-expand]")]
      .find((el) => el.dataset.expand === focusedExpand)
      ?.focus({ preventScroll: true });
  } else if (searchFocused && $("search")) {
    $("search").focus({ preventScroll: true });
    try {
      $("search").setSelectionRange(...selection);
    } catch {}
  } else if (focusedId) {
    $("page")
      .querySelector(`[data-id="${focusedId}"]`)
      ?.focus({ preventScroll: true });
  } else if (focusedStatus) {
    $("page")
      .querySelector(`[data-status="${focusedStatus}"]`)
      ?.focus({ preventScroll: true });
  }
  if (page === "traces") wireTraces();
  else traceRemoveDrawer();
}
$("page").addEventListener("input", (e) => {
  if (activityInput(e)) return;
  if (e.target.id === "search") {
    query = e.target.value;
    visible = 100;
    render();
  }
});
$("page").addEventListener("click", (e) => {
  if (tracesClick(e)) return;
  if (insightsClick(e)) return;
  if (providersClick(e)) return;
  if (modelsClick(e)) return;
  if (activityClick(e)) return;
  const expand = e.target.closest("[data-expand]");
  if (expand) {
    const key = expand.dataset.expand;
    if (expandedGroups.has(key)) expandedGroups.delete(key);
    else expandedGroups.add(key);
    render();
    return;
  }
  const control = e.target.closest("button");
  if (
    control &&
    ["share", "costSplit", "series"].some(
      (k) => control.dataset[k],
    )
  ) {
    const d = control.dataset;
    if (d.share) shareMode = d.share;
    if (d.costSplit) costMode = d.costSplit;
    if (d.series) {
      if (hiddenSeries.has(d.series)) hiddenSeries.delete(d.series);
      else hiddenSeries.add(d.series);
    }
    render();
    return;
  }
  const row = e.target.closest("[data-id]");
  if (row) {
    openDrawer(Number(row.dataset.id), row);
    return;
  }
  const button = e.target.closest("button,th");
  if (!button) return;
  if (button.dataset.activity) {
    activity = button.dataset.activity;
    render();
  } else if (button.dataset.status) {
    status = button.dataset.status;
    visible = 100;
    render();
  } else if (button.dataset.sort) {
    if (sortKey === button.dataset.sort) sortDirection *= -1;
    else {
      sortKey = button.dataset.sort;
      sortDirection = ["model", "project", "status"].includes(sortKey) ? 1 : -1;
    }
    render();
  } else if (button.hasAttribute("data-more")) {
    visible += 100;
    render();
  } else if (button.hasAttribute("data-load")) {
    limit = 2000;
    visible = 100;
    refresh();
  }
});
$("page").addEventListener("keydown", (e) => {
  const action = e.target.closest("th[data-sort],tr[data-id],tr[data-expand],tr[data-error-signature],th[data-errorsort],tr[data-trace],th[data-toolsort],tr[data-toolselect],th[data-provider-sort],tr[data-provider-row],tr[data-model-row]");
  if (
    (e.key === "Enter" || e.key === " ") &&
    action && !e.target.closest("button,a,input,select,textarea")
  ) {
    e.preventDefault();
    action.click();
  }
});
$("page").addEventListener("change", (e) => {
  if (insightsChange(e)) return;
  if (providersChange(e)) return;
});
function detailsGrid(items) {
  return `<dl class="detail-grid">${items.map(([key, value]) => `<div><dt>${h(key)}</dt><dd>${h(value)}</dd></div>`).join("")}</dl>`;
}
let payloads = {};
function payload(label, key, value, expanded = false) {
  payloads[key] = value;
  const recorded = value != null;
  const copy = recorded
    ? `<button class="copy" data-copy="${key}">${icon("copy", 12)} Copy</button>`
    : "";
  const body = recorded
    ? `<pre><code>${h(JSON.stringify(value, null, 2))}</code></pre>`
    : `<div class="unavailable">Not recorded — pp stores request metrics and the first-request context only.</div>`;
  return `<div class="payload"><div class="payload-top"><button class="payload-toggle" data-payload="${key}" aria-expanded="${expanded}" aria-controls="payload-${key}">${icon(expanded ? "down" : "chevron", 12)}${label}</button>${copy}</div><div id="payload-${key}" ${expanded ? "" : "hidden"}>${body}</div></div>`;
}
function drawerContent(detail) {
  const r = detail.request;
  const trace = new URLSearchParams({
    range,
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
async function openDrawer(id, element) {
  hideTooltip();
  returnFocus = element;
  selectedId = id;
  const token = ++detailGeneration;
  detailController?.abort();
  detailController = new AbortController();
  $("overlay").hidden = false;
  document.body.style.overflow = "hidden";
  for (const el of document.querySelectorAll("header,aside,main"))
    el.inert = true;
  $("drawer").innerHTML =
    '<div class="drawer-header"><h2>Request details</h2><button class="close" aria-label="Close">' +
    icon("close", 18) +
    '</button></div><div class="loading">Loading request…</div>';
  $("drawer").focus();
  try {
    const detail = await getJSON(
      `/api/stats/requests/${id}`,
      detailController.signal,
    );
    if (token !== detailGeneration) return;
    payloads = {};
    drawerContextDetail = detail;
    $("drawer").innerHTML = drawerContent(detail);
    $("drawer").querySelector(".close").focus({ preventScroll: true });
  } catch (error) {
    if (error.name === "AbortError" || token !== detailGeneration) return;
    $("drawer").innerHTML =
      `<div class="drawer-header"><h2>Request details</h2><button class="close" aria-label="Close">${icon("close", 18)}</button></div><div class="empty">${h(error.message)}</div>`;
    $("drawer").querySelector(".close").focus();
  }
}
function closeDrawer() {
  closeContextReader();
  if ($("overlay").hidden) return;
  detailGeneration++;
  detailController?.abort();
  $("overlay").hidden = true;
  document.body.style.overflow = "";
  for (const el of document.querySelectorAll("header,aside,main"))
    el.inert = false;
  const target = returnFocus?.isConnected
    ? returnFocus
    : $("page").querySelector(`[data-id="${selectedId}"]`);
  target?.focus({ preventScroll: true });
  selectedId = null;
  payloads = {};
  drawerContextDetail = null;
}
$("overlay").onclick = (e) => {
  if (e.target === $("overlay") || e.target.closest(".close")) closeDrawer();
};
$("drawer").addEventListener("click", async (e) => {
  const context = e.target.closest("[data-context-id]");
  if (context) {
    openContextReader(drawerContextDetail, context);
    return;
  }
  const toggle = e.target.closest("[data-payload]");
  if (toggle) {
    const open = toggle.getAttribute("aria-expanded") !== "true";
    toggle.setAttribute("aria-expanded", String(open));
    toggle.querySelector("svg").outerHTML = icon(open ? "down" : "chevron", 12);
    $(`payload-${toggle.dataset.payload}`).hidden = !open;
  }
  const copy = e.target.closest("[data-copy]");
  if (copy) {
    const value = payloads[copy.dataset.copy];
    if (value == null) {
      toast("No payload recorded.");
      return;
    }
    try {
      await navigator.clipboard.writeText(JSON.stringify(value, null, 2));
      toast("Copied to clipboard");
    } catch {
      toast("Copy failed. Select the JSON text and copy manually.");
    }
  }
});
function toast(text) {
  clearTimeout(toastTimer);
  $("toast").textContent = text;
  $("toast").hidden = false;
  toastTimer = setTimeout(() => ($("toast").hidden = true), 3500);
}
document.addEventListener("keydown", (e) => {
  if (contextReaderKeydown(e)) return;
  if (!$("overlay").hidden) {
    if (e.key === "Escape") {
      e.preventDefault();
      closeDrawer();
    }
    if (e.key === "Tab") {
      const focusable = [
        ...$("drawer").querySelectorAll('button,a[href],input,[tabindex="0"]'),
      ].filter((el) => !el.closest("[hidden]"));
      const first = focusable[0],
        last = focusable.at(-1);
      if (
        e.shiftKey &&
        (document.activeElement === first ||
          document.activeElement === $("drawer"))
      ) {
        e.preventDefault();
        last?.focus();
      } else if (!e.shiftKey && document.activeElement === last) {
        e.preventDefault();
        first?.focus();
      }
    }
    return;
  }
  if (
    e.target.closest('input,textarea,select,[contenteditable="true"]') ||
    e.metaKey ||
    e.ctrlKey ||
    e.altKey
  )
    return;
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
    range = ranges[Number(key) - 1];
    navigate(page);
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
