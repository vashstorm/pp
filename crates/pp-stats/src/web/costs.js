/**
 * Costs page: API-equivalent cost estimates and rate coverage detail, grouped by model or billing
 * component.
 */

import {
  analyticsTable,
  breakdown,
  chartPanel,
  componentNames,
  costNote,
  costPerRequest,
  detailsGrid,
  groupRow,
  metric,
  numericCells,
  pricingNotice,
  segmentControls,
  seriesLegend,
  splitBar,
  stackedChart,
  tokenMix,
  topSeries,
} from "./analytics-shared.js";
import { descriptions, detailed, money, number, percent } from "./format.js";
import { app } from "./state.js";

/** @typedef {import("../../web-types/api.d.ts").AnalyticsResponse} AnalyticsResponse */

/**
 * Renders the Costs page.
 * @returns {string}
 */
export function costsPage() {
  const payload = /** @type {AnalyticsResponse | null} */ (app.data);
  if (!payload) return '<div class="loading">Loading statistics…</div>';
  const s = payload.summary;
  const groups = [...payload.models].sort((a, b) => (b.cost ?? -1) - (a.cost ?? -1));
  const top = groups.find((g) => g.priced);
  const series =
    app.costMode === "model"
      ? topSeries(groups, (g) => `${g.model} · ${g.key}`)
      : componentNames.map(([key, label, color]) => ({
          color,
          key,
          label,
          cost: s.cost_components?.[key],
          daily: payload.daily.map((b) => ({
            timestamp: b.timestamp,
            cost: b.cost_components?.[key],
          })),
          buckets: [],
        }));
  return `<h1>Costs</h1><p class="subtitle">What the ${descriptions[app.range]} of usage cost at the API rates recorded with each request. Estimates are not subscription bills.</p><div class="panel metrics">${metric("API-equivalent estimate", money(s.cost), `${number(s.requests)} requests`)}${metric("Average per day", payload.active_days && s.cost != null ? money(s.cost / payload.active_days) : "—", `over ${number(payload.active_days)} active UTC days`)}${metric("Top model", top?.model || "—", top ? `${money(top.cost)} · ${percent(top.cost, s.cost)} of estimate` : "")}${metric("Per priced request", costPerRequest(s), `${number(s.priced)} priced`)}${metric("Unpriced requests", number(s.unpriced), s.unpriced ? "excluded from estimate" : "all usage priced")}</div><div class="graphs">${chartPanel(
    "Daily estimate",
    "Per UTC day, stacked by model or billing component",
    stackedChart(series, "cost", false, true) + seriesLegend(series, "cost"),
    segmentControls(
      "cost-split",
      [
        ["model", "By model"],
        ["component", "By component"],
      ],
      app.costMode,
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
              detailed(
                g.cost_components?.cache_write,
                money(g.cost_components?.cache_write),
                " USD",
              ),
              costPerRequest(g),
              number(g.unpriced),
            ]),
          () =>
            `<div class="group-details"><h2>Rates · USD per million tokens</h2>${detailsGrid([
              ["Uncached input", money(g.rates?.input)],
              ["Cache read", money(g.rates?.cached_input)],
              ["Cache write", money(g.rates?.cache_write)],
              ["Output", money(g.rates?.output)],
              ["Pricing coverage", costNote(g)],
            ])}${tokenMix(g)}${breakdown(g)}</div>`,
          11,
        ),
      )
      .join(""),
  )}</section>${pricingNotice()}`;
}
