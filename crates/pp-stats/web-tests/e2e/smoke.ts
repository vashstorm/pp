import { Database } from "bun:sqlite";
import { type ChildProcess, spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { chromium } from "playwright-core";

const REPO_ROOT = path.resolve(import.meta.dir, "../../../..");
const DEFAULT_BIN = path.join(REPO_ROOT, "target/debug/pp");

function resolvePpBinary(): string {
  const bin = process.env.PP_BIN || DEFAULT_BIN;
  if (!fs.existsSync(bin)) {
    throw new Error(`pp binary not found at ${bin}; run 'cargo build --locked' first`);
  }
  return bin;
}

function findCachedChromium(): string | undefined {
  if (process.env.PW_CHROMIUM_EXECUTABLE) return process.env.PW_CHROMIUM_EXECUTABLE;
  const cacheDirs = [
    path.join(os.homedir(), "Library/Caches/ms-playwright"),
    path.join(os.homedir(), ".cache/ms-playwright"),
  ];
  for (const dir of cacheDirs) {
    if (!fs.existsSync(dir)) continue;
    const entries = fs
      .readdirSync(dir)
      .filter((name) => name.startsWith("chromium_headless_shell-") || name.startsWith("chromium-"))
      .sort()
      .reverse();
    for (const entry of entries) {
      const candidates = [
        path.join(dir, entry, "chrome-headless-shell-mac-arm64/chrome-headless-shell"),
        path.join(dir, entry, "chrome-headless-shell-mac-x64/chrome-headless-shell"),
        path.join(dir, entry, "chrome-headless-shell-linux/chrome-headless-shell"),
        path.join(
          dir,
          entry,
          "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
        ),
        path.join(dir, entry, "chrome-linux/chrome"),
      ];
      const found = candidates.find((p) => fs.existsSync(p));
      if (found) return found;
    }
  }
  return undefined;
}

interface StartedServer {
  child: ChildProcess;
  baseUrl: string;
}

async function startStatsServer(bin: string, homeDir: string): Promise<StartedServer> {
  const child = spawn(bin, ["stats", "--port", "0", "--no-open"], {
    cwd: homeDir,
    env: {
      ...process.env,
      HOME: homeDir,
      TMPDIR: "/tmp",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });

  let stdout = "";
  let stderr = "";
  const baseUrl = await new Promise<string>((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(
        new Error(`Timed out waiting for pp stats banner.\nstdout: ${stdout}\nstderr: ${stderr}`),
      );
    }, 10_000);
    child.stdout?.on("data", (chunk: Buffer) => {
      stdout += chunk.toString("utf8");
      const match = stdout.match(/pp stats: (http:\/\/127\.0\.0\.1:\d+)/);
      if (match?.[1]) {
        clearTimeout(timer);
        resolve(match[1]);
      }
    });
    child.stderr?.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });
    child.on("exit", (code) => {
      clearTimeout(timer);
      reject(
        new Error(`pp stats exited prematurely (${code}).\nstdout: ${stdout}\nstderr: ${stderr}`),
      );
    });
  });

  return { child, baseUrl };
}

function seedStatsDatabase(homeDir: string): void {
  const dbPath = path.join(homeDir, ".pp/stats/stats.db");
  const db = new Database(dbPath);
  const now = Date.now();
  const rootSession = path.join(homeDir, ".pp/agent/sessions/root.jsonl");
  const subSession = path.join(homeDir, ".pp/agent/sessions/root.subagents/sessions/child.jsonl");

  const costJson = JSON.stringify({
    input: 0.003,
    cache_read: 0.0005,
    cache_write: 0.001,
    output: 0.015,
  });
  const rateJson = JSON.stringify({ input: 3, cached_input: 0.3, cache_write: 3.75, output: 15 });

  db.query(
    `INSERT INTO requests (
      id, request_key, timestamp, model, provider, model_reference, api,
      project, cwd, purpose, session_file, entry_id, duration_ms, ttft_ms,
      input_tokens, output_tokens, cached_tokens, status, stop_reason, error_message,
      cache_write_tokens, cost_json, rate_json
    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
  ).run(
    1,
    "rk-1",
    now - 120_000,
    "claude-sonnet-4-5",
    "anthropic",
    "anthropic/claude-sonnet-4-5",
    "messages",
    "demo-app",
    "/tmp/demo-app",
    "task",
    rootSession,
    null,
    850,
    140,
    1000,
    400,
    500,
    "ok",
    "end_turn",
    null,
    100,
    costJson,
    rateJson,
  );

  db.query(
    `INSERT INTO requests (
      id, request_key, timestamp, model, provider, model_reference, api,
      project, cwd, purpose, session_file, entry_id, duration_ms, ttft_ms,
      input_tokens, output_tokens, cached_tokens, status, stop_reason, error_message,
      cache_write_tokens, cost_json, rate_json
    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
  ).run(
    2,
    "rk-2",
    now - 60_000,
    "gpt-5",
    "openai",
    "openai/gpt-5",
    "responses",
    "demo-app",
    "/tmp/demo-app",
    "subagent",
    subSession,
    null,
    420,
    90,
    600,
    120,
    0,
    "failed",
    "error",
    "HTTP 429 rate limit on req_12345678",
    0,
    costJson,
    rateJson,
  );

  db.query(
    "INSERT INTO request_contexts (request_id, request_json, skills_json) VALUES (?, ?, ?)",
  ).run(
    1,
    JSON.stringify({
      instructions: "System prompt smoke",
      messages: [{ role: "user", content: "Inspect codebase" }],
      tools: [{ type: "function", function: { name: "read", description: "Read file" } }],
    }),
    JSON.stringify([{ name: "rust", description: "Rust guidelines", uri: "skill://rust" }]),
  );

  db.query("INSERT INTO request_tools (request_id, tool, args_chars) VALUES (?, ?, ?)").run(
    1,
    "read",
    48,
  );

  db.query(
    `INSERT INTO tool_calls (
      request_key, session_file, call_id, tool, timestamp, status, error_message, duration_ms, result_chars
    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
  ).run("rk-1", rootSession, "call-1", "read", now - 119_000, "ok", null, 35, 1280);

  db.query(
    `INSERT INTO hook_runs (
      timestamp, session_file, project, cwd, event, tool, status, modified, duration_ms
    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
  ).run(now - 119_500, rootSession, "demo-app", "/tmp/demo-app", "PreToolUse", "read", "ok", 1, 12);

  // Exactly 50 failures (including rk-2 above) and every added row has unknown usage:
  // the browser verifies both the unknown value on Tools and the complete single-page boundary on
  // Errors.
  const insertUnknownFailure = db.query(
    `INSERT INTO requests (
      id, request_key, timestamp, model, provider, model_reference, api,
      project, cwd, purpose, session_file, entry_id, duration_ms, ttft_ms,
      input_tokens, output_tokens, cached_tokens, status, stop_reason, error_message,
      cache_write_tokens, cost_json, rate_json
    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
  );
  for (let id = 3; id <= 51; id += 1) {
    insertUnknownFailure.run(
      id,
      `rk-${id}`,
      now - id,
      "unknown-model",
      "unknown-provider",
      "unknown-provider/unknown-model",
      "responses",
      "demo-app",
      "/tmp/demo-app",
      "task",
      rootSession,
      null,
      20,
      null,
      null,
      null,
      null,
      "failed",
      "error",
      `unknown usage failure ${id}`,
      null,
      null,
      null,
    );
  }
  db.query("INSERT INTO request_tools (request_id, tool, args_chars) VALUES (?, ?, ?)").run(
    3,
    "unknown-tool",
    1,
  );

  db.close();
}

async function run(): Promise<void> {
  const bin = resolvePpBinary();
  const tempHome = fs.mkdtempSync("/tmp/pp-stats-e2e-");
  const executablePath = findCachedChromium();
  const browser = await chromium.launch({
    headless: true,
    ...(executablePath ? { executablePath } : {}),
  });

  const routes = [
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
  ];

  let server: StartedServer | null = null;
  const attachErrorListeners = (page: import("playwright-core").Page, errors: string[]): void => {
    page.on("pageerror", (err) => errors.push(`pageerror: ${err.message}`));
    page.on("console", (msg) => {
      if (msg.type() === "error") errors.push(`console.error: ${msg.text()}`);
    });
    page.on("response", (res) => {
      if (res.status() >= 400) errors.push(`HTTP ${res.status()} on ${res.url()}`);
    });
  };

  try {
    const errors: string[] = [];

    // 1. Empty HOME smoke: the server must start and render every route without config or history.
    server = await startStatsServer(bin, tempHome);
    const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    const page = await context.newPage();
    attachErrorListeners(page, errors);

    for (const route of routes) {
      await page.goto(`${server.baseUrl}/#/${route}?range=24h`, { waitUntil: "networkidle" });
      await page.waitForFunction(
        () => (document.getElementById("page")?.textContent?.trim().length ?? 0) > 0,
      );
    }

    await context.close();
    server.child.kill("SIGTERM");
    server = null;

    // 2. Seed real table data and exercise every page interaction plus style persistence.
    seedStatsDatabase(tempHome);
    server = await startStatsServer(bin, tempHome);
    const seededContext = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    const seededPage = await seededContext.newPage();
    attachErrorListeners(seededPage, errors);

    for (const route of routes) {
      await seededPage.goto(`${server.baseUrl}/#/${route}?range=24h`, { waitUntil: "networkidle" });
      await seededPage.waitForFunction(
        () =>
          !document.querySelector("#page .loading") &&
          (document.getElementById("page")?.textContent?.trim().length ?? 0) > 0,
      );
    }

    // Verify the style switch persistence and theme toggle.
    await seededPage.click("#style");
    await seededPage.waitForFunction(() => document.documentElement.dataset.style === "modern");
    await seededPage.reload({ waitUntil: "networkidle" });
    await seededPage.waitForFunction(() => document.documentElement.dataset.style === "modern");
    await seededPage.click("#theme");

    // Real DOM: unknown usage is not folded into 0; exactly 50 rows with has_more=false shows no load
    // more control.
    await seededPage.goto(`${server.baseUrl}/#/tools?range=24h`, { waitUntil: "networkidle" });
    const unknownTool = seededPage.locator('[data-toolselect="unknown-tool"]');
    await unknownTool.waitFor();
    if (!(await unknownTool.textContent())?.includes("—")) {
      throw new Error("Tools page did not preserve unknown token usage as —");
    }
    if (!(await seededPage.locator("#page").textContent())?.includes("of calls known")) {
      throw new Error("Tools page did not report partial known-usage coverage");
    }

    await seededPage.goto(`${server.baseUrl}/#/errors?range=24h`, { waitUntil: "networkidle" });
    const errorsText = await seededPage.locator("#page").textContent();
    if (
      errorsText?.includes("older ones are not loaded") ||
      (await seededPage.locator("[data-errors-load]").count()) !== 0
    ) {
      throw new Error("Errors page marked an exact complete page as incomplete");
    }

    // Verify the Requests drawer + Context reader.
    await seededPage.goto(`${server.baseUrl}/#/requests?range=24h`, { waitUntil: "networkidle" });
    await seededPage.click('tr[data-id="1"]');
    await seededPage.waitForSelector('[data-context-id="1"]');
    await seededPage.click('[data-context-id="1"]');
    await seededPage.waitForSelector("#context-reader");
    await seededPage.keyboard.press("Escape");
    await seededPage.keyboard.press("Escape");

    // Verify the Errors signature folding and the Projects row expansion.
    await seededPage.goto(`${server.baseUrl}/#/errors?range=24h`, { waitUntil: "networkidle" });
    await seededPage.click("[data-error-signature]");
    await seededPage.waitForSelector(".errors-detail");

    await seededPage.goto(`${server.baseUrl}/#/projects?range=24h`, { waitUntil: "networkidle" });
    await seededPage.click("[data-expand]");
    await seededPage.waitForSelector(".expanded-detail:not([hidden])");

    // Verify the Traces timeline Canvas and the Span drawer.
    await seededPage.goto(`${server.baseUrl}/#/traces?range=24h`, { waitUntil: "networkidle" });
    await seededPage.click("[data-trace-open]");
    await seededPage.waitForSelector("[data-trace-canvas]");
    const canvasBox = await seededPage.locator("[data-trace-canvas]").boundingBox();
    if (!canvasBox || canvasBox.width < 100 || canvasBox.height < 40) {
      throw new Error(`Invalid trace canvas bounding box: ${JSON.stringify(canvasBox)}`);
    }
    await seededPage.click("[data-trace-row]");
    await seededPage.waitForSelector("[data-trace-sheet]");

    await seededContext.close();
    if (errors.length > 0) {
      throw new Error(`Browser smoke detected errors:\n${errors.join("\n")}`);
    }
    console.log(
      "pp-stats Playwright E2E smoke passed (empty + seeded, default + modern, all 10 routes).",
    );
  } finally {
    await browser.close();
    server?.child.kill("SIGTERM");
    fs.rmSync(tempHome, { recursive: true, force: true });
  }
}

run().catch((error) => {
  console.error(error);
  process.exit(1);
});
