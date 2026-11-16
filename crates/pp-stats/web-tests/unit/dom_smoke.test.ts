import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import fs from "node:fs";
import path from "node:path";
import { Window } from "happy-dom";

const XSS = `<script>alert("xss")</script><img src=x onerror="alert(1)">`;
const NOW = 1_700_000_000_000;

function makeFixtures(empty = false) {
  const zeroTotals = {
    requests: 0,
    ok: 0,
    failed: 0,
    aborted: 0,
    input_tokens: 0,
    output_tokens: 0,
    cached_tokens: 0,
    uncached_tokens: 0,
    tokens: 0,
    usage_known: 0,
    cache_known: 0,
    cache_write_known: 0,
    cache_write_tokens: 0,
    avg_duration_ms: null,
    avg_ttft_ms: null,
    tokens_per_second: null,
    first_seen: null,
    last_seen: null,
    priced: 0,
    unpriced: 0,
    cost: 0,
    cost_components: { input: 0, cache_read: 0, cache_write: 0, output: 0 },
  };
  if (empty) {
    return {
      analytics: {
        summary: zeroTotals,
        buckets: [],
        daily: [],
        hours: [],
        models: [],
        providers: [],
        active_days: 0,
        range: "24h",
        updated_at: NOW,
        latest: [],
      },
      requests: { requests: [], has_more: false, limit: 500, updated_at: NOW, range: "24h" },
      errors: { requests: [], has_more: false, limit: 50, updated_at: NOW, range: "24h" },
      traces: { sessions: [], total: 0, has_more: false, limit: 50, updated_at: NOW, range: "24h" },
      tools: {
        summary: {
          calls: 0,
          args_chars: 0,
          tokens: 0,
          output_tokens: 0,
          usage_known: 0,
          cost: 0,
          priced: 0,
          unpriced: 0,
          last_seen: 0,
          errors: null,
          cancelled: null,
          executions: null,
          result_chars: null,
          result_calls: 0,
          avg_duration_ms: null,
          buckets: [],
          recent: [],
        },
        tools: [],
        requests_with_tools: 0,
        partial_requests: 0,
        updated_at: NOW,
        range: "24h",
      },
      hooks: {
        summary: {
          runs: 0,
          ok: 0,
          blocked: 0,
          errors: 0,
          modified: 0,
          total_duration_ms: 0,
          avg_duration_ms: null,
          max_duration_ms: null,
          first_seen: null,
          last_seen: null,
          sessions: 0,
        },
        buckets: [],
        events: [],
        projects: [],
        updated_at: NOW,
        range: "24h",
      },
      projects: { summary: zeroTotals, projects: [], updated_at: NOW, range: "24h" },
    };
  }
  const row = {
    id: 1,
    request_key: "rk-1",
    timestamp: NOW - 60_000,
    model: `claude-${XSS}`,
    provider: "anthropic",
    model_reference: "anthropic/claude",
    api: "messages",
    project: `pp-${XSS}`,
    cwd: `/tmp/${XSS}`,
    purpose: "task",
    session_file: "/tmp/session.jsonl",
    entry_id: null,
    duration_ms: 420,
    ttft_ms: 120,
    input_tokens: 1000,
    output_tokens: 200,
    cached_tokens: 400,
    cache_write_tokens: 100,
    status: "failed",
    stop_reason: "error",
    error_message: `Error ${XSS} 500`,
    cost: 0.015,
    cost_components: { input: 0.005, cache_read: 0.002, cache_write: 0.003, output: 0.005 },
    has_context: true,
  };
  const totals = {
    ...zeroTotals,
    requests: 1,
    failed: 1,
    input_tokens: 1000,
    output_tokens: 200,
    cached_tokens: 400,
    uncached_tokens: 500,
    tokens: 1200,
    usage_known: 1,
    cache_known: 1,
    cache_write_known: 1,
    cache_write_tokens: 100,
    avg_duration_ms: 420,
    avg_ttft_ms: 120,
    tokens_per_second: 476,
    first_seen: NOW - 60_000,
    last_seen: NOW - 60_000,
    priced: 1,
    cost: 0.015,
    cost_components: { input: 0.005, cache_read: 0.002, cache_write: 0.003, output: 0.005 },
  };
  const bucket = { ...totals, timestamp: Math.floor((NOW - 60_000) / 3_600_000) * 3_600_000 };
  const daily = { ...totals, timestamp: Math.floor(NOW / 86_400_000) * 86_400_000 };
  const hours = Array.from({ length: 24 }, (_, hour) => ({ ...zeroTotals, hour }));
  return {
    analytics: {
      summary: totals,
      buckets: [bucket],
      daily: [daily],
      hours,
      models: [
        {
          ...totals,
          key: "anthropic/claude",
          model: row.model,
          provider: "anthropic",
          rates: { input: 3, cached_input: 0.3, cache_write: 3.75, output: 15 },
          buckets: [bucket],
          daily: [daily],
          hours,
          models: 1,
        },
      ],
      providers: [
        {
          ...totals,
          key: "anthropic",
          provider: "anthropic",
          buckets: [bucket],
          daily: [daily],
          hours,
          models: 1,
        },
      ],
      active_days: 1,
      range: "24h",
      updated_at: NOW,
      latest: [row],
    },
    requests: { requests: [row], has_more: false, limit: 500, updated_at: NOW, range: "24h" },
    errors: { requests: [row], has_more: false, limit: 50, updated_at: NOW, range: "24h" },
    traces: {
      sessions: [
        {
          ...totals,
          id: 1,
          title: `Session ${XSS}`,
          session_file: "/tmp/session.jsonl",
          project: row.project,
          cwd: row.cwd,
          tool_calls: 1,
          agents: 0,
          wall_ms: 420,
          model_ms: 420,
          models: [{ key: "anthropic/claude", model: row.model, provider: "anthropic" }],
        },
      ],
      total: 1,
      has_more: false,
      limit: 50,
      updated_at: NOW,
      range: "24h",
    },
    tools: {
      summary: {
        calls: 1,
        args_chars: 24,
        tokens: 1200,
        output_tokens: 200,
        usage_known: 1,
        cost: 0.015,
        priced: 1,
        unpriced: 0,
        last_seen: NOW - 60_000,
        errors: 0,
        cancelled: 0,
        executions: 1,
        result_chars: 64,
        result_calls: 1,
        avg_duration_ms: 30,
        buckets: [{ timestamp: bucket.timestamp, requests: 1 }],
        error_buckets: [],
        recent: [],
      },
      tools: [
        {
          key: "read",
          tool: "read",
          calls: 1,
          args_chars: 24,
          tokens: 1200,
          output_tokens: 200,
          usage_known: 1,
          cost: 0.015,
          priced: 1,
          unpriced: 0,
          last_seen: NOW - 60_000,
          errors: 0,
          cancelled: 0,
          executions: 1,
          result_chars: 64,
          result_calls: 1,
          avg_duration_ms: 30,
          buckets: [{ timestamp: bucket.timestamp, requests: 1 }],
          error_buckets: [],
          recent: [],
          models: [
            {
              key: "anthropic/claude",
              model: row.model,
              provider: "anthropic",
              calls: 1,
              args_chars: 24,
              tokens: 1200,
              output_tokens: 200,
              usage_known: 1,
              cost: 0.015,
              priced: 1,
              unpriced: 0,
              last_seen: NOW - 60_000,
              errors: 0,
              cancelled: 0,
              executions: 1,
              result_chars: 64,
              result_calls: 1,
              avg_duration_ms: 30,
              buckets: [{ timestamp: bucket.timestamp, requests: 1 }],
              recent: [],
            },
          ],
        },
      ],
      requests_with_tools: 1,
      partial_requests: 1,
      updated_at: NOW,
      range: "24h",
    },
    hooks: {
      summary: {
        runs: 1,
        ok: 1,
        blocked: 0,
        errors: 0,
        modified: 0,
        total_duration_ms: 12,
        avg_duration_ms: 12,
        max_duration_ms: 12,
        first_seen: NOW - 60_000,
        last_seen: NOW - 60_000,
        sessions: 1,
      },
      buckets: [
        {
          timestamp: bucket.timestamp,
          runs: 1,
          ok: 1,
          blocked: 0,
          errors: 0,
          modified: 0,
          total_duration_ms: 12,
          avg_duration_ms: 12,
          max_duration_ms: 12,
          first_seen: NOW - 60_000,
          last_seen: NOW - 60_000,
        },
      ],
      events: [
        {
          event: "PreToolUse",
          sessions: 1,
          runs: 1,
          ok: 1,
          blocked: 0,
          errors: 0,
          modified: 0,
          total_duration_ms: 12,
          avg_duration_ms: 12,
          max_duration_ms: 12,
          first_seen: NOW - 60_000,
          last_seen: NOW - 60_000,
          tools: [],
        },
      ],
      projects: [
        {
          key: row.cwd,
          project: row.project,
          sessions: 1,
          events: 1,
          runs: 1,
          ok: 1,
          blocked: 0,
          errors: 0,
          modified: 0,
          total_duration_ms: 12,
          avg_duration_ms: 12,
          max_duration_ms: 12,
          first_seen: NOW - 60_000,
          last_seen: NOW - 60_000,
        },
      ],
      updated_at: NOW,
      range: "24h",
    },
    projects: {
      summary: totals,
      projects: [
        {
          ...totals,
          key: row.cwd,
          project: row.project,
          sessions: 1,
          requests_without_session: 0,
          recent: [row],
          buckets: [bucket],
          models: [{ ...totals, key: "anthropic/claude", model: row.model, provider: "anthropic" }],
        },
      ],
      updated_at: NOW,
      range: "24h",
    },
  };
}

let happyWindow: Window;
let activeFixture = makeFixtures(false);
let failStylePost = false;

describe("pp-stats DOM smoke & invariants", () => {
  beforeAll(async () => {
    const html = fs.readFileSync(path.resolve(import.meta.dir, "../../src/index.html"), "utf8");
    happyWindow = new Window({ url: "http://127.0.0.1:3848/#/overview?range=24h" });
    happyWindow.document.write(html);

    const g = globalThis as Record<string, unknown>;
    g.window = happyWindow;
    g.document = happyWindow.document;
    g.localStorage = happyWindow.localStorage;
    g.HTMLElement = happyWindow.HTMLElement;
    g.HTMLInputElement = happyWindow.HTMLInputElement;
    g.HTMLSelectElement = happyWindow.HTMLSelectElement;
    g.HTMLTableCellElement = happyWindow.HTMLTableCellElement;
    g.HTMLTableRowElement = happyWindow.HTMLTableRowElement;
    g.HTMLDetailsElement = happyWindow.HTMLDetailsElement;
    g.HTMLButtonElement = happyWindow.HTMLButtonElement;
    g.Element = happyWindow.Element;
    g.SVGElement = happyWindow.SVGElement;
    g.CSS = { escape: (s: string) => s.replace(/[^a-zA-Z0-9_-]/g, "\\$&") };
    g.ResizeObserver = class {
      observe() {}
      disconnect() {}
    };
    g.MutationObserver = happyWindow.MutationObserver;
    g.matchMedia = () => ({ matches: false, addEventListener() {} });

    g.fetch = async (input: string | URL, init?: RequestInit) => {
      const url = typeof input === "string" ? input : input.toString();
      if (url === "/api/stats/style") {
        if (init?.method === "POST" && failStylePost) {
          return new Response(JSON.stringify({ error: "unavailable" }), { status: 503 });
        }
        return new Response(JSON.stringify({ style: "default" }), { status: 200 });
      }
      const route = url.split("?")[0]?.replace("/api/stats/", "") ?? "overview";
      const payload =
        route === "overview" || route === "models" || route === "providers" || route === "costs"
          ? activeFixture.analytics
          : activeFixture[route as keyof typeof activeFixture];
      return new Response(JSON.stringify(payload ?? {}), { status: 200 });
    };

    const statsMod = await import("../../src/stats.js");
    statsMod.boot();
  });

  afterAll(() => {
    happyWindow?.close();
  });

  test("all routes render cleanly without script injection on XSS fixtures", async () => {
    activeFixture = makeFixtures(false);
    const { onRoute } = await import("../../src/stats.js");
    const routes = [
      "models",
      "providers",
      "costs",
      "requests",
      "errors",
      "traces",
      "tools",
      "hooks",
      "projects",
      "overview",
    ];
    for (const route of routes) {
      happyWindow.location.hash = `/${route}?range=24h`;
      onRoute();
      await new Promise((r) => setTimeout(r, 15));
      const pageEl = happyWindow.document.getElementById("page") as unknown as HTMLElement | null;
      expect(pageEl).not.toBeNull();
      expect(pageEl?.textContent?.length).toBeGreaterThan(0);
      expect(pageEl?.dataset.enter).toBe("true");
      expect(pageEl?.querySelectorAll("script").length).toBe(0);
      expect(pageEl?.querySelectorAll("img[onerror]").length).toBe(0);
    }
  });

  test("projects row expansion renders recent requests without ReferenceError and clears data-enter", async () => {
    activeFixture = makeFixtures(false);
    const { onRoute } = await import("../../src/stats.js");
    happyWindow.location.hash = "#/projects?range=24h";
    onRoute();
    await new Promise((r) => setTimeout(r, 15));
    const pageEl = happyWindow.document.getElementById("page") as unknown as HTMLElement | null;
    const row = pageEl?.querySelector("[data-expand]") as HTMLElement | null;
    expect(row).not.toBeNull();
    row?.click();
    expect(pageEl?.dataset.enter).toBe("false");
    expect(pageEl?.textContent).toContain("Latest requests");
  });

  test("empty database renders all pages without errors", async () => {
    activeFixture = makeFixtures(true);
    const { onRoute } = await import("../../src/stats.js");
    for (const route of [
      "overview",
      "models",
      "providers",
      "costs",
      "requests",
      "errors",
      "traces",
      "tools",
      "hooks",
      "projects",
    ]) {
      happyWindow.location.hash = `/${route}?range=24h`;
      onRoute();
      await new Promise((r) => setTimeout(r, 10));
      const errorEl = happyWindow.document.getElementById("error") as unknown as HTMLElement | null;
      expect(errorEl?.hidden).toBe(true);
    }
  });

  test("style toggle failure keeps existing data-style and re-enables button", async () => {
    const styleBtn = happyWindow.document.getElementById("style") as HTMLButtonElement | null;
    expect(styleBtn).not.toBeNull();
    await new Promise((r) => setTimeout(r, 10));
    expect(happyWindow.document.documentElement.dataset.style).toBe("default");
    failStylePost = true;
    styleBtn?.click();
    await new Promise((r) => setTimeout(r, 15));
    expect(happyWindow.document.documentElement.dataset.style).toBe("default");
    expect(styleBtn?.disabled).toBe(false);
    failStylePost = false;
  });
});
