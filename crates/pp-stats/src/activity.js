"use strict";
// Errors 页：失败请求按归一化签名分组，签名与模型都可点击筛选，两张表各自维护排序与分页。
// 分组、模型分布与统计量都按“当前已加载的行”计算，加载更多后随之变化。
const ERROR_LIMITS = [50, 200, 1000];
const ERROR_SIGNATURE_LENGTH = 180;
const ERROR_SIGNATURE_ROWS = 12;
const ERROR_FAILURE_ROWS = 50;
// 归一化顺序与参考实现一致：UUID、带前缀的 ID、长十六进制串先换成占位符，数字再换成 N。
const ERROR_UUID = /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/gi;
const ERROR_PREFIXED_ID = /\b(?:req|msg|call|toolu|chatcmpl|resp|run|gen)[-_][A-Za-z0-9_-]{6,}/g;
const ERROR_LONG_HEX = /\b[0-9a-f]{12,}\b/gi;
const ERROR_NUMBER = /(?<![\w.-])\d+(?:\.\d+)?/g;
const ERROR_STATUS = /^[1-5]\d\d$/;
const ERROR_ARROW_UP_RIGHT =
  '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M7 7h10v10"/><path d="M7 17 17 7"/></svg>';
let errorLimit = ERROR_LIMITS[0],
  errorSignature = null,
  errorModel = null,
  errorQuery = "",
  errorSignatureSort = { key: "", dir: "" },
  errorFailureSort = { key: "time", dir: "desc" },
  errorSignatureShown = ERROR_SIGNATURE_ROWS,
  errorFailureShown = ERROR_FAILURE_ROWS;

function resetActivity() {
  errorLimit = ERROR_LIMITS[0];
  errorSignature = null;
  errorModel = null;
  errorQuery = "";
  errorSignatureSort = { key: "", dir: "" };
  errorFailureSort = { key: "time", dir: "desc" };
  errorSignatureShown = ERROR_SIGNATURE_ROWS;
  errorFailureShown = ERROR_FAILURE_ROWS;
}

/// 归一化后的签名：逐行处理时同一条归一化文本即为分组键。
function errorSignatureText(message) {
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

/// 一个失败请求的模型身份：与 `/api/stats/errors` 的 `model_reference` 同源。
const errorModelKey = (row) => row.model_reference;

function errorGroups(rows) {
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
    else
      group.models.set(key, {
        key,
        model: row.model,
        provider: row.provider,
        count: 1,
      });
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

function errorModelGroups(rows) {
  const models = new Map();
  for (const row of rows) {
    const key = errorModelKey(row);
    const model = models.get(key);
    if (model) model.count++;
    else models.set(key, { key, model: row.model, provider: row.provider, count: 1 });
  }
  return [...models.values()].sort(
    (a, b) => b.count - a.count || a.model.localeCompare(b.model),
  );
}

const errorTokens = (row) =>
  row.input_tokens == null || row.output_tokens == null
    ? null
    : row.input_tokens + row.output_tokens;

// 费用按请求记录时的快照展示；未定价（无费率快照）与参考页一样显示 N/A。
const errorCost = (row) => (row.cost == null ? "N/A" : toolCost(row.cost, 4));

function errorEmpty(title, hint = "") {
  return `<div class="x-empty">${TOOL_INBOX}<div class="x-empty-title">${h(title)}</div>${hint ? `<div class="x-empty-hint">${h(hint)}</div>` : ""}</div>`;
}

function errorSortHeader(column, sort, table) {
  const sorted = sort.key === column.key ? sort.dir : "";
  const arrow =
    sorted === "asc" ? TOOL_ARROW_UP : sorted === "desc" ? TOOL_ARROW_DOWN : "";
  return `<th data-align="${column.align || "left"}" data-sortable="${Boolean(column.sort)}"${sorted ? ` data-sorted="${sorted}" aria-sort="${sorted === "asc" ? "ascending" : "descending"}"` : ""}${column.width ? ` style="width:${column.width}px"` : ""}${column.title ? ` title="${h(column.title)}"` : ""}${column.sort ? ` data-errorsort="${column.key}" data-errortable="${table}" data-erroralign="${column.align || "left"}"` : ""}>${h(column.header)}${arrow ? `<span class="x-sort-arrow">${arrow}</span>` : ""}</th>`;
}

function errorTableMore(total, shown, step, table) {
  if (total <= step) return "";
  return `<div class="x-table-more"><span class="x-micro">${number(shown)} of ${number(total)}</span>${total > shown ? `<button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-errortable="${table}" data-errorpage="more" data-errors-focus="${table}-more">Show more</button>` : ""}${shown > step ? `<button type="button" class="x-btn" data-size="sm" data-variant="ghost" data-errortable="${table}" data-errorpage="less" data-errors-focus="${table}-less">Show fewer</button>` : ""}</div>`;
}

/// 表格骨架：列定义、排序、行分页、展开行与行内焦点属性都由调用方给出。
function errorTable(config) {
  const { columns, rows, sort, shown, step, table, dense, rowAttr, selected, expand, empty } =
    config;
  if (!rows.length) return empty;
  const sorted = toolSorted(rows, columns, sort);
  const visible = sorted.slice(0, shown);
  const head = columns.map((column) => errorSortHeader(column, sort, table)).join("");
  const body = visible
    .map((row) => {
      const detail = expand ? expand(row) : "";
      return `<tr data-clickable="true" data-selected="${selected(row)}" tabindex="0" ${rowAttr(row)}>${columns
        .map(
          (column) =>
            `<td data-align="${column.align || "left"}"${column.wrap ? ' data-wrap="true"' : ""}>${column.render(row)}</td>`,
        )
        .join("")}</tr>${
        detail ? `<tr><td colspan="${columns.length}" data-wrap="true" style="padding:0">${detail}</td></tr>` : ""
      }`;
    })
    .join("");
  return `<div class="x-table-wrap"><table class="x-table"${dense ? ' data-dense="true"' : ""}><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>${errorTableMore(sorted.length, visible.length, step, table)}`;
}

function errorCell(primary, secondary) {
  return `<div class="x-row" style="gap:10px"><div class="x-stack" style="gap:0"><span class="x-cell-primary x-truncate">${primary}</span>${secondary == null ? "" : `<span class="x-cell-secondary x-truncate">${h(secondary)}</span>`}</div></div>`;
}

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
      render: (group) => toolMeter(group.count, top, number(group.count), "var(--bad)"),
    },
  ];
}

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
    render: (row) =>
      `<span class="x-mono x-muted" title="${h(row.cwd)}">${h(row.project)}</span>`,
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

function errorModelList(models) {
  if (!models.length) return errorEmpty("No affected models");
  const max = models[0].count;
  return `<div class="x-bar-list">${models
    .slice(0, 12)
    .map((model) => {
      const width = max > 0 ? Math.max(0.5, (model.count / max) * 100) : 0;
      const color =
        model.key === errorModel
          ? "var(--bad)"
          : "color-mix(in srgb, var(--bad) 45%, transparent)";
      return `<button type="button" class="x-bar-list-row" data-error-model="${h(model.key)}" data-errors-focus="model-${encodeURIComponent(model.key)}" aria-pressed="${model.key === errorModel}"><span class="x-bar-list-fill" style="width:${width}%;background:${color}"></span><span class="x-bar-list-label"><span class="x-row" style="gap:6px;min-width:0"><span class="x-mono x-truncate">${h(model.model)}</span><span class="x-dim x-truncate">${h(model.provider)}</span></span></span><span class="x-bar-list-value">${number(model.count)}</span></button>`;
    })
    .join("")}</div>`;
}

function errorFilterChip(kind, label) {
  return `<button type="button" class="x-btn errors-filter" data-size="sm" data-error-clear="${kind}" data-errors-focus="clear-${kind}" title="${h(label)}"><span class="x-mono x-truncate">${h(label)}</span>${icon("close", 12)}</button>`;
}

function errorsPage() {
  if (!data) return '<div class="loading">Loading statistics…</div>';
  const rows = data.requests || [];
  const groups = errorGroups(rows);
  const models = errorModelGroups(rows);
  const newest = rows.reduce((found, row) => (!found || row.timestamp > found.timestamp ? row : found), null);
  // 刷新后已消失的筛选值不再生效，但状态保留：行重新出现时同名筛选自动恢复。
  const signature = groups.some((group) => group.signature === errorSignature) ? errorSignature : null;
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
  const complete = loaded < errorLimit;
  const selectedModel = models.find((item) => item.key === model);
  const filteredSignature = groups.find((group) => group.signature === signature);
  const window_label = descriptions[range];
  const stats = [
    toolStat(
      "Failures",
      number(loaded),
      complete ? `in the ${window_label}` : `latest ${number(loaded)} loaded`,
    ),
    toolStat("Signatures", number(groups.length), groups[0] ? `top: ${number(groups[0].count)} failures` : undefined, {
      title: "Distinct error messages after normalizing ids and numbers",
    }),
    toolStat("Affected models", number(models.length), models[0]?.model),
    toolStat(
      "Last failure",
      newest ? relative(newest.timestamp) : "–",
      newest ? localTime(newest.timestamp) : `none in the ${window_label}`,
    ),
  ].join("");
  const actions = `${signature === null ? "" : errorFilterChip("signature", signature)}${
    selectedModel ? errorFilterChip("model", selectedModel.model) : ""
  }<div class="x-search">${icon("search", 14)}<input class="x-input" id="search" type="search" aria-label="Filter" placeholder="Message, model or project" value="${h(errorQuery)}"></div>`;
  const footer = complete
    ? ""
    : `<span>Showing the latest ${number(loaded)} failures in the ${window_label}; older ones are not loaded.</span>${
        next === null
          ? ""
          : `<button type="button" class="x-btn" data-size="sm" data-errors-load>Load latest ${number(next)}</button>`
      }`;
  return `<div class="x-page"><header class="x-page-header x-rise"><div><h1 class="x-page-title">Errors</h1><p class="x-page-description">Failed model requests in the ${h(window_label)}, grouped by error signature. Open a failure for its full payload.</p></div></header><div class="x-stat-grid x-rise" style="--stat-min:170px">${stats}</div><div class="x-grid x-grid-main-side">${toolCard(
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
      empty: errorEmpty(`No failures in the ${window_label}`, "Every request succeeded or was aborted."),
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
      empty: errorEmpty(
        filteredSignature || selectedModel ? "No failures match" : `No failures in the ${window_label}`,
        filteredSignature || selectedModel || needle ? "Clear the search or the selected filters." : "",
      ),
    }),
    true,
    footer,
  )}</div>`;
}

function activityInput(e) {
  if (page !== "errors" || e.target.id !== "search") return false;
  errorQuery = e.target.value;
  errorFailureShown = ERROR_FAILURE_ROWS;
  render();
  return true;
}

function activityClick(e) {
  const clear = e.target.closest("[data-error-clear]");
  if (clear) {
    if (clear.dataset.errorClear === "signature") errorSignature = null;
    else errorModel = null;
    render();
    return true;
  }
  const header = e.target.closest("[data-errorsort]");
  if (header) {
    const table = header.dataset.errortable;
    const key = header.dataset.errorsort;
    const state = table === "signature" ? errorSignatureSort : errorFailureSort;
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
    render();
    return true;
  }
  const moreButton = e.target.closest("[data-errorpage]");
  if (moreButton) {
    const table = moreButton.dataset.errortable;
    const step = table === "signature" ? ERROR_SIGNATURE_ROWS : ERROR_FAILURE_ROWS;
    const grow = moreButton.dataset.errorpage === "more";
    if (table === "signature")
      errorSignatureShown = grow ? errorSignatureShown + step * 4 : step;
    else errorFailureShown = grow ? errorFailureShown + step * 4 : step;
    render();
    return true;
  }
  if (e.target.closest("[data-errors-load]")) {
    const index = ERROR_LIMITS.indexOf(errorLimit);
    if (index >= 0 && index + 1 < ERROR_LIMITS.length) {
      errorLimit = ERROR_LIMITS[index + 1];
      errorFailureShown = ERROR_FAILURE_ROWS;
      refresh();
    }
    return true;
  }
  const latest = e.target.closest("[data-error-latest]");
  if (latest) {
    openDrawer(Number(latest.dataset.errorLatest), latest);
    return true;
  }
  const signatureRow = e.target.closest("[data-error-signature]");
  if (signatureRow) {
    const signature = signatureRow.dataset.errorSignature;
    errorSignature = errorSignature === signature ? null : signature;
    render();
    return true;
  }
  const modelRow = e.target.closest("[data-error-model]");
  if (modelRow) {
    const key = modelRow.dataset.errorModel;
    errorModel = errorModel === key ? null : key;
    render();
    return true;
  }
  return false;
}
