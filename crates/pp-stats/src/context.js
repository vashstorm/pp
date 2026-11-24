/**
 * 首请求上下文阅读器弹窗（Context / Tools / Skills 分类浏览、语法高亮与剪贴板复制）。
 */

import { h, icon, number } from "./web/format.js";
import { hooks } from "./web/state.js";

/** @typedef {import("../web-types/api.d.ts").RequestContextSnapshot} RequestContextSnapshot */
/** @typedef {import("../web-types/api.d.ts").RequestDetailResponse} RequestDetailResponse */

/**
 * @typedef {{
 *   key: string,
 *   kind: "Context" | "Tools" | "Skills",
 *   label: string,
 *   value: unknown,
 *   description: string,
 *   raw: unknown,
 * }} ContextItem
 */

/** @type {{
 *   detail: RequestDetailResponse,
 *   items: ContextItem[],
 *   selected: string,
 *   category: "Context" | "Tools" | "Skills",
 *   query: string,
 *   focus: Element | null,
 *   overflow: string,
 *   inert: Array<[HTMLElement, boolean]>,
 * } | null} */
let contextReaderState = null;

/**
 * 将首请求快照拆解为 Context / Tools / Skills 三类可阅读条目。
 * @param {RequestContextSnapshot} snapshot
 * @returns {ContextItem[]}
 */
export function contextItems(snapshot) {
  const request = snapshot.request;
  /** @type {ContextItem[]} */
  const items = [];
  /**
   * @param {string} key
   * @param {"Context" | "Tools" | "Skills"} kind
   * @param {string} label
   * @param {unknown} value
   * @param {string} [description]
   * @param {unknown} [raw]
   */
  const add = (key, kind, label, value, description = "", raw = null) => {
    items.push({ key, kind, label, value, description, raw });
  };
  if (request.instructions != null) {
    add("instructions", "Context", "System prompt", request.instructions);
  }
  const messages = request.messages ?? request.input;
  if (Array.isArray(messages)) {
    messages.forEach((message, index) => {
      const msg = /** @type {Record<string, unknown> | null} */ (
        typeof message === "object" && message !== null ? message : null
      );
      const role = String(msg?.role || msg?.type || "item");
      const system = role === "system" || role === "developer";
      const label = system
        ? "System prompt"
        : role === "user"
          ? "User Prompt"
          : `${index + 1}. ${role}`;
      const text = typeof msg?.content === "string" ? msg.content : message;
      add(
        `message-${index}`,
        "Context",
        label,
        text,
        `Original ${request.messages ? "messages" : "input"}[${index}]`,
        typeof text === "string" ? message : null,
      );
    });
  } else if (messages != null) {
    add("input", "Context", "User Prompt", messages);
  }
  const parameters = Object.fromEntries(
    Object.entries(request).filter(
      ([key]) => !["instructions", "messages", "input", "tools"].includes(key),
    ),
  );
  add("parameters", "Context", "Request parameters", parameters);
  const tools = Array.isArray(request.tools) ? request.tools : [];
  tools.forEach((tool, index) => {
    const toolObj = /** @type {Record<string, unknown> | null} */ (
      typeof tool === "object" && tool !== null ? tool : null
    );
    const definition = /** @type {Record<string, unknown> | null} */ (
      toolObj && typeof toolObj.function === "object" && toolObj.function !== null
        ? toolObj.function
        : toolObj
    );
    add(
      `tool-${index}`,
      "Tools",
      String(definition?.name || toolObj?.type || `Tool ${index + 1}`),
      tool,
      String(definition?.description || "Full tool definition and schema"),
    );
  });
  const skills = Array.isArray(snapshot.skills) ? snapshot.skills : [];
  skills.forEach((skill, index) => {
    add(
      `skill-${index}`,
      "Skills",
      skill.name || `Skill ${index + 1}`,
      skill,
      skill.description || skill.uri || "",
    );
  });
  add("native", "Context", "Full request JSON", request, "Complete original request body");
  return items;
}

/**
 * 渲染打开上下文阅读器的入口按钮（仅首请求带快照时返回按钮）。
 * @param {RequestDetailResponse} detail
 * @returns {string}
 */
export function requestContextButton(detail) {
  if (!detail.request_context) return "";
  return `<button class="drawer-link" data-context-id="${detail.request.id}" title="Read the full context sent with this session's first request">${icon("system", 14)} Context</button>`;
}

/**
 * 对 JSON 或 Markdown 文本做安全的语法高亮（全量内容经 `h()` 转义）。
 * @param {string} text
 * @param {boolean} json
 * @returns {string}
 */
export function highlightContext(text, json) {
  const tokens = json
    ? /"(?:\\.|[^"\\])*"|\b(?:true|false|null)\b|-?\b\d+(?:\.\d+)?(?:[eE][+-]?\d+)?\b/g
    : /^#{1,6}[^\n]*$|`[^`\n]+`|<\/?[A-Za-z][^>\n]*>|^\s*(?:[-*+]|\d+\.)\s+/gm;
  let result = "";
  let cursor = 0;
  for (const match of text.matchAll(tokens)) {
    const index = match.index ?? 0;
    result += h(text.slice(cursor, index));
    let kind = "marker";
    if (json) {
      kind = match[0].startsWith('"')
        ? /^\s*:/.test(text.slice(index + match[0].length))
          ? "key"
          : "string"
        : "literal";
    } else if (match[0].startsWith("`")) {
      kind = "string";
    } else if (match[0].startsWith("<")) {
      kind = "tag";
    }
    result += `<span class="context-token-${kind}">${h(match[0])}</span>`;
    cursor = index + match[0].length;
  }
  return result + h(text.slice(cursor));
}

/**
 * 将上下文条目序列化为展示/复制文本。
 * @param {ContextItem} item
 * @returns {string}
 */
export function contextItemText(item) {
  return typeof item.value === "string" ? item.value : JSON.stringify(item.value, null, 2);
}

function renderContextList() {
  const state = contextReaderState;
  if (!state) return;
  const query = state.query.trim().toLowerCase();
  const items = state.items.filter(
    (item) =>
      item.kind === state.category &&
      (!query || `${item.label}\n${item.description}`.toLowerCase().includes(query)),
  );
  const tabsEl = document.getElementById("context-tabs");
  const itemsEl = document.getElementById("context-items");
  if (!tabsEl || !itemsEl) return;
  tabsEl.innerHTML = /** @type {const} */ (["Context", "Tools", "Skills"])
    .map(
      (kind) =>
        `<button role="tab" aria-selected="${state.category === kind}" data-context-category="${kind}">${kind}<span>${state.items.filter((item) => item.kind === kind).length}</span></button>`,
    )
    .join("");
  itemsEl.innerHTML =
    items
      .map(
        (item) =>
          `<button class="context-item${state.selected === item.key ? " selected" : ""}" data-context-item="${item.key}" aria-pressed="${state.selected === item.key}"><b>${h(item.label)}</b><span>${h(item.description)}</span></button>`,
      )
      .join("") || '<div class="empty">No matching items</div>';
}

function renderContextDetail() {
  const state = contextReaderState;
  const item = state?.items.find((entry) => entry.key === state.selected);
  const root = document.getElementById("context-item-detail");
  if (!root) return;
  if (!item) {
    root.innerHTML = '<div class="empty">No items in this category</div>';
    return;
  }
  const text = contextItemText(item);
  const chars = [...text].length;
  root.innerHTML = `<div class="context-detail-heading"><div><h3>${h(item.label)}</h3><p>${h(item.kind)} · ${number(chars)} chars</p></div><button class="outline-button" data-context-copy="item">${icon("copy", 12)} Copy item</button></div>${item.kind === "Skills" ? '<p class="context-skill-note">Available skill summary supplied to the model. This does not indicate that its body was read or executed.</p>' : ""}<pre class="context-code" tabindex="0"><code>${highlightContext(text, typeof item.value !== "string")}</code></pre>${item.raw == null ? "" : `<details class="context-original"><summary>Original item JSON</summary><pre class="context-code" tabindex="0"><code>${highlightContext(JSON.stringify(item.raw, null, 2), true)}</code></pre></details>`}`;
  root.scrollTop = 0;
}

/**
 * 打开首请求上下文阅读器弹窗。
 * @param {RequestDetailResponse | null} detail
 * @param {Element | null} [trigger]
 */
export function openContextReader(detail, trigger) {
  if (!detail?.request_context || contextReaderState) return;
  hooks.hideTooltip();
  const items = contextItems(detail.request_context);
  const selected = items.find((item) => item.label === "System prompt") || items[0];
  const background = [
    ...document.querySelectorAll("body > header,body > #sidebar,body > main,#overlay"),
  ].filter((el) => el instanceof HTMLElement);
  contextReaderState = {
    detail,
    items,
    selected: selected.key,
    category: selected.kind,
    query: "",
    focus: trigger || document.activeElement,
    overflow: document.body.style.overflow,
    inert: background.map((element) => [element, element.inert]),
  };
  for (const element of background) element.inert = true;
  document.body.style.overflow = "hidden";
  const overlay = document.getElementById("context-overlay");
  const reader = document.getElementById("context-reader");
  if (!overlay || !reader) return;
  overlay.hidden = false;
  reader.innerHTML = `<div class="context-reader-heading"><div><h2>Request context <span>#${detail.request.id}</span></h2><p>First request snapshot · ${h(detail.request.model)} · ${h(detail.request.api)}</p></div><div class="context-reader-actions"><button class="outline-button" data-context-copy="request">${icon("copy", 12)} Copy full request</button><button class="close" data-context-close aria-label="Close context reader">${icon("close", 18)}</button></div></div><div class="context-reader-layout"><div class="context-sidebar"><label for="context-search">Filter snapshot items</label><input id="context-search" type="search" placeholder="Name or description…" autocomplete="off"><div id="context-tabs" role="tablist" aria-label="Context categories"></div><div id="context-items" aria-label="Snapshot items"></div></div><div id="context-item-detail" class="context-item-detail"></div></div>`;
  renderContextList();
  renderContextDetail();
  /** @type {HTMLElement | null} */ (reader.querySelector("[data-context-close]"))?.focus({
    preventScroll: true,
  });
}

/**
 * 关闭上下文阅读器并恢复触发前焦点与背景交互状态。
 */
export function closeContextReader() {
  const state = contextReaderState;
  if (!state) return;
  const overlay = document.getElementById("context-overlay");
  const reader = document.getElementById("context-reader");
  if (overlay) overlay.hidden = true;
  for (const [element, inert] of state.inert) element.inert = inert;
  document.body.style.overflow = state.overflow;
  contextReaderState = null;
  const focus = state.focus?.isConnected
    ? state.focus
    : document.querySelector(`[data-context-id="${state.detail.request.id}"]`);
  if (focus instanceof HTMLElement) focus.focus({ preventScroll: true });
  reader?.replaceChildren();
}

/**
 * 处理上下文阅读器弹窗内的键盘焦点环与 Tab 切换。
 * @param {KeyboardEvent} event
 * @returns {boolean}
 */
export function contextReaderKeydown(event) {
  if (!contextReaderState) return false;
  if (event.key === "Escape") {
    event.preventDefault();
    closeContextReader();
  } else if (event.key === "Tab") {
    const reader = document.getElementById("context-reader");
    if (!reader) return true;
    const elements = [...reader.querySelectorAll('button,input,summary,[tabindex="0"]')].filter(
      (element) => element instanceof HTMLElement && element.getClientRects().length,
    );
    const first = elements[0];
    const last = elements.at(-1);
    if (event.shiftKey && document.activeElement === first && last instanceof HTMLElement) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last && first instanceof HTMLElement) {
      event.preventDefault();
      first.focus();
    }
  } else if (
    ["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key) &&
    event.target instanceof Element &&
    event.target.matches("[data-context-category]")
  ) {
    event.preventDefault();
    const tabsEl = document.getElementById("context-tabs");
    if (!tabsEl) return true;
    const tabs = [...tabsEl.querySelectorAll("button")];
    const index = tabs.indexOf(/** @type {HTMLButtonElement} */ (event.target));
    const next =
      event.key === "Home"
        ? 0
        : event.key === "End"
          ? tabs.length - 1
          : (index + (event.key === "ArrowRight" ? 1 : -1) + tabs.length) % tabs.length;
    tabs[next]?.click();
    tabsEl.querySelectorAll("button")[next]?.focus();
  }
  return true;
}

let contextBound = false;

/**
 * 绑定上下文阅读器浮层点击与搜索过滤事件（幂等）。
 */
export function initContextReader() {
  if (contextBound) return;
  contextBound = true;
  const overlay = document.getElementById("context-overlay");
  overlay?.addEventListener("click", async (event) => {
    const target = event.target;
    if (!(target instanceof Element)) return;
    if (target.id === "context-overlay" || target.closest("[data-context-close]")) {
      closeContextReader();
      return;
    }
    const state = contextReaderState;
    if (!state) return;
    const category = target.closest("[data-context-category]");
    if (category instanceof HTMLElement) {
      state.category = /** @type {"Context" | "Tools" | "Skills"} */ (
        category.dataset.contextCategory || "Context"
      );
      state.selected =
        state.items.find((item) => item.kind === state.category)?.key || state.selected;
      renderContextList();
      renderContextDetail();
      /** @type {HTMLElement | null} */ (
        document.querySelector(`[data-context-category="${state.category}"]`)
      )?.focus({ preventScroll: true });
      return;
    }
    const item = target.closest("[data-context-item]");
    if (item instanceof HTMLElement) {
      state.selected = item.dataset.contextItem || state.selected;
      renderContextList();
      renderContextDetail();
      /** @type {HTMLElement | null} */ (
        document.querySelector(`[data-context-item="${state.selected}"]`)
      )?.focus({ preventScroll: true });
      return;
    }
    const copy = target.closest("[data-context-copy]");
    if (copy instanceof HTMLElement) {
      const selected = state.items.find((entry) => entry.key === state.selected);
      const text =
        copy.dataset.contextCopy === "request"
          ? JSON.stringify(state.detail.request_context?.request, null, 2)
          : selected
            ? contextItemText(selected)
            : null;
      if (text == null) return;
      try {
        await navigator.clipboard.writeText(text);
        hooks.toast("Copied to clipboard");
      } catch {
        hooks.toast("Copy failed. Select the text and copy manually.");
      }
    }
  });
  overlay?.addEventListener("input", (event) => {
    if (
      event.target instanceof HTMLInputElement &&
      event.target.id === "context-search" &&
      contextReaderState
    ) {
      contextReaderState.query = event.target.value;
      renderContextList();
    }
  });
}
