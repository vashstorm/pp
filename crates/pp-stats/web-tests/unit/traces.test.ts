import { describe, expect, test } from "bun:test";
import { traceSignature, traceViewModel } from "../../src/traces.js";
import {
  traceCompressed,
  traceEqualWidth,
  traceLayout,
  traceScale,
  traceTicks,
} from "../../src/web/traces-scale.js";
import type { TraceDetailResponse } from "../../web-types/api.d.ts";

function sampleTrace(): TraceDetailResponse {
  return {
    id: 10,
    session_file: "/tmp/s1.jsonl",
    project: "pp",
    cwd: "/tmp/pp",
    title: "Test trace",
    summary: {
      requests: 2,
      ok: 2,
      failed: 0,
      aborted: 0,
      input_tokens: 100,
      output_tokens: 50,
      cached_tokens: 20,
      uncached_tokens: 80,
      tokens: 150,
      usage_known: 2,
      cache_known: 2,
      cache_write_known: 2,
      cache_write_tokens: 0,
      avg_duration_ms: 500,
      avg_ttft_ms: 100,
      tokens_per_second: 50,
      first_seen: 1000,
      last_seen: 60_000,
      priced: 2,
      unpriced: 0,
      cost: 0.02,
      cost_components: { input: 0.01, cache_read: 0, cache_write: 0, output: 0.01 },
      wall_ms: 60_000,
      model_ms: 1_000,
      tool_ms: 200,
      tool_calls: 1,
      idle_ms: 58_800,
      agents: 1,
      models: [{ key: "anthropic/claude", model: "claude", provider: "anthropic" }],
    },
    requests: [
      {
        id: 10,
        request_key: "rk-1",
        timestamp: 1_000,
        model: "claude",
        provider: "anthropic",
        model_reference: "anthropic/claude",
        api: "messages",
        project: "pp",
        cwd: "/tmp/pp",
        purpose: "task",
        session_file: "/tmp/s1.jsonl",
        entry_id: null,
        duration_ms: 500,
        ttft_ms: 100,
        input_tokens: 60,
        output_tokens: 30,
        cached_tokens: 10,
        cache_write_tokens: 0,
        status: "ok",
        stop_reason: "end_turn",
        error_message: null,
        cost: 0.01,
        cost_components: null,
      },
      {
        id: 11,
        request_key: "rk-2",
        timestamp: 55_000,
        model: "claude",
        provider: "anthropic",
        model_reference: "anthropic/claude",
        api: "messages",
        project: "pp",
        cwd: "/tmp/pp",
        purpose: "subagent",
        session_file: "/tmp/s1.subagents/sessions/child.jsonl",
        entry_id: null,
        duration_ms: 500,
        ttft_ms: 80,
        input_tokens: 40,
        output_tokens: 20,
        cached_tokens: 10,
        cache_write_tokens: 0,
        status: "ok",
        stop_reason: "end_turn",
        error_message: null,
        cost: 0.01,
        cost_components: null,
      },
    ],
    tool_calls: [
      {
        id: 1,
        request_key: "rk-1",
        session_file: "/tmp/s1.jsonl",
        call_id: "call-1",
        tool: "read",
        timestamp: 1_600,
        duration_ms: 200,
        status: "ok",
        error_message: null,
        request_id: 10,
      },
    ],
    tool_stats: [{ tool: "read", calls: 1, errors: 0, total_ms: 200, max_ms: 200 }],
    updated_at: 61_000,
  };
}

describe("traces timeline & layout", () => {
  test("traceSignature stays stable for identical payloads and changes when requests or title change", () => {
    const payload = sampleTrace();
    const sig1 = traceSignature(payload, "24h");
    const sig2 = traceSignature(sampleTrace(), "24h");
    expect(sig1).toBe(sig2);
    expect(traceSignature({ ...payload, title: "Updated" }, "24h")).not.toBe(sig1);
  });

  test("traceViewModel folds subagent tracks and traceLayout hides collapsed child tracks", () => {
    const view = traceViewModel(sampleTrace(), "");
    expect(view.tracks.length).toBe(2);
    expect(view.tracks[0]?.id).toBe("main");
    expect(view.tracks[1]?.parentId).toBe("main");

    const expanded = traceLayout(view.tracks, new Set());
    expect(expanded.blocks.length).toBe(2);

    const collapsed = traceLayout(view.tracks, new Set(["main/1"]));
    expect(collapsed.blocks.length).toBe(2);
    expect(collapsed.blocks[1]?.lanes.length).toBe(0);
    expect(collapsed.totalHeight).toBeLessThan(expanded.totalHeight);
  });

  test("traceCompressed collapses >30s idle gaps and preserves monotonic toU/toT roundtrip", () => {
    const view = traceViewModel(sampleTrace(), "");
    const compressed = traceCompressed(view.tracks, 1_000, 55_500);
    expect(compressed.gaps.length).toBe(1);
    const uStart = compressed.toU(1_000);
    const uEnd = compressed.toU(55_500);
    expect(uEnd).toBeGreaterThan(uStart);
    expect(compressed.toT(uStart)).toBeCloseTo(1_000, 3);
    expect(compressed.toT(uEnd)).toBeCloseTo(55_500, 3);

    const equal = traceEqualWidth([1_000, 55_000], 1_000, 55_500);
    expect(equal.toU(55_000)).toBeGreaterThan(equal.toU(1_000));

    const scale = traceScale(view.tracks, "duration", true);
    const ticks = traceTicks(scale, scale.domain[0], scale.domain[1], 800);
    expect(ticks.length).toBeGreaterThan(0);
  });
});
