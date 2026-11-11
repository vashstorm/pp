"use strict";
let errorSignature = "",
  errorModel = "";
function resetActivity() {
  errorSignature = "";
  errorModel = "";
}
function activitySearch(label, placeholder) {
  return `<div class="search">${icon("search")}<input id="search" type="search" aria-label="${label}" placeholder="${placeholder}" value="${h(query)}"></div>`;
}
function activityFooter(rows) {
  return `<div class="table-footer">${number(Math.min(visible, rows.length))} of ${number(rows.length)}${visible < rows.length ? '<button class="outline-button" data-more>Show more</button>' : ""}</div>`;
}
function activityLimit(label) {
  return data.has_more
    ? `<div class="load-note"><span>Loaded the latest ${number(data.limit)} ${label}. Search and filters apply to this loaded list.</span>${limit === 500 ? '<button class="outline-button" data-load>Load latest 2,000</button>' : ""}</div>`
    : "";
}
function failureTable(rows) {
  return `<div class="table-wrap"><table class="failure-table"><thead><tr><th>When</th><th>Model</th><th>Error</th><th>Project</th><th class="right">Tokens</th><th class="right">Cost</th></tr></thead><tbody>${
    rows.length
      ? rows
          .slice(0, visible)
          .map(
            (r) =>
              `<tr tabindex="0" data-id="${r.id}" aria-label="View failed ${h(r.model)} request ${r.id}"><td title="${h(localTime(r.timestamp))}">${relative(r.timestamp)}</td><td><div class="model-name">${h(r.model)}</div><div class="provider">${h(r.provider)}</div></td><td><div class="failure-message" title="${h(r.error_message)}">${h(r.error_message || "Unknown error")}</div></td><td class="project" title="${h(r.cwd)}">${h(r.project)}</td><td class="right num">${detailed(total(r))}</td><td class="right num">${money(r.cost)}</td></tr>`,
          )
          .join("")
      : '<tr><td colspan="6"><div class="empty">No matching failures</div></td></tr>'
  }</tbody></table></div>`;
}
function errorsPage() {
  if (!data) return '<div class="loading">Loading errors…</div>';
  const s = data.summary,
    q = query.trim().toLowerCase();
  const rows = data.requests.filter(
    (r) =>
      (!errorSignature || r.signature === errorSignature) &&
      (!errorModel || r.model_reference === errorModel) &&
      (!q ||
        [r.model, r.provider, r.project, r.cwd, r.error_message].some((v) =>
          String(v || "")
            .toLowerCase()
            .includes(q),
        )),
  );
  return `<h1>Errors</h1><p class="subtitle">Failed model requests in the ${descriptions[range]}, grouped by error signature. Open a failure for its full payload.</p><div class="panel metrics error-metrics">${metric("Failures", number(s.requests), `in the ${descriptions[range]}`)}${metric("Signatures", number(data.signatures.length), `top: ${number(data.signatures[0]?.requests || 0)} failures`)}${metric("Affected models", number(data.models.length))}${metric("Last failure", s.last_seen ? relative(s.last_seen) : "—", s.last_seen ? localTime(s.last_seen) : "No failures recorded")}</div><div class="error-layout"><section class="panel"><div class="requests-heading"><h2>Error signatures</h2><p class="panel-description">IDs and counters are normalized. Expand a signature to inspect it and filter the failures below.</p></div><div class="table-wrap"><table class="signature-table"><thead><tr><th>Signature</th><th>Models</th><th>Last seen</th><th class="right">Failures</th></tr></thead><tbody>${
    data.signatures
      .map((g, i) => {
        const key = `error:${g.signature}`,
          open = expandedGroups.has(key);
        return `<tr class="group-row${errorSignature === g.signature ? " selected" : ""}" tabindex="0" data-error-signature="${i}" aria-expanded="${open}" aria-controls="signature-${i}" aria-label="${open ? "Collapse" : "Expand"} error signature ${i + 1}"><td><div class="signature-label">${icon(open ? "down" : "chevron", 12)}<span>${h(g.signature)}</span></div></td><td title="${h(g.models.map((m) => m.key).join(", "))}">${h(g.models[0]?.model)}${g.models.length > 1 ? ` +${g.models.length - 1}` : `<div class="provider">${h(g.models[0]?.provider)}</div>`}</td><td>${relative(g.last_seen)}</td><td class="right num">${number(g.requests)}</td></tr><tr class="expanded-detail" id="signature-${i}"${open ? "" : " hidden"}><td colspan="4">${open ? `<div class="group-details"><div class="detail-title">Normalized signature</div><pre class="error-example">${h(g.signature)}</pre><div class="detail-title">Latest example</div><pre class="error-example">${h(g.example || "Unknown error")}</pre><p class="panel-description">${h(g.models.map((m) => m.key).join(" · "))} · First seen ${h(localTime(g.first_seen))}</p></div>` : ""}</td></tr>`;
      })
      .join("") ||
    '<tr><td colspan="4"><div class="empty">No failures in this range</div></td></tr>'
  }</tbody></table></div></section><section class="panel error-models"><div class="requests-heading"><h2>By model</h2><p class="panel-description">Select a model to filter.</p></div>${data.models.map((m, i) => `<button class="error-model${errorModel === m.key ? " selected" : ""}" data-error-model="${i}" aria-pressed="${errorModel === m.key}"><span><span class="model-name">${h(m.model)}</span><span class="provider">${h(m.provider)} · ${h(m.key)}</span></span><span class="num">${number(m.failures)}</span></button>`).join("") || '<div class="empty">No affected models</div>'}</section></div><section class="panel"><div class="requests-heading panel-heading"><div><h2>Failures</h2><p class="panel-description">${number(rows.length)} of ${number(data.requests.length)} loaded failures, newest first${errorSignature || errorModel ? ' · <button class="inline-link" data-clear-errors>Clear filters</button>' : ""}</p></div>${activitySearch("Search failures", "Error, model or project…")}</div>${failureTable(rows)}${activityFooter(rows)}</section>${activityLimit("failures; signature and model totals cover the entire range")}${pricingNotice(data)}`;
}
function toolSignatureTable(signatures) {
  return `<div class="table-wrap"><table class="signature-table"><thead><tr><th>Tool</th><th>Signature</th><th>Last seen</th><th class="right">Failures</th></tr></thead><tbody>${
    signatures.length
      ? signatures
          .map(
            (g) =>
              `<tr tabindex="0" aria-label="Tool error signature in ${h(g.tool)}"><td><div class="model-name">${h(g.tool)}</div></td><td><div class="failure-message" title="${h(g.signature)}">${h(g.signature)}</div></td><td title="${h(localTime(g.last_seen))}">${relative(g.last_seen)}</td><td class="right num">${number(g.failures)}</td></tr>`,
          )
          .join("")
      : '<tr><td colspan="4"><div class="empty">No tool failures in this range</div></td></tr>'
  }</tbody></table></div>`;
}
function toolCounts(signatures) {
  const counts = new Map();
  for (const g of signatures) counts.set(g.tool, (counts.get(g.tool) || 0) + g.failures);
  const rows = [...counts].sort((a, b) => b[1] - a[1]);
  return `<div class="table-wrap"><table><thead><tr><th>Tool</th><th class="right">Failures</th></tr></thead><tbody>${
    rows.length
      ? rows
          .map(([tool, failures]) => `<tr><td><div class="model-name">${h(tool)}</div></td><td class="right num">${number(failures)}</td></tr>`)
          .join("")
      : '<tr><td colspan="2"><div class="empty">No affected tools</div></td></tr>'
  }</tbody></table></div>`;
}
function toolFailureTable(rows) {
  return `<div class="table-wrap"><table class="failure-table"><thead><tr><th>When</th><th>Tool</th><th>Error</th><th>Request</th><th class="right">Duration</th></tr></thead><tbody>${
    rows.length
      ? rows
          .map(
            (r) =>
              `<tr tabindex="0"${r.request_id ? ` data-id="${r.request_id}" aria-label="View request ${r.request_id}"` : ""}><td title="${h(localTime(r.timestamp))}">${relative(r.timestamp)}</td><td><div class="model-name">${h(r.tool)}</div><div class="provider">${h(r.call_id)}</div></td><td><div class="failure-message" title="${h(r.error_message)}">${h(r.error_message || "Unknown tool error")}</div></td><td class="project" title="${h(r.cwd)}">${r.request_id ? `#${r.request_id}<div class="provider">${h(r.project || "")}</div>` : '<span class="provider">Not linked to a request</span>'}</td><td class="right num">${detailed(r.duration_ms, duration(r.duration_ms), " ms")}</td></tr>`,
          )
          .join("")
      : '<tr><td colspan="5"><div class="empty">No tool failures recorded</div></td></tr>'
  }</tbody></table></div>`;
}
function toolFailuresPanel() {
  if (!data) return "";
  const s = data.tool_summary || {},
    rows = data.tool_failures || [],
    signatures = data.tool_signatures || [];
  const more = data.tool_has_more
    ? `<p class="load-note">Loaded the latest ${number(data.tool_limit)} tool failures; narrow the time range to see older ones.</p>`
    : "";
  return `<div class="panel metrics error-metrics">${metric("Tool failures", number(s.failures), `in the ${descriptions[range]}`)}${metric("Tool signatures", number(s.signatures), `top: ${number(signatures[0]?.failures || 0)} failures`)}${metric("Affected tools", number(s.tools))}${metric("Last tool failure", s.last_seen ? relative(s.last_seen) : "—", s.last_seen ? localTime(s.last_seen) : "No tool failures recorded")}</div><div class="error-layout"><section class="panel"><div class="requests-heading"><h2>Tool error signatures</h2><p class="panel-description">Recorded tool executions that returned an error, grouped by tool and normalized message. Only executions collected after this pp version are counted.</p></div>${toolSignatureTable(signatures)}</section><section class="panel error-models"><div class="requests-heading"><h2>Tools</h2><p class="panel-description">Failed executions per tool.</p></div>${toolCounts(signatures)}</section></div><section class="panel"><div class="requests-heading panel-heading"><div><h2>Tool failures</h2><p class="panel-description">${number(rows.length)} loaded tool failures, newest first</p></div></div>${toolFailureTable(rows)}${more}</section>`;
}
function activityClick(e) {
  const target = e.target.closest(
    "[data-error-signature],[data-error-model],[data-clear-errors]",
  );
  if (!target) return false;
  const d = target.dataset;
  if (d.errorSignature !== undefined) {
    const i = Number(d.errorSignature),
      sig = data.signatures[i].signature,
      key = `error:${sig}`;
    if (expandedGroups.has(key)) {
      expandedGroups.delete(key);
      if (errorSignature === sig) errorSignature = "";
    } else {
      expandedGroups.add(key);
      errorSignature = sig;
    }
    visible = 100;
    render();
  } else if (d.errorModel !== undefined) {
    const key = data.models[Number(d.errorModel)].key;
    errorModel = errorModel === key ? "" : key;
    visible = 100;
    render();
  } else {
    errorSignature = "";
    errorModel = "";
    render();
  }
  return true;
}
