/**
 * Application state shared across modules plus the runtime callback bridge, avoiding circular imports
 * and globals.
 */

/** @typedef {import("../../web-types/api.d.ts").AnalyticsResponse} AnalyticsResponse */
/** @typedef {import("../../web-types/api.d.ts").ErrorsResponse} ErrorsResponse */
/** @typedef {import("../../web-types/api.d.ts").HooksResponse} HooksResponse */
/** @typedef {import("../../web-types/api.d.ts").ProjectsResponse} ProjectsResponse */
/** @typedef {import("../../web-types/api.d.ts").RequestDetailResponse} RequestDetailResponse */
/** @typedef {import("../../web-types/api.d.ts").RequestsResponse} RequestsResponse */
/** @typedef {import("../../web-types/api.d.ts").StatsPage} StatsPage */
/** @typedef {import("../../web-types/api.d.ts").StatsRange} StatsRange */
/** @typedef {import("../../web-types/api.d.ts").StatsStyle} StatsStyle */
/** @typedef {import("../../web-types/api.d.ts").ToolsResponse} ToolsResponse */
/** @typedef {import("../../web-types/api.d.ts").TraceDetailResponse} TraceDetailResponse */
/** @typedef {import("../../web-types/api.d.ts").TracesListResponse} TracesListResponse */

/**
 * @typedef {AnalyticsResponse | ErrorsResponse | TracesListResponse | TraceDetailResponse | ToolsResponse | HooksResponse | ProjectsResponse} PageData
 */

export const app = {
  /** @type {StatsPage} */
  page: "overview",
  /** @type {StatsRange} */
  range: "24h",
  /** @type {PageData | null} */
  data: null,
  /** @type {RequestsResponse | null} */
  requestData: null,
  limit: 500,
  visible: 100,
  query: "",
  status: "all",
  sortKey: "timestamp",
  sortDirection: -1,
  /** @type {"requests" | "tokens" | "cost"} */
  activity: "requests",
  /** @type {StatsStyle} */
  style: "default",
  /** @type {Set<string>} */
  expandedGroups: new Set(),
  /** @type {Set<string>} */
  hiddenSeries: new Set(),
  /** @type {"share" | "requests"} */
  shareMode: "share",
  /** @type {"model" | "component"} */
  costMode: "model",
  /** @type {RequestDetailResponse | null} */
  drawerContextDetail: null,
};

/** @type {{
 *   render: () => void,
 *   refresh: () => Promise<void> | void,
 *   openDrawer: (id: number, element?: Element | null) => Promise<void> | void,
 *   toast: (text: string) => void,
 *   hideTooltip: () => void,
 * }} */
export const hooks = {
  render: () => {},
  refresh: () => {},
  openDrawer: () => {},
  toast: () => {},
  hideTooltip: () => {},
};

/**
 * Registers the entry-level runtime callbacks, called by `stats.js` at startup.
 * @param {Partial<typeof hooks>} next
 */
export function registerHooks(next) {
  Object.assign(hooks, next);
}

/**
 * Requests same-origin stats JSON and maps HTTP statuses to readable error messages.
 * @template T
 * @param {string} url
 * @param {AbortSignal} [signal]
 * @returns {Promise<T>}
 */
export async function getJSON(url, signal) {
  const response = await fetch(url, { signal, cache: "no-store" });
  if (!response.ok) {
    throw new Error(
      response.status === 404
        ? "Request no longer exists."
        : response.status === 503
          ? "Statistics database is temporarily unavailable."
          : `Statistics request failed (${response.status}).`,
    );
  }
  return /** @type {Promise<T>} */ (response.json());
}
