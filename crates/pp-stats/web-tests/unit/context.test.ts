import { describe, expect, test } from "bun:test";
import {
  contextItems,
  contextItemText,
  highlightContext,
  requestContextButton,
} from "../../src/context.js";
import type { RequestDetailResponse } from "../../web-types/api.d.ts";

describe("context reader parsing & highlighting", () => {
  test("contextItems parses both Chat messages and Responses input, tools, and skills", () => {
    const chatItems = contextItems({
      request: {
        instructions: "Be concise.",
        messages: [
          { role: "system", content: "Sys" },
          { role: "user", content: "Hello" },
        ],
        tools: [{ type: "function", function: { name: "read", description: "Read a file" } }],
        temperature: 0.2,
      },
      skills: [{ name: "rust", description: "Rust best practices", uri: "skill://rust" }],
    });

    expect(chatItems.map((item) => item.kind)).toContain("Context");
    expect(chatItems.some((item) => item.kind === "Tools" && item.label === "read")).toBe(true);
    expect(chatItems.some((item) => item.kind === "Skills" && item.label === "rust")).toBe(true);

    const responsesItems = contextItems({
      request: {
        input: [{ role: "user", content: "Ping" }],
      },
      skills: [],
    });
    expect(responsesItems.some((item) => item.label === "User Prompt")).toBe(true);
    const userItem = responsesItems.find((item) => item.key === "message-0");
    expect(userItem ? contextItemText(userItem) : "").toBe("Ping");
  });

  test("highlightContext escapes embedded HTML/script tags in JSON and Markdown", () => {
    const jsonHtml = highlightContext(JSON.stringify({ msg: "<script>alert(1)</script>" }), true);
    expect(jsonHtml).not.toContain("<script>");
    expect(jsonHtml).toContain("&lt;script&gt;");

    const mdHtml = highlightContext("# Title <img src=x onerror=alert(1)>", false);
    expect(mdHtml).not.toContain("<img");
    expect(mdHtml).toContain("&lt;img");
  });

  test("requestContextButton renders only when request_context is present", () => {
    const baseDetail = {
      request: {
        id: 7,
      },
      request_context: null,
    } as unknown as RequestDetailResponse;
    expect(requestContextButton(baseDetail)).toBe("");
    const withCtx = {
      ...baseDetail,
      request_context: { request: { instructions: "hi" }, skills: [] },
    } as RequestDetailResponse;
    expect(requestContextButton(withCtx)).toContain('data-context-id="7"');
  });
});
