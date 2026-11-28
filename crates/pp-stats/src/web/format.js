/**
 * Shared pure helpers for formatting, escaping, numeric aggregation and SVG icon/range metadata.
 */

/** @typedef {import("../../web-types/api.d.ts").RequestRow} RequestRow */
/** @typedef {import("../../web-types/api.d.ts").StatsRange} StatsRange */

/**
 * Looks up a DOM element by ID.
 * @param {string} id
 * @returns {HTMLElement | null}
 */
export const $ = (id) => document.getElementById(id);

/**
 * Escapes special characters in HTML text and attributes so dynamic fields are not interpreted as
 * tags or script.
 * @param {unknown} value
 * @returns {string}
 */
export const h = (value) =>
  String(value ?? "").replace(
    /[&<>"']/g,
    (c) =>
      ({
        "&": "&amp;",
        "<": "&lt;",
        ">": "&gt;",
        '"': "&quot;",
        "'": "&#39;",
      })[c] ?? c,
  );

/** @type {Record<string, string>} */
export const paths = {
  overview: "M3 3h6v6H3z M15 3h6v6h-6z M3 15h6v6H3z M15 15h6v6h-6z",
  models:
    "M7 3v3 M17 3v3 M7 18v3 M17 18v3 M3 7h3 M18 7h3 M3 17h3 M18 17h3 M6 6h12v12H6z M9 9h6v6H9z",
  providers: "m14 4 6 6 m-9 3 6-6 M4 14l6 6 m-3-3 6-6 M4 10 2 8l6-6 2 2 M14 20l2 2 6-6-2-2",
  costs: "M14 3a7 7 0 1 1-7 7 M3 14a7 7 0 0 0 7 7 M14 7v6 m2-5h-3a1 1 0 0 0 0 2h2a1 1 0 0 1 0 2h-3",
  requests: "M2 12h5l3-9 4 18 3-9h5",
  errors: "M12 8v5 M12 16h.01 M22 12a10 10 0 1 1-20 0 10 10 0 0 1 20 0",
  traces: "M4 3h16v18H4z M8 7h8 M8 11h8 M8 15h5",
  branch:
    "M6 3v12 M6 15a3 3 0 1 1 0 6 3 3 0 0 1 0-6 M18 3a3 3 0 1 1 0 6 3 3 0 0 1 0-6 M6 15a9 9 0 0 1 9-9",
  tools: "m14 7-4 4 3 3 4-4 M14 7a6 6 0 0 0 7 7L9 22l-5-5 8-10a6 6 0 0 1 7-7l-4 4z",
  hooks: "M12 6v15 M5 13H2a10 10 0 0 0 20 0h-3 M12 6a2 2 0 1 0 0-4 2 2 0 0 0 0 4",
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
  alert:
    "M12 9v4 M12 17h.01 M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z",
  check: "m20 6-11 11-5-5",
  inbox: "M22 12h-6l-2 3h-4l-2-3H2 M5.5 5h13l3.5 7v7H2v-7z",
};

/**
 * Renders a built-in line SVG icon.
 * @param {string} name
 * @param {number} [size]
 * @returns {string}
 */
export const icon = (name, size = 15) =>
  `<svg width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="${paths[name] || paths.traces}"/></svg>`;

export const TOOL_INBOX =
  '<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><polyline points="22 12 16 12 14 15 10 15 8 12 2 12"/><path d="M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z"/></svg>';
export const TOOL_ARROW_UP =
  '<svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m5 12 7-7 7 7"/><path d="M12 19V5"/></svg>';
export const TOOL_ARROW_DOWN =
  '<svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14"/><path d="m19 12-7 7-7-7"/></svg>';
export const TOOL_CLEAR =
  '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg>';

/** @type {StatsRange[]} */
export const ranges = ["1h", "24h", "7d", "30d", "90d", "all"];

/** @type {Record<StatsRange, string>} */
export const descriptions = {
  "1h": "last hour",
  "24h": "last 24 hours",
  "7d": "last 7 days",
  "30d": "last 30 days",
  "90d": "last 90 days",
  all: "entire history",
};

/** @type {Record<StatsRange, string>} */
export const rangeWindows = {
  "1h": "the last hour",
  "24h": "the last 24 hours",
  "7d": "the last 7 days",
  "30d": "the last 30 days",
  "90d": "the last 90 days",
  all: "all time",
};

/** @type {Record<StatsRange, { windowLabel: string, bucketMs: number, spanMs: number | null }>} */
export const TOOL_RANGES = {
  "1h": { windowLabel: "the last hour", bucketMs: 300000, spanMs: 3600000 },
  "24h": { windowLabel: "the last 24 hours", bucketMs: 3600000, spanMs: 86400000 },
  "7d": { windowLabel: "the last 7 days", bucketMs: 86400000, spanMs: 604800000 },
  "30d": { windowLabel: "the last 30 days", bucketMs: 86400000, spanMs: 2592000000 },
  "90d": { windowLabel: "the last 90 days", bucketMs: 86400000, spanMs: 7776000000 },
  all: { windowLabel: "all time", bucketMs: 86400000, spanMs: null },
};

/**
 * Formats an integer or a nullable value.
 * @param {number | null | undefined} n
 * @returns {string}
 */
export const number = (n) => (n == null ? "—" : Number(n).toLocaleString("en-US"));

/**
 * Formats a quantity with K/M/B abbreviations.
 * @param {number | null | undefined} n
 * @returns {string}
 */
export const short = (n) => {
  if (n == null) return "—";
  if (n < 1000) return number(n);
  const unit = n >= 1e9 ? 1e9 : n >= 1e6 ? 1e6 : 1e3;
  return `${Number((n / unit).toFixed(n / unit < 10 ? 1 : 0))}${unit === 1e9 ? "B" : unit === 1e6 ? "M" : "K"}`;
};

/**
 * Outputs a compact number using Intl compact notation.
 * @param {number | null | undefined} value
 * @returns {string}
 */
export const compact = (value) =>
  value == null ? "—" : Number(value).toLocaleString("en-US", { notation: "compact" });

/**
 * Formats milliseconds as a seconds string.
 * @param {number | null | undefined} ms
 * @returns {string}
 */
export const duration = (ms) =>
  ms == null ? "—" : ms < 1000 ? `${(ms / 1000).toFixed(2)}s` : `${(ms / 1000).toFixed(1)}s`;

/**
 * Formats `n / d` as a percentage.
 * @param {number | null | undefined} n
 * @param {number | null | undefined} d
 * @returns {string}
 */
export const percent = (n, d) =>
  n != null && d != null && d > 0
    ? `${((100 * n) / d).toFixed((100 * n) / d < 0.1 ? 2 : 1)}%`
    : "—";

/**
 * Formats a ratio value as a percentage string.
 * @param {number} value
 * @param {number} [digits]
 * @returns {string}
 */
export const ratioPercent = (value, digits = 1) => `${(value * 100).toFixed(digits)}%`;

/**
 * Formats an error rate and keeps the `<0.01%` marker for tiny non-zero values.
 * @param {number} value
 * @returns {string}
 */
export const errorRateText = (value) => {
  const pct = value * 100;
  if (pct > 0 && pct < 0.005) return "<0.01%";
  return ratioPercent(value, pct > 0 && pct < 0.1 ? 2 : 1);
};

/**
 * Formats a USD cost (overview/requests page style, small amounts keep up to 6 digits).
 * @param {number | null | undefined} n
 * @returns {string}
 */
export const money = (n) =>
  n == null
    ? "—"
    : n === 0
      ? "$0"
      : `$${n.toLocaleString("en-US", {
          minimumFractionDigits: n < 0.01 ? 4 : 2,
          maximumFractionDigits: n < 0.01 ? 6 : 2,
        })}`;

/**
 * Formats a USD cost (Tools/Models/Traces style, fixed decimal places).
 * @param {number | null | undefined} value
 * @param {number} [digits]
 * @returns {string}
 */
export const fixedCost = (value, digits) => {
  if (value == null) return "—";
  if (value === 0) return "$0";
  const places = digits ?? (value > 0 && value < 0.01 ? 4 : 2);
  return `$${value.toLocaleString("en-US", {
    minimumFractionDigits: places,
    maximumFractionDigits: places,
  })}`;
};

/**
 * Formats an estimated cost including the unpriced fallback.
 * @param {number | null | undefined} value
 * @param {number} unpriced
 * @returns {string}
 */
export const estimatedCost = (value, unpriced) =>
  value == null ? "—" : value === 0 && unpriced > 0 ? "N/A" : fixedCost(value);

/**
 * Computes the total tokens of a single request (null when either input or output is unknown).
 * @param {Pick<RequestRow, "input_tokens" | "output_tokens">} r
 * @returns {number | null}
 */
export const total = (r) =>
  r.input_tokens == null || r.output_tokens == null ? null : r.input_tokens + r.output_tokens;

/**
 * Computes plain input tokens that neither hit nor wrote the cache (null when any component is
 * unknown or out of range).
 * @param {Pick<RequestRow, "input_tokens" | "cached_tokens" | "cache_write_tokens">} r
 * @returns {number | null}
 */
export const uncached = (r) =>
  r.input_tokens != null &&
  r.cached_tokens != null &&
  r.cache_write_tokens != null &&
  r.cached_tokens >= 0 &&
  r.cache_write_tokens >= 0 &&
  r.cached_tokens <= r.input_tokens &&
  r.cache_write_tokens <= r.input_tokens - r.cached_tokens
    ? r.input_tokens - r.cached_tokens - r.cache_write_tokens
    : null;

/**
 * Sums the known values; returns null when all are null/undefined and the array is not empty.
 * @param {Array<number | null | undefined>} values
 * @returns {number | null}
 */
export const sum = (values) => {
  const known = /** @type {number[]} */ (values.filter((v) => v != null));
  return values.length && !known.length ? null : known.reduce((a, b) => a + b, 0);
};

/**
 * Computes a linearly interpolated quantile.
 * @param {Array<number | null | undefined>} values
 * @param {number} q
 * @returns {number | null}
 */
export const quantile = (values, q) => {
  const sorted = /** @type {number[]} */ (values.filter((v) => v != null)).sort((a, b) => a - b);
  if (!sorted.length) return null;
  const x = (sorted.length - 1) * q;
  const i = Math.floor(x);
  return sorted[i] + (sorted[Math.ceil(x)] - sorted[i]) * (x - i);
};

/**
 * Computes a relative-time description (under 30 seconds is not counted in minutes).
 * @param {number} ms
 * @param {number} [nowMs]
 * @returns {string}
 */
export const relative = (ms, nowMs = Date.now()) => {
  const minutes = Math.max(0, (nowMs - ms) / 60000);
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

/**
 * Formats the short time delta of the topbar Live chip.
 * @param {number} ms
 * @param {number} [nowMs]
 * @returns {string}
 */
export const liveAge = (ms, nowMs = Date.now()) => {
  const minutes = Math.floor(Math.max(0, nowMs - ms) / 60000);
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
};

/**
 * Formats a local time string.
 * @param {number} ms
 * @returns {string}
 */
export const localTime = (ms) =>
  new Date(ms).toLocaleString("en-US", {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });

/**
 * Pads to two digits.
 * @param {number} value
 * @returns {string}
 */
export const pad2 = (value) => String(value).padStart(2, "0");

/**
 * Wraps display text with a hover tooltip carrying the exact full value.
 * @param {number | null | undefined} value
 * @param {string} [display]
 * @param {string} [unit]
 * @returns {string}
 */
export const detailed = (value, display = short(value), unit = "") =>
  `<span title="${h(
    value == null
      ? "Not reported"
      : `${Number(value).toLocaleString("en-US", { maximumSignificantDigits: 21 })}${unit}`,
  )}">${display}</span>`;

/**
 * Generates the transparent hit-area rectangle for focus and hover in a chart.
 * @param {number} x
 * @param {number} y
 * @param {number} width
 * @param {number} height
 * @param {string} text
 * @returns {string}
 */
export const chartHit = (x, y, width, height, text) =>
  `<rect class="chart-hit" x="${x}" y="${y}" width="${width}" height="${height}" tabindex="0" role="graphics-symbol" aria-label="${h(text)}" data-tooltip="${h(text)}"/>`;
