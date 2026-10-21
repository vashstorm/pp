"use strict";
let errorSignature = "",
  errorModel = "",
  traceRequest = null,
  traceFocusPending = false;
const traceExpanded = new Set(),
  traceSections = new Set(),
  traceCache = new Map(),
  tracePending = new Map();
let traceEpoch = 0;
function resetActivity(url) {
  errorSignature = "";
  errorModel = "";
  const requested = url.searchParams.get("request");
  const session = url.searchParams.get("session");
  traceRequest = /^[1-9]\d*$/.test(requested || session || "")
    ? requested || session
    : null;
  traceFocusPending = Boolean(requested && traceRequest);
  traceExpanded.clear();
  if (traceFocusPending) traceExpanded.add(Number(traceRequest));
  traceSections.clear();
  traceCache.clear();
  tracePending.clear();
  traceEpoch++;
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
              `<tr tabindex="0" data-id="${r.id}" aria-label="View failed ${h(r.model)} request ${r.id}"><td title="${h(localTime(r.timestamp))}">${relative(r.timestamp)}</td><td><div class="model-name">${h(r.model)}</div><div class="provider">${h(r.provider)}</div></td><td><div class="failure-message" title="${h(r.error_message)}">${h(r.error_message || "Unknown error")}</div></td><td class="project" title="${h(r.cwd)}">${h(r.project)}</td><td class="right num">${short(total(r))}</td><td class="right num">${money(r.cost)}</td></tr>`,
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
function sessionTitle(s) {
  return s.session_file
    ? s.session_file
        .split(/[\\/]/)
        .pop()
        .replace(/\.jsonl$/, "")
    : `Request #${s.id} · No session recorded`;
}
function openTrace(id) {
  location.hash = `/traces?${new URLSearchParams({ range, session: String(id) })}`;
}
function tracesPage() {
  if (!data) return '<div class="loading">Loading traces…</div>';
  if (traceRequest) return tracePage();
  const q = query.trim().toLowerCase();
  const rows = data.sessions.filter(
    (s) =>
      !q ||
      [s.project, s.cwd, s.session_file, ...s.models.map((m) => m.key)].some(
        (v) =>
          String(v || "")
            .toLowerCase()
            .includes(q),
      ),
  );
  return `<h1>Traces</h1><p class="subtitle">Recorded model requests grouped by session in the ${descriptions[range]}. Open a session to inspect its timeline.</p><section class="panel"><div class="requests-heading panel-heading"><div><h2>Sessions</h2><p class="panel-description">${number(data.sessions.length)} of ${number(data.total)} sessions · metrics cover requests in this range</p></div>${activitySearch("Search sessions", "Session, model or project…")}</div><div class="table-wrap"><table class="session-table"><thead><tr><th>Session</th><th>Models</th><th>Started</th><th class="right">Wall time</th><th class="right">Requests</th><th class="right">Failed</th><th class="right">Tokens</th><th class="right">Cost</th></tr></thead><tbody>${
    rows
      .slice(0, visible)
      .map(
        (s) =>
          `<tr tabindex="0" data-trace="${s.id}" aria-label="Open session ${h(sessionTitle(s))}"><td><div class="session-name" title="${h(s.session_file || sessionTitle(s))}">${icon("chevron", 12)} ${h(sessionTitle(s))}</div><div class="provider">${h(s.project)}</div></td><td title="${h(s.models.map((m) => m.key).join(", "))}">${h(s.models[0]?.model)}${s.models.length > 1 ? ` +${s.models.length - 1}` : ""}</td><td title="${h(localTime(s.first_seen))}">${relative(s.first_seen)}</td><td class="right num">${duration(s.wall_ms)}</td><td class="right num">${number(s.requests)}</td><td class="right num ${s.failed ? "failure-count" : ""}">${number(s.failed)}</td><td class="right num">${short(s.tokens)}</td><td class="right num" title="${s.unpriced} unpriced">${money(s.cost)}</td></tr>`,
      )
      .join("") ||
    '<tr><td colspan="8"><div class="empty">No matching sessions</div></td></tr>'
  }</tbody></table></div>${activityFooter(rows)}</section>${activityLimit("sessions")}${pricingNotice(data)}`;
}
function traceSection(id, key, label, value, plain = false) {
  const section = `${id}-${key}`,
    open = traceSections.has(section);
  return `<details class="trace-section" data-trace-section="${h(section)}"${open ? " open" : ""}><summary>${label}</summary>${value == null || value === "" ? '<div class="unavailable">Not recorded for this request.</div>' : `<pre><code>${h(plain ? value : JSON.stringify(value, null, 2))}</code></pre>`}</details>`;
}
function traceToolCalls(detail) {
  const content = (detail.session_entry || []).flatMap(
    (e) => e.message?.content || [],
  );
  const saved = content.filter((c) => c.type === "toolCall");
  if (saved.length) return saved;
  const native = detail.output_message?.native;
  if (native?.tool_calls)
    return native.tool_calls.map((c) => ({
      id: c.id,
      name: c.function?.name,
      arguments: c.function?.arguments,
    }));
  const items = Array.isArray(native) ? native : native?.output || [];
  return items.filter((c) => c.type === "function_call");
}
function traceRequestContent(r) {
  const detail = traceCache.get(r.id);
  if (!detail) return '<div class="loading">Loading request content…</div>';
  if (detail.error)
    return `<div class="empty">${h(detail.error)} <button class="outline-button" data-retry-trace="${r.id}">Retry</button></div>`;
  const output = detail.output_message || {},
    calls = traceToolCalls(detail);
  const content = (detail.session_entry || []).flatMap(
    (e) => e.message?.content || [],
  );
  const text =
    output.text ||
    content
      .filter((c) => c.type === "text")
      .map((c) => c.text)
      .join("\n");
  const thinking =
    output.thinking ||
    content
      .filter((c) => c.type === "thinking")
      .map((c) => c.thinking)
      .join("\n");
  return `<div class="trace-request-meta">${detailsGrid([
    ["Started", localTime(r.timestamp)],
    ["Duration", duration(r.duration_ms)],
    ["TTFT", duration(r.ttft_ms)],
    ["Tokens", number(total(r))],
    ["Purpose", r.purpose],
    ["Cost", money(r.cost)],
  ])}<button class="outline-button" data-id="${r.id}">Full request details ${icon("arrow", 12)}</button></div>${r.error_message ? `<div class="detail-error">${h(r.error_message)}</div>` : ""}${traceSection(r.id, "text", "Assistant output", text, true)}${traceSection(r.id, "thinking", "Thinking", thinking, true)}${calls.length ? `<div class="detail-title">Tool calls · arguments only</div>${calls.map((c, i) => traceSection(r.id, `tool-${i}`, `${h(c.name || "Tool")} · ${h(c.id || c.call_id || i + 1)}`, c.arguments)).join("")}` : ""}${traceSection(r.id, "native", "Native output JSON", output)}${traceSection(r.id, "entry", "Session entries JSON", detail.session_entry)}`;
}
function tracePage() {
  const s = data.summary,
    all = data.requests,
    start = s.first_seen,
    wall = Math.max(s.wall_ms, 1),
    q = query.trim().toLowerCase();
  const rows = all.filter(
    (r) =>
      !q ||
      [r.model, r.provider, r.purpose, r.status, r.error_message, r.id].some(
        (v) =>
          String(v || "")
            .toLowerCase()
            .includes(q),
      ),
  );
  return `<div class="trace-heading"><div><button class="inline-link" data-sessions>← Sessions</button><h1>${h(sessionTitle(data))}</h1><p class="subtitle">${h(data.cwd)}${data.session_file ? ` · ${h(data.session_file)}` : ""}</p></div><button class="outline-button" data-refresh-trace>Refresh trace</button></div><div class="panel metrics request-metrics">${metric("Wall time", duration(s.wall_ms), "First request to last completion")}${metric("Model time", duration(s.model_ms), "Sum of recorded model calls")}${metric("Requests", number(s.requests), `${s.failed} failed · ${s.aborted} aborted`)}${metric("Models", number(s.models.length))}${metric("Tokens", short(s.tokens))}${metric("Cost", money(s.cost), `${s.unpriced} unpriced`)}</div><section class="panel trace-panel"><div class="requests-heading panel-heading"><div><h2>Timeline</h2><p class="panel-description">Full recorded session · select a bar to expand its request</p></div></div><div class="trace-timeline"><div class="timeline-axis"><span>0s</span><span>${duration(wall / 2)}</span><span>${duration(wall)}</span></div>${s.models
    .map(
      (m) =>
        `<div class="timeline-lane"><div class="timeline-label" title="${h(m.key)}">${h(m.model)}<span class="provider">${h(m.provider)}</span></div><div class="timeline-track">${all
          .filter((r) => r.model_reference === m.key)
          .map(
            (r) =>
              `<button class="timeline-span ${r.status}${traceExpanded.has(r.id) ? " active" : ""}" style="left:${Math.min(99.5, (100 * (r.timestamp - start)) / wall)}%;width:${Math.max(0.5, Math.min(100, (100 * r.duration_ms) / wall))}%" data-timeline-request="${r.id}" aria-label="Expand request ${r.id}, ${h(r.model)}, ${h(r.status)}, ${duration(r.duration_ms)}" title="#${r.id} · ${h(r.model)} · ${duration(r.duration_ms)} · ${h(r.status)}"></button>`,
          )
          .join("")}</div></div>`,
    )
    .join(
      "",
    )}<div class="timeline-legend"><span><i class="ok"></i>OK</span><span><i class="failed"></i>Failed</span><span><i class="aborted"></i>Aborted</span></div></div></section><section class="panel"><div class="requests-heading panel-heading"><div><h2>Transcript</h2><p class="panel-description">${number(rows.length)} recorded requests · expand each request and its content sections</p></div><div class="trace-controls">${activitySearch("Search trace requests", "Model, status or request ID…")}<button class="outline-button" data-expand-trace>Expand all</button><button class="outline-button" data-collapse-trace>Collapse all</button></div></div><div class="trace-transcript">${
    rows
      .slice(0, visible)
      .map((r) => {
        const open = traceExpanded.has(r.id);
        return `<article class="trace-request" id="trace-request-${r.id}"><button class="trace-request-toggle" data-trace-request="${r.id}" aria-expanded="${open}" aria-controls="trace-content-${r.id}">${icon(open ? "down" : "chevron", 12)}<span class="trace-kind">model</span><span class="trace-request-name">${h(r.model)}<span class="provider">${h(r.provider)} · ${h(r.purpose)} · #${r.id}</span></span>${statusBadge(r)}<span class="num">${duration(r.duration_ms)}</span><span class="provider num">+${duration(r.timestamp - start)}</span></button><div class="trace-content" id="trace-content-${r.id}"${open ? "" : " hidden"}>${open ? traceRequestContent(r) : ""}</div></article>`;
      })
      .join("") || '<div class="empty">No matching requests</div>'
  }</div>${activityFooter(rows)}</section><p class="load-note">This trace uses saved statistics only. User prompts, tool results, tool duration and subagent spans were not recorded.</p>${pricingNotice(data)}`;
}
async function loadTraceRequest(id) {
  if (traceCache.has(id) || tracePending.has(id)) return;
  const epoch = traceEpoch;
  const task = (async () => {
    try {
      const detail = await getJSON(`/api/stats/requests/${id}`);
      if (epoch === traceEpoch) traceCache.set(id, detail);
    } catch (error) {
      if (epoch === traceEpoch) traceCache.set(id, { error: error.message });
    } finally {
      if (epoch === traceEpoch) {
        tracePending.delete(id);
        if (page === "traces" && traceRequest) render();
      }
    }
  })();
  tracePending.set(id, task);
  return task;
}
function activityClick(e) {
  const target = e.target.closest(
    "[data-error-signature],[data-error-model],[data-clear-errors],[data-trace],[data-sessions],[data-trace-request],[data-timeline-request],[data-expand-trace],[data-collapse-trace],[data-retry-trace],[data-refresh-trace]",
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
  } else if ("clearErrors" in d) {
    errorSignature = "";
    errorModel = "";
    render();
  } else if (d.trace) openTrace(d.trace);
  else if ("sessions" in d) navigate("traces");
  else if ("refreshTrace" in d) {
    traceCache.clear();
    refresh();
  } else if ("expandTrace" in d) {
    for (const r of data.requests
      .filter(
        (r) =>
          !query.trim() ||
          [
            r.model,
            r.provider,
            r.purpose,
            r.status,
            r.error_message,
            r.id,
          ].some((v) =>
            String(v || "")
              .toLowerCase()
              .includes(query.trim().toLowerCase()),
          ),
      )
      .slice(0, visible)) {
      traceExpanded.add(r.id);
      loadTraceRequest(r.id);
    }
    render();
  } else if ("collapseTrace" in d) {
    traceExpanded.clear();
    traceSections.clear();
    render();
  } else {
    const id = Number(d.traceRequest || d.timelineRequest || d.retryTrace);
    if (d.retryTrace) traceCache.delete(id);
    if (d.timelineRequest) {
      query = "";
      visible = Math.max(
        visible,
        data.requests.findIndex((r) => r.id === id) + 1,
      );
    }
    if (traceExpanded.has(id) && d.traceRequest) traceExpanded.delete(id);
    else {
      traceExpanded.add(id);
      loadTraceRequest(id);
    }
    render();
    if (d.timelineRequest)
      $(`trace-request-${id}`)?.scrollIntoView({
        behavior: "smooth",
        block: "center",
      });
  }
  return true;
}
