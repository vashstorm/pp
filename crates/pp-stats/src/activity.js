/**
 * First-request context desktop reader (browse, search and copy by Context / Tools / Skills category).
 */

import { toolCard, toolStat } from "./web/charts.js";
import { descriptions, fixedCost, h, icon, localTime, number, relative } from "./web/format.js";
import { app, hooks } from "./web/state.js";
import { renderSortHeader, renderTableMore, sortRows, xEmpty, xMeter } from "./web/table.js";

/** @typedef {import("../web-types/api.d.ts").ErrorsResponse} ErrorsResponse */
/** @typedef {import("../web-types/api.d.ts").RequestRow} RequestRow */
/** @typedef {import("../web-types/api.d.ts").SortDirection} SortDirection */
/** @typedef {import("../web-types/api.d.ts").SortState} SortState */

const ERROR_LIMITS = [50, 200, 1000];
const ERROR_SIGNATURE_LENGTH = 180;
const ERROR_SIGNATURE_ROWS = 12;
const ERROR_FAILURE_ROWS = 50;
// Normalization order matches the reference implementation: UUIDs, prefixed IDs and long hex strings
// become placeholders first, then numbers become N.
const ERROR_UUID = /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/gi;
const ERROR_PREFIXED_ID = /\b(?:req|msg|call|toolu|chatcmpl|resp|run|gen)[-_][A-Za-z0-9_-]{6,}/g;
const ERROR_LONG_HEX = /\b[0-9a-f]{12,}\b/gi;
const ERROR_NUMBER = /(?<![\w.-])\d+(?:\.\d+)?/g;
const ERROR_STATUS = /^[1-5]\d\d$/;
const ERROR_ARROW_UP_RIGHT =
  '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M7 7h10v10"/><path d="M7 17 17 7"/></svg>';

let errorLimit = ERROR_LIMITS[0];
/** @type {string | null} */
let errorSignature = null;
/** @type {string | null} */
let errorModel = null;
let errorQuery = "";
/** @type {SortState} */
let errorSignatureSort = { key: "", dir: "" };
/** @type {SortState} */
let errorFailureSort = { key: "time", dir: "desc" };
let errorSignatureShown = ERROR_SIGNATURE_ROWS;
let errorFailureShown = ERROR_FAILURE_ROWS;

/**
 * Returns the current request paging limit of the Errors page, used by `refresh()` to assemble the
 * query parameters.
 * @returns {number}
 */
export function getErrorLimit() {
  return errorLimit;
}

/**
 * Resets the Errors page filtering and paging state on route or time range changes.
 */
export function resetActivity() {
  errorLimit = ERROR_LIMITS[0];
  errorSignature = null;
  errorModel = null;
  errorQuery = "";
  errorSignatureSort = { key: "", dir: "" };
  errorFailureSort = { key: "time", dir: "desc" };
  errorSignatureShown = ERROR_SIGNATURE_ROWS;
  errorFailureShown = ERROR_FAILURE_ROWS;
}

/**
 * Normalizes an error message signature: when processing line by line, the normalized text itself is
 * the grouping key.
 * @param {string | null | undefined} message
 * @returns {string}
 */
export function errorSignatureText(message) {
  if (!message?.trim()) return "Unknown error";
  const normalized = message
    .replace(/\s+/g, " ")
    .trim()
    .replace(ERROR_UUID, "<id>")
    .replace(ERROR_PREFIXED_ID, "<id>")
    .replace(ERROR_LONG_HEX, "<hex>")
    .replace(ERROR_NUMBER, (value) => (ERROR_STATUS.test(value) ? value : "N"));
  return normalized.length > ERROR_SIGNATURE_LENGTH
    ? `${normalized.slice(0, ERROR_SIGNATURE_LENGTH - 1)}…`
    : normalized;
}

/**
 * Returns the model identifier of a failed request (same source as the backend `model_reference`).
 * @param {RequestRow} row
 * @returns {string}
 */
const errorModelKey = (row) => row.model_reference;

/**
 * Aggregates the loaded failed request rows by normalized error signature.
 * @param {RequestRow[]} rows
 */
export function errorGroups(rows) {
  /** @type {Map<string, {
   *   signature: string,
   *   count: number,
   *   firstSeen: number,
   *   lastSeen: number,
   *   latest: RequestRow,
   *   models: Map<string, { key: string, model: string, provider: string, count: number }>,
   * }>} */
  const groups = new Map();
  for (const row of rows) {
    const signature = errorSignatureText(row.error_message);
    let group = groups.get(signature);
    if (!group) {
      group = {
        signature,
        count: 0,
        firstSeen: row.timestamp,
        lastSeen: row.timestamp,
        latest: row,
        models: new Map(),
      };
      groups.set(signature, group);
    }
    group.count++;
    if (row.timestamp < group.firstSeen) group.firstSeen = row.timestamp;
    if (row.timestamp > group.lastSeen) {
      group.lastSeen = row.timestamp;
      group.latest = row;
    }
    const key = errorModelKey(row);
    const model = group.models.get(key);
    if (model) model.count++;
    else {
      group.models.set(key, {
        key,
        model: row.model,
        provider: row.provider,
        count: 1,
      });
    }
  }
  return [...groups.values()]
    .map((group) => ({
      ...group,
      models: [...group.models.values()].sort(
        (a, b) => b.count - a.count || a.model.localeCompare(b.model),
      ),
    }))
    .sort((a, b) => b.count - a.count || b.lastSeen - a.lastSeen);
}

/**
 * Aggregates the loaded failed request counts by model.
 * @param {RequestRow[]} rows
 * @returns {Array<{ key: string, model: string, provider: string, count: number }>}
 */
export function errorModelGroups(rows) {
  /** @type {Map<string, { key: string, model: string, provider: string, count: number }>} */
  const models = new Map();
  for (const row of rows) {
    const key = errorModelKey(row);
    const model = models.get(key);
    if (model) model.count++;
    else models.set(key, { key, model: row.model, provider: row.provider, count: 1 });
  }
  return [...models.values()].sort((a, b) => b.count - a.count || a.model.localeCompare(b.model));
}

/** @param {RequestRow} row */
const errorTokens = (row) =>
  row.input_tokens == null || row.output_tokens == null
    ? null
    : row.input_tokens + row.output_tokens;

// Costs are displayed from the snapshot recorded with the request; unpriced requests (no rate
// snapshot) show N/A as on the reference page.
/** @param {RequestRow} row */
const errorCost = (row) => (row.cost == null ? "N/A" : fixedCost(row.cost, 4));

/**
 * @template T
 * @param {{
 *   columns: import("./web/table.js").TableColumn<T>[],
 *   rows: T[],
 *   sort: SortState,
 *   shown: number,
 *   step: number,
 *   table: string,
 *   dense?: boolean,
 *   rowAttr: (row: T) => string,
 *   selected: (row: T) => boolean,
 *   expand?: (row: T) => string,
 *   empty: string,
 * }} config
 */
function errorTable(config) {
  const { columns, rows, sort, shown, step, table, dense, rowAttr, selected, expand, empty } =
    config;
  if (!rows.length) return empty;
  const sorted = sortRows(rows, columns, sort);
  const visible = sorted.slice(0, shown);
  const head = columns
    .map((column) =>
      renderSortHeader(column, sort, {
        sortAttr: "data-errorsort",
        tableAttr: "data-errortable",
        table,
        alignAttr: "data-erroralign",
      }),
    )
    .join("");
  const body = visible
    .map((row) => {
      const detail = expand ? expand(row) : "";
      return `<tr data-clickable="true" data-selected="${selected(row)}" tabindex="0" ${rowAttr(row)}>${columns
        .map(
          (column) =>
            `<td data-align="${column.align || "left"}"${column.wrap ? ' data-wrap="true"' : ""}>${column.render(row)}</td>`,
        )
        .join("")}</tr>${
        detail
          ? `<tr><td colspan="${columns.length}" data-wrap="true" style="padding:0">${detail}</td></tr>`
          : ""
      }`;
    })
    .join("");
  return `<div class="x-table-wrap"><table class="x-table"${dense ? ' data-dense="true"' : ""}><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>${renderTableMore(
    {
      total: sorted.length,
      shown: visible.length,
      step,
      moreAttr: `data-errortable="${table}" data-errorpage="more"`,
      lessAttr: `data-errortable="${table}" data-errorpage="less"`,
      focusAttr: "data-errors-focus",
      focusMore: `${table}-more`,
      focusLess: `${table}-less`,
    },
  )}`;
}

/**
 * @param {string} primary
 * @param {string | null | undefined} secondary
 */
function errorCell(primary, secondary) {
  return `<div class="x-row" style="gap:10px"><div class="x-stack" style="gap:0"><span class="x-cell-primary x-truncate">${primary}</span>${secondary == null ? "" : `<span class="x-cell-secondary x-truncate">${h(secondary)}</span>`}</div></div>`;
}

/** @param {ReturnType<typeof errorGroups>[number]} group */
function errorGroupDetail(group) {
  const latest = group.latest;
  const source = latest.error_message;
  return `<div class="errors-detail"><pre class="x-code-block errors-detail-message">${h(source || "Unknown error")}</pre><div class="errors-detail-meta"><span class="x-muted">${number(group.count)} failures · first ${h(localTime(group.firstSeen))} · last ${h(localTime(group.lastSeen))}</span>${latest.id == null ? "" : `<button type="button" class="x-btn" data-size="sm" data-error-latest="${latest.id}" data-errors-focus="latest-${encodeURIComponent(group.signature)}">Open latest ${ERROR_ARROW_UP_RIGHT}</button>`}</div><div class="errors-detail-models">${group.models
    .map(
      (model) =>
        `<span class="x-badge" data-mono="true">${h(model.model)} <span class="x-dim">${h(model.provider)}</span> <span class="x-num">${number(model.count)}</span></span>`,
    )
    .join("")}</div></div>`;
}

/**
 * @param {number} top
 * @returns {import("./web/table.js").TableColumn<ReturnType<typeof errorGroups>[number]>[]}
 */
function errorSignatureColumns(top) {
  return [
    {
      key: "signature",
      header: "Signature",
      wrap: true,
      sort: (group) => group.signature,
      render: (group) =>
        `<span class="errors-signature"${group.latest.error_message == null ? "" : ` title="${h(group.latest.error_message)}"`}>${h(group.signature)}</span>`,
    },
    {
      key: "models",
      header: "Models",
      sort: (group) => group.models.length,
      render: (group) =>
        errorCell(
          `<span class="x-mono">${h(group.models[0]?.model)}</span>`,
          group.models.length > 1 ? `+${group.models.length - 1} more` : group.models[0]?.provider,
        ),
    },
    {
      key: "last",
      header: "Last seen",
      sort: (group) => group.lastSeen,
      render: (group) =>
        `<span class="x-muted" title="${h(localTime(group.lastSeen))}">${relative(group.lastSeen)}</span>`,
    },
    {
      key: "count",
      header: "Failures",
      align: "right",
      width: 120,
      sort: (group) => group.count,
      render: (group) => xMeter(group.count, top, number(group.count), "var(--bad)"),
    },
  ];
}

/** @type {import("./web/table.js").TableColumn<RequestRow>[]} */
const errorFailureColumns = [
  {
    key: "time",
    header: "When",
    width: 120,
    sort: (row) => row.timestamp,
    render: (row) =>
      `<span class="x-muted" title="${h(localTime(row.timestamp))}">${relative(row.timestamp)}</span>`,
  },
  {
    key: "model",
    header: "Model",
    sort: (row) => row.model,
    render: (row) => errorCell(`<span class="x-mono">${h(row.model)}</span>`, row.provider),
  },
  {
    key: "error",
    header: "Error",
    sort: (row) => row.error_message ?? "",
    render: (row) =>
      `<span class="errors-message"${row.error_message == null ? "" : ` title="${h(row.error_message)}"`}>${h(row.error_message || "Unknown error")}</span>`,
  },
  {
    key: "project",
    header: "Project",
    sort: (row) => row.project,
    render: (row) => `<span class="x-mono x-muted" title="${h(row.cwd)}">${h(row.project)}</span>`,
  },
  {
    key: "tokens",
    header: "Tokens",
    align: "right",
    sort: (row) => errorTokens(row) ?? -1,
    render: (row) => `<span class="x-num">${number(errorTokens(row))}</span>`,
  },
  {
    key: "cost",
    header: "Cost",
    title: "API-equivalent estimate",
    align: "right",
    sort: (row) => row.cost ?? -1,
    render: (row) => `<span class="x-num">${errorCost(row)}</span>`,
  },
];

/** @param {ReturnType<typeof errorModelGroups>} models */
function errorModelList(models) {
  if (!models.length) return xEmpty("No affected models");
  const max = models[0].count;
  return `<div class="x-bar-list">${models
    .slice(0, 12)
    .map((model) => {
      const width = max > 0 ? Math.max(0.5, (model.count / max) * 100) : 0;
      const color =
        model.key === errorModel ? "var(--bad)" : "color-mix(in srgb, var(--bad) 45%, transparent)";
      return `<button type="button" class="x-bar-list-row" data-error-model="${h(model.key)}" data-errors-focus="model-${encodeURIComponent(model.key)}" aria-pressed="${model.key === errorModel}"><span class="x-bar-list-fill" style="width:${width}%;background:${color}"></span><span class="x-bar-list-label"><span class="x-row" style="gap:6px;min-width:0"><span class="x-mono x-truncate">${h(model.model)}</span><span class="x-dim x-truncate">${h(model.provider)}</span></span></span><span class="x-bar-list-value">${number(model.count)}</span></button>`;
    })
    .join("")}</div>`;
}

/**
 * @param {string} kind
 * @param {string} label
 */
function errorFilterChip(kind, label) {
  return `<button type="button" class="x-btn errors-filter" data-size="sm" data-error-clear="${kind}" data-errors-focus="clear-${kind}" title="${h(label)}"><span class="x-mono x-truncate">${h(label)}</span>${icon("close", 12)}</button>`;
}

/**
 * Renders the Errors page.
 * @returns {string}
 */
export function errorsPage() {
  const payload = /** @type {ErrorsResponse | null} */ (app.data);
  if (!payload) return '<div class="loading">Loading statistics…</div>';
  const rows = payload.requests || [];
  const groups = errorGroups(rows);
  const models = errorModelGroups(rows);
  /** @type {RequestRow | null} */
  const newest = rows.reduce(
    (/** @type {RequestRow | null} */ found, row) =>
      !found || row.timestamp > found.timestamp ? row : found,
    null,
  );
  // A filter value that disappeared after a refresh is no longer active, but the state is kept: when
  // the row comes back the filter with the same name applies again.
  const signature = groups.some((group) => group.signature === errorSignature)
    ? errorSignature
    : null;
  const model = models.some((item) => item.key === errorModel) ? errorModel : null;
  const needle = errorQuery.trim().toLowerCase();
  const filtered = rows.filter(
    (row) =>
      (signature === null || errorSignatureText(row.error_message) === signature) &&
      (model === null || errorModelKey(row) === model) &&
      (needle === "" ||
        [row.model, row.provider, row.project, row.error_message].some((value) =>
          String(value ?? "")
            .toLowerCase()
            .includes(needle),
        )),
  );
  const loaded = rows.length;
  const limitIndex = ERROR_LIMITS.indexOf(errorLimit);
  const next =
    limitIndex >= 0 && limitIndex + 1 < ERROR_LIMITS.length ? ERROR_LIMITS[limitIndex + 1] : null;
  // Completeness depends only on the server-side has_more; a loaded row count equal to the limit can
  // still be a complete page.
  const complete = !payload.has_more;
  const selectedModel = models.find((item) => item.key === model);
  const filteredSignature = groups.find((group) => group.signature === signature);
  const windowLabel = descriptions[app.range];
  const stats = [
    toolStat(
      "Failures",
      number(loaded),
      complete ? `in the ${windowLabel}` : `latest ${number(loaded)} loaded`,
    ),
    toolStat(
      "Signatures",
      number(groups.length),
      groups[0] ? `top: ${number(groups[0].count)} failures` : undefined,
      {
        title: "Distinct error messages after normalizing ids and numbers",
      },
    ),
    toolStat("Affected models", number(models.length), models[0]?.model),
    toolStat(
      "Last failure",
      newest ? relative(newest.timestamp) : "–",
      newest ? localTime(newest.timestamp) : `none in the ${windowLabel}`,
    ),
  ].join("");
  const actions = `${signature === null ? "" : errorFilterChip("signature", signature)}${
    selectedModel ? errorFilterChip("model", selectedModel.model) : ""
  }<div class="x-search">${icon("search", 14)}<input class="x-input" id="search" type="search" aria-label="Filter" placeholder="Message, model or project" value="${h(errorQuery)}"></div>`;
  const footer = complete
    ? ""
    : `<span>Showing the latest ${number(loaded)} failures in the ${windowLabel}; older ones are not loaded.</span>${
        next === null
          ? ""
          : `<button type="button" class="x-btn" data-size="sm" data-errors-load>Load latest ${number(next)}</button>`
      }`;
  return `<div class="x-page"><header class="x-page-header x-rise"><div><h1 class="x-page-title">Errors</h1><p class="x-page-description">Failed model requests in the ${h(windowLabel)}, grouped by error signature. Open a failure for its full payload.</p></div></header><div class="x-stat-grid x-rise" style="--stat-min:170px">${stats}</div><div class="x-grid x-grid-main-side">${toolCard(
    1,
    "Error signatures",
    "Same message with ids and counters normalized. Select one to filter the failures below.",
    "",
    errorTable({
      columns: errorSignatureColumns(groups[0]?.count ?? 0),
      rows: groups,
      sort: errorSignatureSort,
      shown: errorSignatureShown,
      step: ERROR_SIGNATURE_ROWS,
      table: "signature",
      rowAttr: (group) =>
        `data-error-signature="${h(group.signature)}" data-errors-focus="signature-${encodeURIComponent(group.signature)}"`,
      selected: (group) => group.signature === signature,
      expand: (group) => (group.signature === signature ? errorGroupDetail(group) : ""),
      empty: xEmpty(`No failures in the ${windowLabel}`, "Every request succeeded or was aborted."),
    }),
    true,
  )}${toolCard(
    2,
    "By model",
    "Failures per model. Select one to filter.",
    "",
    errorModelList(models),
  )}</div>${toolCard(
    3,
    "Failures",
    `${number(filtered.length)} of ${number(loaded)} failures, newest first`,
    actions,
    errorTable({
      columns: errorFailureColumns,
      rows: filtered,
      sort: errorFailureSort,
      shown: errorFailureShown,
      step: ERROR_FAILURE_ROWS,
      table: "failure",
      dense: true,
      rowAttr: (row) => `data-id="${row.id}" data-errors-focus="failure-${row.id}"`,
      selected: () => false,
      empty: xEmpty(
        filteredSignature || selectedModel
          ? "No failures match"
          : `No failures in the ${windowLabel}`,
        filteredSignature || selectedModel || needle
          ? "Clear the search or the selected filters."
          : "",
      ),
    }),
    true,
    footer,
  )}</div>`;
}

/**
 * Handles input events of the Errors page search box.
 * @param {Event} e
 * @returns {boolean}
 */
export function activityInput(e) {
  if (
    app.page !== "errors" ||
    !(e.target instanceof HTMLInputElement) ||
    e.target.id !== "search"
  ) {
    return false;
  }
  errorQuery = e.target.value;
  errorFailureShown = ERROR_FAILURE_ROWS;
  hooks.render();
  return true;
}

/**
 * Handles table sorting, paging, filtering and "view latest failure" clicks on the Errors page.
 * @param {MouseEvent} e
 * @returns {boolean}
 */
export function activityClick(e) {
  if (!(e.target instanceof Element)) return false;
  const clear = e.target.closest("[data-error-clear]");
  if (clear instanceof HTMLElement) {
    if (clear.dataset.errorClear === "signature") errorSignature = null;
    else errorModel = null;
    hooks.render();
    return true;
  }
  const header = e.target.closest("[data-errorsort]");
  if (header instanceof HTMLElement) {
    const table = header.dataset.errortable;
    const key = header.dataset.errorsort ?? "";
    const state = table === "signature" ? errorSignatureSort : errorFailureSort;
    /** @type {SortDirection} */
    const dir =
      state.key === key
        ? state.dir === "asc"
          ? "desc"
          : "asc"
        : header.dataset.erroralign === "right"
          ? "desc"
          : "asc";
    if (table === "signature") errorSignatureSort = { key, dir };
    else errorFailureSort = { key, dir };
    hooks.render();
    return true;
  }
  const moreButton = e.target.closest("[data-errorpage]");
  if (moreButton instanceof HTMLElement) {
    const table = moreButton.dataset.errortable;
    const step = table === "signature" ? ERROR_SIGNATURE_ROWS : ERROR_FAILURE_ROWS;
    const grow = moreButton.dataset.errorpage === "more";
    if (table === "signature") {
      errorSignatureShown = grow ? errorSignatureShown + step * 4 : step;
    } else {
      errorFailureShown = grow ? errorFailureShown + step * 4 : step;
    }
    hooks.render();
    return true;
  }
  if (e.target.closest("[data-errors-load]")) {
    const index = ERROR_LIMITS.indexOf(errorLimit);
    if (index >= 0 && index + 1 < ERROR_LIMITS.length) {
      errorLimit = ERROR_LIMITS[index + 1];
      errorFailureShown = ERROR_FAILURE_ROWS;
      hooks.refresh();
    }
    return true;
  }
  const latest = e.target.closest("[data-error-latest]");
  if (latest instanceof HTMLElement) {
    hooks.openDrawer(Number(latest.dataset.errorLatest), latest);
    return true;
  }
  const signatureRow = e.target.closest("[data-error-signature]");
  if (signatureRow instanceof HTMLElement) {
    const signature = signatureRow.dataset.errorSignature ?? null;
    errorSignature = errorSignature === signature ? null : signature;
    hooks.render();
    return true;
  }
  const modelRow = e.target.closest("[data-error-model]");
  if (modelRow instanceof HTMLElement) {
    const key = modelRow.dataset.errorModel ?? null;
    errorModel = errorModel === key ? null : key;
    hooks.render();
    return true;
  }
  return false;
}
