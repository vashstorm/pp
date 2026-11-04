let contextReaderState = null;
let drawerContextDetail = null;

function contextItems(snapshot) {
  const request = snapshot.request;
  const items = [];
  const add = (key, kind, label, value, description = "", raw = null) => {
    items.push({ key, kind, label, value, description, raw });
  };
  if (request.instructions != null)
    add("instructions", "Context", "System prompt", request.instructions);
  const messages = request.messages ?? request.input;
  if (Array.isArray(messages)) {
    messages.forEach((message, index) => {
      const role = message?.role || message?.type || "item";
      const system = role === "system" || role === "developer";
      const label = system ? "System prompt" : role === "user" ? "User Prompt" : `${index + 1}. ${role}`;
      const text = typeof message?.content === "string" ? message.content : message;
      add(`message-${index}`, "Context", label,
        text, `Original ${request.messages ? "messages" : "input"}[${index}]`,
        typeof text === "string" ? message : null);
    });
  } else if (messages != null) {
    add("input", "Context", "User Prompt", messages);
  }
  const parameters = Object.fromEntries(Object.entries(request)
    .filter(([key]) => !["instructions", "messages", "input", "tools"].includes(key)));
  add("parameters", "Context", "Request parameters", parameters);
  const tools = Array.isArray(request.tools) ? request.tools : [];
  tools.forEach((tool, index) => {
    const definition = tool?.function || tool;
    add(`tool-${index}`, "Tools", definition?.name || tool?.type || `Tool ${index + 1}`,
      tool, definition?.description || "Full tool definition and schema");
  });
  const skills = Array.isArray(snapshot.skills) ? snapshot.skills : [];
  skills.forEach((skill, index) => {
    add(`skill-${index}`, "Skills", skill.name || `Skill ${index + 1}`,
      skill, skill.description || skill.uri || "");
  });
  add("native", "Context", "Full request JSON", request, "Complete original request body");
  return items;
}

function requestContextButton(detail) {
  return `<button class="outline-button context-button"${detail.request_context
    ? ` data-context-id="${detail.request.id}" title="Read the full context sent with this session's first request"`
    : ' disabled title="Context was not recorded for this request"'}>${icon("system", 14)} Context</button>`;
}

function requestContextNote(detail) {
  return detail.request_context
    ? '<span class="first-request-badge" title="Session first request · full context saved">First request</span>'
    : '<span class="context-note">Context not recorded</span>';
}

function highlightContext(text, json) {
  const tokens = json
    ? /"(?:\\.|[^"\\])*"|\b(?:true|false|null)\b|-?\b\d+(?:\.\d+)?(?:[eE][+-]?\d+)?\b/g
    : /^#{1,6}[^\n]*$|`[^`\n]+`|<\/?[A-Za-z][^>\n]*>|^\s*(?:[-*+]|\d+\.)\s+/gm;
  let result = "", cursor = 0;
  for (const match of text.matchAll(tokens)) {
    result += h(text.slice(cursor, match.index));
    let kind = "marker";
    if (json) {
      kind = match[0].startsWith('"')
        ? (/^\s*:/.test(text.slice(match.index + match[0].length)) ? "key" : "string")
        : "literal";
    } else if (match[0].startsWith("`")) kind = "string";
    else if (match[0].startsWith("<")) kind = "tag";
    result += `<span class="context-token-${kind}">${h(match[0])}</span>`;
    cursor = match.index + match[0].length;
  }
  return result + h(text.slice(cursor));
}

function contextItemText(item) {
  return typeof item.value === "string" ? item.value : JSON.stringify(item.value, null, 2);
}

function renderContextList() {
  const state = contextReaderState;
  if (!state) return;
  const query = state.query.trim().toLowerCase();
  const items = state.items.filter((item) => item.kind === state.category &&
    (!query || `${item.label}\n${item.description}`.toLowerCase().includes(query)));
  document.getElementById("context-tabs").innerHTML = ["Context", "Tools", "Skills"].map((kind) =>
    `<button role="tab" aria-selected="${state.category === kind}" data-context-category="${kind}">${kind}<span>${state.items.filter((item) => item.kind === kind).length}</span></button>`).join("");
  document.getElementById("context-items").innerHTML = items.map((item) =>
    `<button class="context-item${state.selected === item.key ? " selected" : ""}" data-context-item="${item.key}" aria-pressed="${state.selected === item.key}"><b>${h(item.label)}</b><span>${h(item.description)}</span></button>`).join("") ||
    '<div class="empty">No matching items</div>';
}

function renderContextDetail() {
  const state = contextReaderState;
  const item = state?.items.find((item) => item.key === state.selected);
  const root = document.getElementById("context-item-detail");
  if (!item) {
    root.innerHTML = '<div class="empty">No items in this category</div>';
    return;
  }
  const text = contextItemText(item);
  const chars = [...text].length;
  root.innerHTML = `<div class="context-detail-heading"><div><h3>${h(item.label)}</h3><p>${h(item.kind)} · ${number(chars)} chars</p></div><button class="outline-button" data-context-copy="item">${icon("copy", 12)} Copy item</button></div>${item.kind === "Skills" ? '<p class="context-skill-note">Available skill summary supplied to the model. This does not indicate that its body was read or executed.</p>' : ""}<pre class="context-code" tabindex="0"><code>${highlightContext(text, typeof item.value !== "string")}</code></pre>${item.raw == null ? "" : `<details class="context-original"><summary>Original item JSON</summary><pre class="context-code" tabindex="0"><code>${highlightContext(JSON.stringify(item.raw, null, 2), true)}</code></pre></details>`}`;
  root.scrollTop = 0;
}

function openContextReader(detail, trigger) {
  if (!detail?.request_context || contextReaderState) return;
  hideTooltip();
  const items = contextItems(detail.request_context);
  const selected = items.find((item) => item.label === "System prompt") || items[0];
  const background = [...document.querySelectorAll("body > header,body > aside,body > main,#overlay")];
  contextReaderState = {
    detail, items, selected: selected.key, category: selected.kind, query: "",
    focus: trigger || document.activeElement,
    overflow: document.body.style.overflow,
    inert: background.map((element) => [element, element.inert]),
  };
  background.forEach((element) => { element.inert = true; });
  document.body.style.overflow = "hidden";
  document.getElementById("context-overlay").hidden = false;
  document.getElementById("context-reader").innerHTML = `<div class="context-reader-heading"><div><h2>Request context <span>#${detail.request.id}</span></h2><p>First request snapshot · ${h(detail.request.model)} · ${h(detail.request.api)}</p></div><div class="context-reader-actions"><button class="outline-button" data-context-copy="request">${icon("copy", 12)} Copy full request</button><button class="close" data-context-close aria-label="Close context reader">${icon("close", 18)}</button></div></div><div class="context-reader-layout"><div class="context-sidebar"><label for="context-search">Filter snapshot items</label><input id="context-search" type="search" placeholder="Name or description…" autocomplete="off"><div id="context-tabs" role="tablist" aria-label="Context categories"></div><div id="context-items" aria-label="Snapshot items"></div></div><div id="context-item-detail" class="context-item-detail"></div></div>`;
  renderContextList();
  renderContextDetail();
  document.getElementById("context-reader").querySelector("[data-context-close]").focus({ preventScroll: true });
}

function closeContextReader() {
  const state = contextReaderState;
  if (!state) return;
  document.getElementById("context-overlay").hidden = true;
  state.inert.forEach(([element, inert]) => { element.inert = inert; });
  document.body.style.overflow = state.overflow;
  contextReaderState = null;
  const focus = state.focus?.isConnected ? state.focus
    : document.querySelector(`[data-context-id="${state.detail.request.id}"]`);
  focus?.focus({ preventScroll: true });
  document.getElementById("context-reader").replaceChildren();
}

function contextReaderKeydown(event) {
  if (!contextReaderState) return false;
  if (event.key === "Escape") {
    event.preventDefault();
    closeContextReader();
  } else if (event.key === "Tab") {
    const elements = [...document.getElementById("context-reader")
      .querySelectorAll('button,input,summary,[tabindex="0"]')]
      .filter((element) => element.getClientRects().length);
    const first = elements[0], last = elements.at(-1);
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last?.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first?.focus();
    }
  } else if (["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key) &&
    event.target.matches("[data-context-category]")) {
    event.preventDefault();
    const tabs = [...document.getElementById("context-tabs").querySelectorAll("button")];
    const index = tabs.indexOf(event.target);
    const next = event.key === "Home" ? 0 : event.key === "End" ? tabs.length - 1
      : (index + (event.key === "ArrowRight" ? 1 : -1) + tabs.length) % tabs.length;
    tabs[next].click();
    document.getElementById("context-tabs").querySelectorAll("button")[next].focus();
  }
  return true;
}

document.getElementById("context-overlay").addEventListener("click", async (event) => {
  if (event.target.id === "context-overlay" || event.target.closest("[data-context-close]")) {
    closeContextReader();
    return;
  }
  const state = contextReaderState;
  if (!state) return;
  const category = event.target.closest("[data-context-category]");
  if (category) {
    state.category = category.dataset.contextCategory;
    state.selected = state.items.find((item) => item.kind === state.category)?.key;
    renderContextList();
    renderContextDetail();
    document.querySelector(`[data-context-category="${state.category}"]`).focus({ preventScroll: true });
    return;
  }
  const item = event.target.closest("[data-context-item]");
  if (item) {
    state.selected = item.dataset.contextItem;
    renderContextList();
    renderContextDetail();
    document.querySelector(`[data-context-item="${state.selected}"]`).focus({ preventScroll: true });
    return;
  }
  const copy = event.target.closest("[data-context-copy]");
  if (copy) {
    const selected = state.items.find((item) => item.key === state.selected);
    const text = copy.dataset.contextCopy === "request"
      ? JSON.stringify(state.detail.request_context.request, null, 2)
      : selected ? contextItemText(selected) : null;
    if (text == null) return;
    try {
      await navigator.clipboard.writeText(text);
      toast("Copied to clipboard");
    } catch {
      toast("Copy failed. Select the text and copy manually.");
    }
  }
});
document.getElementById("context-overlay").addEventListener("input", (event) => {
  if (event.target.id === "context-search" && contextReaderState) {
    contextReaderState.query = event.target.value;
    renderContextList();
  }
});
