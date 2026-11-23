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
let activeFixture: Record<string, unknown> = makeFixtures(false);
let failStylePost = false;

const TOOL_METRIC_BASE = {
  calls: 1,
  args_chars: 10,
  cost: 0.5,
  priced: 1,
  unpriced: 0,
  last_seen: NOW,
  errors: 0,
  cancelled: 0,
  executions: 1,
  result_chars: 8,
  result_calls: 1,
  avg_duration_ms: 5,
  buckets: [],
  error_buckets: [],
  recent: [],
};

function toolRow(tool: string, calls: number, tokens: number | null, usageKnown: number) {
  const metric = {
    ...TOOL_METRIC_BASE,
    calls,
    tokens,
    output_tokens: tokens == null ? null : Math.round(tokens / 5),
    usage_known: usageKnown,
  };
  return {
    ...metric,
    key: tool,
    tool,
    models: [
      {
        ...metric,
        key: "anthropic/claude",
        model: "claude",
        provider: "anthropic",
        buckets: [],
      },
    ],
  };
}

function toolsFixture(rows: ReturnType<typeof toolRow>[]) {
  const calls = rows.reduce((sum, row) => sum + row.calls, 0);
  const knownTokens = rows.flatMap((row) => (row.tokens == null ? [] : [row.tokens]));
  return {
    ...makeFixtures(false).tools,
    summary: {
      ...TOOL_METRIC_BASE,
      calls,
      tokens: knownTokens.length ? knownTokens.reduce((sum, value) => sum + value, 0) : null,
      usage_known: rows.reduce((sum, row) => sum + row.usage_known, 0),
      buckets: [],
    },
    tools: rows,
    updated_at: NOW,
  };
}

function errorsFixture(count: number, hasMore: boolean) {
  const base = makeFixtures(false).errors;
  return {
    ...base,
    requests: Array.from({ length: count }, (_, index) => ({
      ...base.requests[0],
      id: index + 1,
      timestamp: NOW - (index + 1) * 60_000,
      error_message: `upstream 500 after ${index + 1} attempts`,
    })),
    has_more: hasMore,
    updated_at: NOW,
  };
}

function statText(pageEl: HTMLElement, label: string) {
  const stat = [...pageEl.querySelectorAll(".x-stat")].find(
    (element) => element.querySelector(".x-stat-label")?.textContent === label,
  );
  return {
    value: stat?.querySelector(".x-stat-value")?.textContent?.trim() ?? null,
    hint: stat?.querySelector(".x-stat-foot")?.textContent?.trim() ?? null,
  };
}

function columnTexts(pageEl: HTMLElement, sortKey: string, table: string) {
  const header = pageEl.querySelector(`th[data-toolsort="${sortKey}"][data-tooltable="${table}"]`);
  const scope = header?.closest("table");
  if (!header || !scope) return [];
  const index = [...scope.querySelectorAll("thead th")].indexOf(header);
  return [...scope.querySelectorAll("tbody tr")].map(
    (row) => row.querySelectorAll("td")[index]?.textContent?.trim() ?? "",
  );
}

async function openRoute(route: string) {
  const { onRoute } = await import("../../src/stats.js");
  happyWindow.location.hash = `/${route}?range=24h`;
  onRoute();
  await new Promise((r) => setTimeout(r, 15));
  return happyWindow.document.getElementById("page") as unknown as HTMLElement;
}

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

  test("tools page shows unknown attributed usage as — and keeps known coverage", async () => {
    activeFixture = {
      ...makeFixtures(false),
      tools: toolsFixture([toolRow("read", 3, 3000, 3), toolRow("bash", 1, null, 0)]),
    };
    const pageEl = await openRoute("tools");
    const tokens = statText(pageEl, "Attributed tokens");
    expect(tokens.value).toBe("3K");
    expect(tokens.hint).toContain("75% of calls known");

    const toolTokens = columnTexts(pageEl, "tokens", "tool");
    expect(toolTokens[0]).toContain("3K");
    expect(toolTokens[1]).toBe("—");
    const modelTokens = columnTexts(pageEl, "tokens", "model");
    expect(modelTokens[0]).toContain("3K");
    expect(modelTokens[1]).toBe("—");

    activeFixture = {
      ...makeFixtures(false),
      tools: toolsFixture([toolRow("read", 3, null, 0), toolRow("bash", 1, null, 0)]),
    };
    const unknownPage = await openRoute("tools");
    const unknownTokens = statText(unknownPage, "Attributed tokens");
    expect(unknownTokens.value).toBe("—");
    expect(unknownTokens.hint).toBe("No token usage recorded");
  });

  test("tools page sorts unknown usage below known zero tokens", async () => {
    activeFixture = {
      ...makeFixtures(false),
      tools: toolsFixture([toolRow("read", 2, 0, 2), toolRow("bash", 1, null, 0)]),
    };
    const pageEl = await openRoute("tools");
    const order = () =>
      [...pageEl.querySelectorAll("tr[data-toolselect]")].map((row) =>
        row.getAttribute("data-toolselect"),
      );
    const tokensHeader = pageEl.querySelector('[data-toolsort="tokens"]');
    expect(tokensHeader).not.toBeNull();
    expect(statText(pageEl, "Attributed tokens").value).toBe("0");
    (tokensHeader as HTMLElement).click();
    await new Promise((r) => setTimeout(r, 15));
    expect(order()).toEqual(["read", "bash"]);
    (pageEl.querySelector('[data-toolsort="tokens"]') as HTMLElement).click();
    await new Promise((r) => setTimeout(r, 15));
    expect(order()).toEqual(["bash", "read"]);
  });

  test("errors page reports completeness from has_more, not from the loaded count", async () => {
    for (const count of [50, 200, 1000]) {
      activeFixture = { ...makeFixtures(false), errors: errorsFixture(count, false) };
      const pageEl = await openRoute("errors");
      expect(pageEl.textContent).not.toContain("older ones are not loaded");
      expect(pageEl.querySelector("[data-errors-load]")).toBeNull();
      expect(statText(pageEl, "Failures").hint).toBe("in the last 24 hours");
    }

    for (const count of [50, 200]) {
      activeFixture = { ...makeFixtures(false), errors: errorsFixture(count, true) };
      const pageEl = await openRoute("errors");
      expect(pageEl.textContent).toContain("older ones are not loaded");
      const load = pageEl.querySelector("[data-errors-load]");
      expect(load?.textContent?.trim()).toBe("Load latest 200");
      expect(statText(pageEl, "Failures").hint).toBe(`latest ${count} loaded`);
    }
  });
});
