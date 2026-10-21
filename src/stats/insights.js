"use strict";
let toolFilter = "", insightSort = "cost";
function resetInsights() {
  toolFilter = "";
  insightSort = "cost";
}
const toolCoverage = (g) => `${number(g.priced)} priced · ${number(g.unpriced)} unpriced calls`;
const insightTrend = (g) => `<td class="trend">${sparkline(fillBuckets(g.buckets).map((b) => b.requests ?? 0), colors[0])}</td>`;
function insightRecent(rows, full = true) {
  if (full) return overviewTable(rows);
  return analyticsTable(["Invoking request", "Model", "When", "Request status"], rows.map((r) =>
    `<tr tabindex="0" data-id="${r.id}" aria-label="View request ${r.id}"><td>#${r.id} ${icon("arrow", 12)}</td><td>${h(r.model)}<div class="provider">${h(r.provider)}</div></td><td>${h(relative(r.timestamp))}</td><td class="right">${statusBadge(r, true)}</td></tr>`).join(""));
}
function toolModelDetails(g) {
  return `<div class="group-details">${detailsGrid([
    ["Call arguments", `${number(g.args_chars)} chars`],
    ["Usage coverage", `${number(g.usage_known)} / ${number(g.calls)} calls`],
    ["Pricing coverage", toolCoverage(g)],
    ["Attributed output", short(g.output_tokens)],
    ["Tool errors / duration", "Not recorded"],
  ])}<h2>Latest invoking requests</h2>${insightRecent(g.recent, false)}</div>`;
}
function toolModels(tool, scope = "breakdown") {
  return analyticsTable(["Model", "Calls", "Arguments", "Attr. tokens", "Attr. cost", "Last used"],
    [...tool.models].sort((a, b) => b.calls - a.calls).map((g) => groupRow(
      {...g, key: JSON.stringify([scope, tool.key, g.key]), description: `${g.provider} · ${g.key}`}, "tool-model",
      numericCells([number(g.calls), `${short(g.args_chars)} chars`, short(g.tokens), money(g.cost), relative(g.last_seen)]),
      () => toolModelDetails(g), 6,
    )).join(""));
}
function toolsPage() {
  if (!data) return '<div class="loading">Loading tools…</div>';
  const s = data.summary, q = query.trim().toLowerCase();
  const tools = [...data.tools].sort((a, b) => b.calls - a.calls);
  const rows = tools.filter((g) => !q || [g.tool, ...g.models.map((m) => m.key)].some((v) => v.toLowerCase().includes(q)));
  const series = topSeries(tools.map((g) => ({...g, requests:g.calls, daily:[]})), (g) => g.tool);
  const selected = tools.find((g) => g.key === toolFilter);
  return `<h1>Tools</h1><p class="subtitle">Tool calls emitted by pp in the ${descriptions[range]}, with usage attributed to the invoking model request.</p>
    <div class="panel metrics">${metric("Tool calls", number(s.calls), `${number(data.requests_with_tools)} invoking requests`)}${metric("Distinct tools", number(tools.length), tools[0] ? `Most used: ${tools[0].tool}` : "")}${metric("Attributed tokens", short(s.tokens), `${short(s.output_tokens)} output`)}${metric("Attributed cost", money(s.cost), toolCoverage(s))}${metric("Call arguments", `${short(s.args_chars)} chars`, s.calls ? `${short(Math.round(s.args_chars / s.calls))} chars per call` : "")}</div>
    ${chartPanel("Calls over time", "Top 6 tools and remaining calls stacked over time", stackedChart(series, "requests") + seriesLegend(series, "requests"))}
    <section class="panel analytics-panel"><div class="requests-heading panel-heading"><div><h2>By tool</h2><p class="panel-description">Select a row to expand model usage, coverage and recent requests.</p></div>${activitySearch("Search tools", "Tool or model…")}</div>${analyticsTable(
      ["Tool", "Calls", "Share", "Arguments", "Attr. tokens", "Attr. cost", "Last used", "Trend"],
      rows.slice(0, visible).map((g) => groupRow({...g, label:g.tool, description:`${g.models.length} model${g.models.length === 1 ? "" : "s"}`}, "tool",
        numericCells([number(g.calls), percent(g.calls, s.calls), `${short(g.args_chars)} chars`, short(g.tokens), money(g.cost), relative(g.last_seen)]) + insightTrend(g),
        () => `<div class="group-details">${detailsGrid([["Average arguments", `${short(Math.round(g.args_chars / g.calls))} chars / call`], ["Usage coverage", `${number(g.usage_known)} / ${number(g.calls)} calls`], ["Pricing coverage", toolCoverage(g)], ["Tool errors / results / duration", "Not recorded"]])}<h2>By model · select a model for recent requests</h2>${toolModels(g, "expanded")}</div>`, 8)).join(""),
    )}${activityFooter(rows)}</section>
    <section class="panel analytics-panel"><div class="requests-heading panel-heading"><div><h2>By tool and model</h2><p class="panel-description">Expand a model row to inspect the requests that emitted its calls.</p></div><select class="insight-select" id="tool-filter" aria-label="Filter by tool"><option value="">All tools</option>${tools.map((g) => `<option value="${h(g.key)}"${g.key === toolFilter ? " selected" : ""}>${h(g.tool)}</option>`).join("")}</select></div>${(selected ? [selected] : rows).map((g) => `<div class="tool-model-group"><h3>${h(g.tool)}</h3>${toolModels(g)}</div>`).join("") || '<div class="empty">No matching tools</div>'}</section>
    <p class="load-note">Tokens and estimated cost of each invoking request are split evenly across all its emitted tool calls. This is attribution, not a separate tool charge. Missing usage and prices remain unknown. Tool execution, errors, results and duration were not recorded. ${number(data.partial_requests)} invoking requests failed or were aborted; their saved calls may be partial or unexecuted.</p>${pricingNotice()}`;
}
function projectRank(projects, field, title, color) {
  const rows = [...projects].filter((g) => g[field] != null).sort((a, b) => b[field] - a[field]).slice(0, 6);
  const total = data.summary[field], max = Math.max(...rows.map((g) => g[field]), 1e-12);
  return chartPanel(title, field === "cost" ? "Share of priced API-equivalent estimate" : "Share of all requests",
    `<div class="insight-ranking">${rows.map((g) => `<button data-insight-open="${h(groupKey("project", g.key))}" title="${h(g.key)}" style="--rank-width:${100 * g[field] / max}%;--rank-color:${color}"><span>${h(g.project || g.key || "Unknown directory")}</span><span class="num">${field === "cost" ? money(g[field]) : number(g[field])} · ${percent(g[field], total)}</span></button>`).join("") || '<div class="empty">No data in this range</div>'}</div>`);
}
function projectDetails(g) {
  return `<div class="group-details"><div class="session-path">${h(g.key || "Working directory not recorded")}</div>${detailsGrid([
    ["Sessions", number(g.sessions)], ["Requests without session", number(g.requests_without_session)],
    ["Avg TTFT", duration(g.avg_ttft_ms)], ["Pricing coverage", costNote(g)],
    ["First active", localTime(g.first_seen)], ["Last active", localTime(g.last_seen)],
  ])}${tokenMix(g)}<h2>Models · select a row for usage and latency</h2>${analyticsTable(["Model", "Requests", "Tokens", "Cost", "Cache rate", "Errors"],
    [...g.models].sort((a, b) => b.requests - a.requests).map((m) => groupRow(
      {...m, key: JSON.stringify([g.key, m.key]), description:`${m.provider} · ${m.key}`}, "project-model",
      numericCells([number(m.requests), short(m.tokens), money(m.cost), cacheRate(m), percent(m.failed, m.requests)]),
      () => `<div class="group-details">${detailsGrid([["Avg duration", duration(m.avg_duration_ms)], ["Avg TTFT", duration(m.avg_ttft_ms)], ["Pricing coverage", costNote(m)], ["Aborted", number(m.aborted)]])}${tokenMix(m)}</div>`, 6,
    )).join(""))}<h2>Latest requests · up to 10 in this range</h2>${insightRecent(g.recent)}</div>`;
}
function projectsPage() {
  if (!data) return '<div class="loading">Loading projects…</div>';
  const s = data.summary, q = query.trim().toLowerCase();
  const rows = [...data.projects].filter((g) => !q || [g.project, g.key, ...g.models.map((m) => m.key)].some((v) => v.toLowerCase().includes(q)))
    .sort((a, b) => (b[insightSort] ?? -1) - (a[insightSort] ?? -1) || a.key.localeCompare(b.key));
  return `<h1>Projects</h1><p class="subtitle">Usage by working directory in the ${descriptions[range]}.</p><div class="panel metrics">${metric("Folders", number(data.projects.length))}${metric("Requests", number(s.requests), `${number(s.failed)} failed`)}${metric("API-equivalent cost", money(s.cost), costNote(s))}${metric("Conversation tokens", short(s.tokens))}${metric("Cache rate", cacheRate(s))}</div>
    <div class="graphs project-ranks">${projectRank(data.projects, "cost", "Top by cost", "var(--pink)")}${projectRank(data.projects, "requests", "Top by requests", "var(--accent)")}</div>
    <section class="panel analytics-panel"><div class="requests-heading panel-heading"><div><h2>Folders</h2><p class="panel-description">${number(rows.length)} folders · select a row for tokens, models and recent requests</p></div><div class="trace-controls">${activitySearch("Search folders", "Folder, path or model…")}<select class="insight-select" id="insight-sort" aria-label="Sort folders">${[["cost","Cost"],["requests","Requests"],["tokens","Tokens"],["last_seen","Last active"]].map(([key,label]) => `<option value="${key}"${key === insightSort ? " selected" : ""}>${label}</option>`).join("")}</select></div></div>${analyticsTable(
      ["Folder", "Requests", "Cost", "Tokens", "Cache rate", "Errors", "Avg duration", "Last active"],
      rows.slice(0, visible).map((g) => groupRow({...g, label:g.project || g.key || "Unknown directory", description:g.key}, "project",
        numericCells([number(g.requests), money(g.cost), short(g.tokens), cacheRate(g), percent(g.failed, g.requests), duration(g.avg_duration_ms), relative(g.last_seen)]),
        () => projectDetails(g), 8)).join(""),
    )}${activityFooter(rows)}</section><p class="load-note">Folders are grouped by the full recorded working directory. Different paths with the same folder name stay separate. All summaries cover every request in the selected range.</p>${pricingNotice()}`;
}
function insightsClick(e) {
  const target = e.target.closest("[data-insight-open]");
  if (!target) return false;
  const key = target.dataset.insightOpen;
  expandedGroups.add(key);
  query = "";
  visible = Math.max(100, data.projects.length);
  render();
  const row = [...$("page").querySelectorAll("[data-expand]")].find((el) => el.dataset.expand === key);
  row?.focus({preventScroll:true});
  row?.scrollIntoView({block:"center", behavior:"smooth"});
  return true;
}
function insightsChange(e) {
  if (!["tool-filter", "insight-sort"].includes(e.target.id)) return false;
  const id = e.target.id;
  if (id === "tool-filter") toolFilter = e.target.value;
  else insightSort = e.target.value;
  render();
  $(id)?.focus({preventScroll:true});
  return true;
}
