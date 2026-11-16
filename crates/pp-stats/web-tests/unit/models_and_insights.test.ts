import { describe, expect, test } from "bun:test";
import { fillBuckets } from "../../src/web/analytics-shared.js";
import { toolNiceScale } from "../../src/web/charts.js";
import { modelCacheSavings, modelLabels, modelPivot } from "../../src/web/models.js";
import type { ModelAggregate } from "../../web-types/api.d.ts";

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
