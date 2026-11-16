import { describe, expect, test } from "bun:test";
import { errorGroups, errorModelGroups, errorSignatureText } from "../../src/activity.js";
import type { RequestRow } from "../../web-types/api.d.ts";

function makeFailure(overrides: Partial<RequestRow>): RequestRow {
  return {
    id: 1,
    request_key: "rk-1",
    timestamp: 1_700_000_000_000,
    model: "claude-sonnet",
    provider: "anthropic",
    model_reference: "anthropic/claude-sonnet",
    api: "messages",
    project: "pp",
    cwd: "/tmp/pp",
    purpose: "task",
    session_file: null,
    entry_id: null,
    duration_ms: 120,
    ttft_ms: null,
    input_tokens: 100,
    output_tokens: 0,
    cached_tokens: 0,
    cache_write_tokens: 0,
    status: "failed",
    stop_reason: "error",
    error_message: "429 rate limit exceeded",
    cost: null,
    cost_components: null,
    ...overrides,
  };
}

describe("activity error normalization & grouping", () => {
  test("errorSignatureText normalizes UUIDs, prefixed IDs, hex, and numbers while preserving HTTP status codes", () => {
    expect(errorSignatureText(null)).toBe("Unknown error");
    expect(errorSignatureText("   ")).toBe("Unknown error");
    const raw =
      "HTTP 429 on req_abc1234567 for 123e4567-e89b-12d3-a456-426614174000 hash deadbeefcafebabe retry 12";
    expect(errorSignatureText(raw)).toBe("HTTP 429 on <id> for <id> hash <hex> retry N");
  });

  test("errorSignatureText truncates long normalized messages at 180 chars with ellipsis", () => {
    const longMsg = "x".repeat(220);
    const sig = errorSignatureText(longMsg);
    expect(sig.length).toBe(180);
    expect(sig.endsWith("…")).toBe(true);
  });

  test("errorGroups and errorModelGroups aggregate failures and track latest row per signature", () => {
    const rows: RequestRow[] = [
      makeFailure({ id: 1, timestamp: 1000, error_message: "503 upstream req_abcdef12" }),
      makeFailure({
        id: 2,
        timestamp: 2000,
        error_message: "503 upstream req_zyxwvu98",
        model: "gpt-5",
        provider: "openai",
        model_reference: "openai/gpt-5",
      }),
      makeFailure({ id: 3, timestamp: 1500, error_message: "timeout after 30s" }),
    ];

    const groups = errorGroups(rows);
    expect(groups.length).toBe(2);
    expect(groups[0]?.signature).toBe("503 upstream <id>");
    expect(groups[0]?.count).toBe(2);
    expect(groups[0]?.latest.id).toBe(2);
    expect(groups[0]?.models.length).toBe(2);

    const byModel = errorModelGroups(rows);
    expect(byModel[0]?.key).toBe("anthropic/claude-sonnet");
    expect(byModel[0]?.count).toBe(2);
  });
});
