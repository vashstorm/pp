import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { Window } from "happy-dom";
import { toolView } from "../../src/insights.js";
import { fillBuckets } from "../../src/web/analytics-shared.js";
import { toolNiceScale } from "../../src/web/charts.js";
import { modelCacheSavings, modelLabels, modelPivot } from "../../src/web/models.js";
import type {
  ModelAggregate,
  ToolMetricRow,
  ToolRow,
  ToolsResponse,
} from "../../web-types/api.d.ts";

function makeModel(overrides: Partial<ModelAggregate>): ModelAggregate {
  return {
    key: "anthropic/claude-sonnet",
    model: "claude-sonnet",
    provider: "anthropic",
    rates: { input: 3, cached_input: 0.3, cache_write: 3.75, output: 15 },
    requests: 2,
    ok: 2,
    failed: 0,
    aborted: 0,
    input_tokens: 1_000_000,
    output_tokens: 100_000,
    cached_tokens: 800_000,
    uncached_tokens: 200_000,
    tokens: 1_100_000,
    usage_known: 2,
    cache_known: 2,
    cache_write_known: 2,
    cache_write_tokens: 0,
    avg_duration_ms: 500,
    avg_ttft_ms: 100,
    tokens_per_second: 50,
    first_seen: 1000,
    last_seen: 2000,
    priced: 2,
    unpriced: 0,
    cost: 2.34,
    cost_components: { input: 0.6, cache_read: 0.24, cache_write: 0, output: 1.5 },
    buckets: [],
    daily: [],
    hours: [],
    models: 1,
    ...overrides,
  };
}

const NOW = 1_700_000_000_000;

function toolMetric(calls: number, tokens: number | null, usageKnown: number): ToolMetricRow {
  return {
    calls,
    args_chars: calls * 10,
    tokens,
    output_tokens: tokens == null ? null : Math.round(tokens / 5),
    usage_known: usageKnown,
    cost: 0.5,
    priced: calls,
    unpriced: 0,
    last_seen: NOW,
    errors: 0,
    cancelled: 0,
    executions: calls,
    result_chars: calls * 4,
    result_calls: calls,
    avg_duration_ms: 5,
    buckets: [],
    error_buckets: [],
    recent: [],
  };
}

function toolRow(tool: string, calls: number, tokens: number | null, usageKnown: number): ToolRow {
  return {
    ...toolMetric(calls, tokens, usageKnown),
    key: tool,
    tool,
    models: [
      {
        ...toolMetric(calls, tokens, usageKnown),
        key: "anthropic/claude-sonnet",
        model: "claude-sonnet",
        provider: "anthropic",
      },
    ],
  };
}

function toolsResponse(rows: ToolRow[]): ToolsResponse {
  const calls = rows.reduce((sum, row) => sum + row.calls, 0);
  const knownTokens = rows.flatMap((row) => (row.tokens == null ? [] : [row.tokens]));
  const knownOutput = rows.flatMap((row) => (row.output_tokens == null ? [] : [row.output_tokens]));
  return {
    summary: {
      ...toolMetric(
        calls,
        null,
        rows.reduce((sum, row) => sum + row.usage_known, 0),
      ),
      tokens: knownTokens.length ? knownTokens.reduce((sum, value) => sum + value, 0) : null,
      output_tokens: knownOutput.length ? knownOutput.reduce((sum, value) => sum + value, 0) : null,
    },
    tools: rows,
    requests_with_tools: 0,
    partial_requests: 0,
    updated_at: NOW,
    range: "24h",
  };
}

let happyWindow: Window | null = null;

describe("tools view keeps unknown usage unknown", () => {
  beforeAll(() => {
    happyWindow = new Window({ url: "http://127.0.0.1:3848/" });
    (globalThis as Record<string, unknown>).document = happyWindow.document;
  });

  afterAll(() => {
    happyWindow?.close();
    happyWindow = null;
  });

  test("all-unknown usage keeps tokens null and never reports a zero share", () => {
    const view = toolView(
      toolsResponse([toolRow("read", 3, null, 0), toolRow("bash", 1, null, 0)]),
    );
    expect(view.totals.tokens).toBeNull();
    expect(view.totals.output).toBeNull();
    expect(view.totals.usageKnown).toBe(0);
    expect(view.totals.tokenCoverage).toBeNull();
    for (const row of view.rows) {
      expect(row.totalTokensShare).toBeNull();
      expect(row.outputTokensShare).toBeNull();
      expect(row.tokenFraction).toBeNull();
    }
  });

  test("partially known usage sums known tokens only and reports known-call coverage", () => {
    const view = toolView(
      toolsResponse([toolRow("read", 3, 3000, 3), toolRow("bash", 1, null, 0)]),
    );
    expect(view.totals.tokens).toBe(3000);
    expect(view.totals.output).toBe(600);
    expect(view.totals.usageKnown).toBe(3);
    expect(view.totals.tokenCoverage).toBeCloseTo(0.75, 6);
    const readRow = view.rows.find((row) => row.tool === "read");
    const bashRow = view.rows.find((row) => row.tool === "bash");
    expect(readRow?.tokenFraction).toBe(1);
    expect(bashRow?.tokenFraction).toBeNull();
    expect(bashRow?.totalTokensShare).toBeNull();
  });

  test("all-known usage keeps summed totals and full coverage", () => {
    const view = toolView(
      toolsResponse([toolRow("read", 2, 2000, 2), toolRow("bash", 2, 1000, 2)]),
    );
    expect(view.totals.tokens).toBe(3000);
    expect(view.totals.output).toBe(600);
    expect(view.totals.tokenCoverage).toBe(1);
    for (const row of view.rows) expect(row.tokenFraction).not.toBeNull();
  });

  test("an empty tools response keeps the existing zero baseline", () => {
    const view = toolView(toolsResponse([]));
    expect(view.totals.calls).toBe(0);
    expect(view.totals.tools).toBe(0);
    expect(view.totals.tokens).toBe(0);
    expect(view.totals.output).toBe(0);
    expect(view.totals.tokenCoverage).toBeNull();
  });
});

describe("models & insights analytics calculations", () => {
  test("modelCacheSavings returns null unless all requests are priced with consistent rates and cache-write usage", () => {
    const valid = makeModel({});
    const savings = modelCacheSavings(valid);
    expect(savings).not.toBeNull();
    // counterfactual = 1_000_000 * 3 / 1e6 = 3.0; actual input-side = 0.6 + 0.24 + 0 = 0.84 -> 72% saved
    expect(savings ?? 0).toBeCloseTo(0.72, 6);

    expect(modelCacheSavings(makeModel({ rates: null }))).toBeNull();
    expect(modelCacheSavings(makeModel({ priced: 1 }))).toBeNull();
    expect(modelCacheSavings(makeModel({ cache_write_known: 1 }))).toBeNull();
    expect(modelCacheSavings(makeModel({ input_tokens: 0 }))).toBeNull();
  });

  test("modelLabels qualifies duplicate model names with provider only when needed", () => {
    const labels = modelLabels([
      makeModel({ model: "shared", provider: "p1" }),
      makeModel({ model: "shared", provider: "p2" }),
      makeModel({ model: "unique", provider: "p1" }),
    ]);
    expect(labels.get("shared::p1")).toBe("shared · p1");
    expect(labels.get("shared::p2")).toBe("shared · p2");
    expect(labels.get("unique::p1")).toBe("unique");
  });

  test("modelPivot collapses series beyond limit into __other__", () => {
    const buckets = [1000, 2000];
    const points = [
      { timestamp: 1000, key: "a::p", value: 10 },
      { timestamp: 1000, key: "b::p", value: 5 },
      { timestamp: 2000, key: "c::p", value: 2 },
    ];
    const colors = new Map([
      ["a::p", "#111"],
      ["b::p", "#222"],
      ["c::p", "#333"],
    ]);
    const labels = new Map([
      ["a::p", "a"],
      ["b::p", "b"],
      ["c::p", "c"],
    ]);
    const series = modelPivot(points, buckets, colors, labels, 1);
    expect(series.length).toBe(2);
    expect(series[0]?.key).toBe("a::p");
    expect(series[1]?.key).toBe("__other__");
    expect(series[1]?.values).toEqual([5, 2]);
  });

  test("toolNiceScale and fillBuckets produce contiguous monotonic axes", () => {
    const scale = toolNiceScale(93, 0);
    expect(scale.min).toBe(0);
    expect(scale.max).toBeGreaterThanOrEqual(93);
    expect(scale.ticks.length).toBeGreaterThanOrEqual(2);

    const now = 1_700_000_400_000;
    const filled = fillBuckets(
      [{ timestamp: Math.floor((now - 1_800_000) / 300_000) * 300_000 }],
      "1h",
      now,
    );
    expect(filled.length).toBeGreaterThanOrEqual(12);
    for (let i = 1; i < filled.length; i++) {
      expect((filled[i]?.timestamp ?? 0) - (filled[i - 1]?.timestamp ?? 0)).toBe(300_000);
    }
  });
});
