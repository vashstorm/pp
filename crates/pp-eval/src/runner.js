"use strict";

/**
 * pp JavaScript eval runner — persistent NDJSON REPL kernel (Bun / Node.js)
 * with isolated control FDs, top-level await, persistent bindings, and duplex tool bridge.
 */

const fs = require("node:fs");
const path = require("node:path");
const util = require("node:util");
const vm = require("node:vm");
const { Console } = require("node:console");
const { Writable } = require("node:stream");
const { createRequire } = require("node:module");

// ---------------------------------------------------------------------------
// Control channel & warning suppression
// ---------------------------------------------------------------------------

const CTRL_IN_FD = Number.parseInt(process.env.PP_EVAL_CTRL_IN_FD || "", 10);
const CTRL_OUT_FD = Number.parseInt(process.env.PP_EVAL_CTRL_OUT_FD || "", 10);
delete process.env.PP_EVAL_CTRL_IN_FD;
delete process.env.PP_EVAL_CTRL_OUT_FD;

if (!Number.isInteger(CTRL_IN_FD) || !Number.isInteger(CTRL_OUT_FD)) {
  process.stderr.write("Missing PP_EVAL_CTRL_IN_FD / PP_EVAL_CTRL_OUT_FD\n");
  process.exit(1);
}

// Suppress Node's ExperimentalWarning for vm.USE_MAIN_CONTEXT_DEFAULT_LOADER so user stderr stays clean.
const _origEmitWarning = process.emitWarning.bind(process);
process.emitWarning = function ppEmitWarning(warning, ...rest) {
  const msg = typeof warning === "string" ? warning : warning && warning.message;
  if (typeof msg === "string" && msg.includes("USE_MAIN_CONTEXT_DEFAULT_LOADER")) {
    return;
  }
  return _origEmitWarning(warning, ...rest);
};

let _inBuf = Buffer.alloc(0);
let _activeRid = null;
let _executionCount = 0;
let _shutdown = false;
let _abortAsync = null;
const _interruptedRids = new Set();

function _writeAllSync(fd, buf) {
  let offset = 0;
  while (offset < buf.length) {
    try {
      const n = fs.writeSync(fd, buf, offset, buf.length - offset, null);
      if (n <= 0) {
        break;
      }
      offset += n;
    } catch (err) {
      if (err && (err.code === "EINTR" || err.code === "EAGAIN")) {
        continue;
      }
      throw err;
    }
  }
}

function _emit(frame) {
  const line = JSON.stringify(frame) + "\n";
  _writeAllSync(CTRL_OUT_FD, Buffer.from(line, "utf8"));
}

function _readFrameSync() {
  const chunk = Buffer.allocUnsafe(8192);
  while (true) {
    const nl = _inBuf.indexOf(10);
    if (nl !== -1) {
      const rawLine = _inBuf.subarray(0, nl).toString("utf8").trim();
      _inBuf = _inBuf.subarray(nl + 1);
      if (!rawLine) {
        continue;
      }
      try {
        return JSON.parse(rawLine);
      } catch (err) {
        _emit({
          type: "error",
          id: "",
          ename: "ProtocolError",
          evalue: `Invalid JSON request: ${err && err.message ? err.message : String(err)}`,
          traceback: [],
        });
        continue;
      }
    }
    let n = 0;
    try {
      n = fs.readSync(CTRL_IN_FD, chunk, 0, chunk.length, null);
    } catch (err) {
      if (err && (err.code === "EINTR" || err.code === "EAGAIN")) {
        continue;
      }
      return null;
    }
    if (n === 0) {
      return null;
    }
    _inBuf = Buffer.concat([_inBuf, chunk.subarray(0, n)]);
  }
}

function _startParentWatchdog() {
  const originalPpid = process.ppid;
  if (!originalPpid || originalPpid <= 1) {
    return;
  }
  const timer = setInterval(() => {
    try {
      if (process.ppid !== originalPpid) {
        process.exit(0);
      }
    } catch {
      process.exit(0);
    }
  }, 2000);
  if (typeof timer.unref === "function") {
    timer.unref();
  }
}

// ---------------------------------------------------------------------------
// Stream capture, rich console & image extraction
// ---------------------------------------------------------------------------

const _origStdoutWrite = process.stdout.write.bind(process.stdout);
const _origStderrWrite = process.stderr.write.bind(process.stderr);

const _streamBuffers = { stdout: "", stderr: "" };
const _MAX_STREAM_BUFFER = 8192;

function _appendStream(kind, chunk, encoding) {
  const text = Buffer.isBuffer(chunk)
    ? chunk.toString(typeof encoding === "string" && Buffer.isEncoding(encoding) ? encoding : "utf8")
    : String(chunk);
  if (!text) {
    return;
  }
  if (_activeRid === null) {
    if (kind === "stdout") {
      _origStdoutWrite(text);
    } else {
      _origStderrWrite(text);
    }
    return;
  }
  const combined = _streamBuffers[kind] + text;
  const nl = combined.lastIndexOf("\n");
  if (nl >= 0) {
    const emitPart = combined.slice(0, nl + 1);
    _streamBuffers[kind] = combined.slice(nl + 1);
    _emit({ type: kind, id: _activeRid, data: emitPart });
  } else if (combined.length >= _MAX_STREAM_BUFFER) {
    _streamBuffers[kind] = "";
    _emit({ type: kind, id: _activeRid, data: combined });
  } else {
    _streamBuffers[kind] = combined;
  }
}

function _flushStreams(rid) {
  const targetRid = rid !== undefined ? rid : _activeRid;
  for (const kind of ["stdout", "stderr"]) {
    const pending = _streamBuffers[kind];
    _streamBuffers[kind] = "";
    if (pending && targetRid !== null) {
      _emit({ type: kind, id: targetRid, data: pending });
    }
  }
}

process.stdout.write = function ppStdoutWrite(chunk, encoding, callback) {
  if (typeof encoding === "function") {
    callback = encoding;
    encoding = undefined;
  }
  _appendStream("stdout", chunk, encoding);
  if (typeof callback === "function") {
    callback();
  }
  return true;
};

process.stderr.write = function ppStderrWrite(chunk, encoding, callback) {
  if (typeof encoding === "function") {
    callback = encoding;
    encoding = undefined;
  }
  _appendStream("stderr", chunk, encoding);
  if (typeof callback === "function") {
    callback();
  }
  return true;
};

function _imageBase64(data) {
  if (typeof data === "string") {
    return data.length > 0 ? data : null;
  }
  if (Buffer.isBuffer(data)) {
    return data.length > 0 ? data.toString("base64") : null;
  }
  if (ArrayBuffer.isView(data)) {
    const buf = Buffer.from(data.buffer, data.byteOffset, data.byteLength);
    return buf.length > 0 ? buf.toString("base64") : null;
  }
  if (data instanceof ArrayBuffer) {
    const buf = Buffer.from(data);
    return buf.length > 0 ? buf.toString("base64") : null;
  }
  return null;
}

function _extractImage(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return null;
  }
  if (value.type !== "image" || typeof value.mimeType !== "string" || !value.mimeType) {
    return null;
  }
  const b64 = _imageBase64(value.data);
  if (!b64) {
    return null;
  }
  return { mimeType: value.mimeType, data: b64 };
}

function _emitImage(img) {
  if (_activeRid === null) {
    return;
  }
  _flushStreams(_activeRid);
  _emit({ type: "image", id: _activeRid, image: img });
}

function _makeWritableStream(kind) {
  return new Writable({
    write(chunk, encoding, cb) {
      _appendStream(kind, chunk, encoding);
      cb();
    },
  });
}

function _createRichConsole() {
  const base = new Console({
    stdout: _makeWritableStream("stdout"),
    stderr: _makeWritableStream("stderr"),
    colorMode: false,
  });

  function wrapLog(method) {
    const orig = base[method].bind(base);
    return function wrappedConsoleLog(...args) {
      const images = [];
      const mapped = args.map((arg) => {
        const img = _extractImage(arg);
        if (img) {
          images.push(img);
          return `<image ${img.mimeType}>`;
        }
        return arg;
      });
      orig(...mapped);
      for (const img of images) {
        _emitImage(img);
      }
    };
  }

  base.log = wrapLog("log");
  base.info = wrapLog("info");
  base.debug = wrapLog("debug");
  base.warn = wrapLog("warn");
  base.error = wrapLog("error");
  return base;
}

const _richConsole = _createRichConsole();

function _echoValue(value) {
  if (value === undefined) {
    return;
  }
  const img = _extractImage(value);
  if (img) {
    _appendStream("stdout", `<image ${img.mimeType}>\n`);
    _emitImage(img);
    return;
  }
  const text =
    typeof value === "string"
      ? util.inspect(value, { colors: false, maxStringLength: Infinity })
      : util.inspect(value, {
          depth: 4,
          maxArrayLength: 100,
          maxStringLength: 10000,
          colors: false,
        });
  _appendStream("stdout", `${text}\n`);
}

// ---------------------------------------------------------------------------
// Duplex tool bridge & prelude helpers
// ---------------------------------------------------------------------------

class ToolError extends Error {
  constructor(result) {
    const message = String((result && result.error) || "tool call failed");
    super(message);
    this.name = "ToolError";
    this.result = result;
  }
}

class ShellError extends Error {
  constructor(message, output, exitCode) {
    super(message);
    this.name = "ShellError";
    this.output = output;
    this.exitCode = exitCode;
  }
}

function _makeInterruptedError(message) {
  const err = new Error(message || "Execution interrupted");
  err.name = "InterruptError";
  err.code = "ERR_SCRIPT_EXECUTION_INTERRUPTED";
  return err;
}

let _callSeq = 0;

function _callBridge(name, args) {
  if (_activeRid === null) {
    throw new Error("tool bridge cannot be called outside an active cell");
  }
  if (_interruptedRids.has(_activeRid) || _shutdown) {
    throw _makeInterruptedError("Execution interrupted");
  }
  _callSeq += 1;
  const callId = `bridge-${_callSeq}`;
  const rid = _activeRid;
  _flushStreams(rid);
  _emit({
    type: "tool_call",
    id: rid,
    call_id: callId,
    name,
    args: args && typeof args === "object" ? args : {},
  });

  while (true) {
    const frame = _readFrameSync();
    if (!frame || frame.type === "exit") {
      _shutdown = true;
      throw _makeInterruptedError("Kernel shutting down");
    }
    if (frame.type === "interrupt") {
      const target = String(frame.id || "");
      if (!target || target === rid) {
        _interruptedRids.add(rid);
        throw _makeInterruptedError("Execution interrupted");
      }
      continue;
    }
    if (frame.type === "tool_result" && String(frame.call_id || "") === callId) {
      if (frame.error !== undefined && frame.error !== null) {
        throw new Error(String(frame.error));
      }
      const val = frame.value;
      if (val && typeof val === "object" && !Array.isArray(val)) {
        return val;
      }
      return { ok: true, value: val };
    }
  }
}

const _toolNamespace = new Proxy(
  {},
  {
    get(_target, prop) {
      if (typeof prop !== "string" || prop.startsWith("_")) {
        return undefined;
      }
      if (prop === "then") {
        return undefined;
      }
      return function toolCall(args) {
        if (args !== undefined && (typeof args !== "object" || args === null || Array.isArray(args))) {
          throw new TypeError(`tool.${prop}(...) expects an object of arguments`);
        }
        const res = _callBridge(prop, args || {});
        if (res.ok === false) {
          throw new ToolError(res);
        }
        return res;
      };
    },
  }
);

function _readHelper(targetPath, opts) {
  let pathStr = String(targetPath);
  const options = opts && typeof opts === "object" ? opts : {};
  const offset = options.offset !== undefined && options.offset !== null ? Number(options.offset) : null;
  const limit = options.limit !== undefined && options.limit !== null ? Number(options.limit) : null;
  const raw = options.raw === undefined ? true : Boolean(options.raw);

  const hasScheme =
    pathStr.includes("://") && !pathStr.startsWith("http://") && !pathStr.startsWith("https://");
  let hasSelector = false;
  if (pathStr.includes(":") && !hasScheme) {
    const idx = pathStr.lastIndexOf(":");
    const tail = pathStr.slice(idx + 1);
    if (tail === "raw" || /^-?\d+([+-]\d+|-\d*)?$/.test(tail)) {
      hasSelector = true;
    }
  }

  if (!hasSelector) {
    const suffix =
      raw && !pathStr.startsWith("http://") && !pathStr.startsWith("https://") ? ":raw" : "";
    if (offset !== null || limit !== null) {
      const start = Math.max(1, offset || 1);
      if (limit !== null) {
        if (limit <= 0) {
          return "";
        }
        pathStr = `${pathStr}${suffix}:${start}+${limit}`;
      } else {
        pathStr = `${pathStr}${suffix}:${start}`;
      }
    } else if (suffix) {
      pathStr = `${pathStr}${suffix}`;
    }
  }

  const res = _callBridge("read", { path: pathStr });
  if (res.ok === false) {
    throw new ToolError(res);
  }
  return typeof res.content === "string" ? res.content : JSON.stringify(res);
}

function _writeHelper(targetPath, content) {
  const res = _callBridge("write", { path: String(targetPath), content: String(content) });
  if (res.ok === false) {
    throw new ToolError(res);
  }
  return res;
}

function _rgHelper(pattern, targetPath, opts) {
  const args = Object.assign({}, opts && typeof opts === "object" ? opts : {}, {
    pattern: String(pattern),
  });
  if (targetPath !== undefined && targetPath !== null) {
    if (typeof targetPath === "object" && opts === undefined) {
      Object.assign(args, targetPath);
    } else {
      args.path = String(targetPath);
    }
  }
  const res = _callBridge("rg", args);
  if (res.ok === false) {
    throw new ToolError(res);
  }
  return res;
}

function _globHelper(pattern, targetPath, opts) {
  const args = Object.assign({}, opts && typeof opts === "object" ? opts : {}, {
    pattern: String(pattern),
  });
  if (targetPath !== undefined && targetPath !== null) {
    if (typeof targetPath === "object" && opts === undefined) {
      Object.assign(args, targetPath);
    } else {
      args.path = String(targetPath);
    }
  }
  const res = _callBridge("glob", args);
  if (res.ok === false) {
    throw new ToolError(res);
  }
  return res;
}

function _shHelper(command, timeoutSeconds) {
  if (typeof command !== "string") {
    throw new TypeError(`sh() expects a command string (got ${typeof command})`);
  }
  const args = { command };
  if (timeoutSeconds !== undefined && timeoutSeconds !== null) {
    args.timeout_seconds = Math.trunc(Number(timeoutSeconds));
  }
  const res = _callBridge("bash", args);
  const output = String(res.output || "");
  const exitCode = typeof res.exit_code === "number" ? res.exit_code : null;
  if (res.ok === true) {
    return output;
  }
  if (typeof res.error === "string" && res.error) {
    throw new ShellError(res.error, output, exitCode);
  }
  const trimmed = output.trimEnd();
  const msg = trimmed
    ? `$ ${command}: exited with code ${exitCode}\n${trimmed}`
    : `$ ${command}: exited with code ${exitCode}`;
  throw new ShellError(msg, output, exitCode);
}

function _envHelper(key, value) {
  if (key === undefined || key === null) {
    const out = {};
    for (const k of Object.keys(process.env).sort()) {
      out[k] = process.env[k];
    }
    return out;
  }
  const k = String(key);
  if (value !== undefined) {
    const v = String(value);
    process.env[k] = v;
    return v;
  }
  return process.env[k];
}

// ---------------------------------------------------------------------------
// Persistent VM context
// ---------------------------------------------------------------------------

function _createVmContext(cwd) {
  const filename = path.join(cwd, "__pp_eval__.cjs");
  const ctxObj = {
    globalThis: null,
    global: null,
    console: _richConsole,
    Buffer,
    URL,
    URLSearchParams,
    TextEncoder,
    TextDecoder,
    AbortController,
    AbortSignal,
    setTimeout,
    setInterval,
    setImmediate: typeof setImmediate === "function" ? setImmediate : (fn, ...a) => setTimeout(fn, 0, ...a),
    clearTimeout,
    clearInterval,
    clearImmediate: typeof clearImmediate === "function" ? clearImmediate : clearTimeout,
    queueMicrotask,
    fetch: globalThis.fetch,
    crypto: globalThis.crypto,
    structuredClone: globalThis.structuredClone,
    performance: globalThis.performance,
    process,
    require: createRequire(filename),
    __dirname: cwd,
    __filename: filename,
    tool: _toolNamespace,
    ToolError,
    ShellError,
    read: _readHelper,
    write: _writeHelper,
    rg: _rgHelper,
    glob: _globHelper,
    sh: _shHelper,
    env: _envHelper,
  };
  if (typeof globalThis.Bun !== "undefined") {
    ctxObj.Bun = globalThis.Bun;
  }
  ctxObj.globalThis = ctxObj;
  ctxObj.global = ctxObj;
  return vm.createContext(ctxObj);
}

let _vmContext = _createVmContext(process.cwd());

// ---------------------------------------------------------------------------
// Lexical analysis & cell transformation (persistent bindings + top-level await)
// ---------------------------------------------------------------------------

const _KEYWORDS_NO_REGEX_AFTER = new Set([
  "return",
  "case",
  "throw",
  "delete",
  "void",
  "typeof",
  "yield",
  "await",
  "in",
  "instanceof",
  "of",
  "new",
  "else",
  "do",
]);

function _isIdentStart(ch) {
  return (
    (ch >= "a" && ch <= "z") ||
    (ch >= "A" && ch <= "Z") ||
    ch === "_" ||
    ch === "$" ||
    ch.charCodeAt(0) > 127
  );
}

function _isIdentPart(ch) {
  return _isIdentStart(ch) || (ch >= "0" && ch <= "9");
}

function _canPrecedeRegex(prevTokenType, prevWord) {
  if (prevTokenType === "start" || prevTokenType === "punct") {
    return true;
  }
  if (prevTokenType === "word") {
    return _KEYWORDS_NO_REGEX_AFTER.has(prevWord);
  }
  return false;
}

function _peekNonWs(source, startIdx) {
  let i = startIdx;
  const n = source.length;
  while (i < n) {
    if (/\s/.test(source[i])) {
      i += 1;
    } else if (source[i] === "/" && source[i + 1] === "/") {
      i += 2;
      while (i < n && source[i] !== "\n") i += 1;
    } else if (source[i] === "/" && source[i + 1] === "*") {
      i += 2;
      while (i < n && !(source[i] === "*" && source[i + 1] === "/")) i += 1;
      i = Math.min(n, i + 2);
    } else {
      return source[i];
    }
  }
  return "";
}

/**
 * Scan JS source into top-level statement spans while tracking lexical state.
 * Also reports whether a top-level `await` token exists outside nested functions.
 */
function _analyzeTopLevel(source) {
  const n = source.length;
  let i = 0;
  let parenDepth = 0;
  let bracketDepth = 0;
  let braceDepth = 0;
  let stmtStart = 0;
  const statements = [];
  let hasTopLevelAwait = false;

  // Stack of template brace depths so `${ ... }` returns cleanly to template string scanning.
  const templateStack = [];
  let prevTokenType = "start";
  let prevWord = "";
  // Track nested function body brace depths accurately (ignoring concise arrow bodies).
  const fnBraceStack = [];
  let pendingArrowBrace = false;
  // Track `function ...(...) {` parameter list parens before entering its body `{`.
  let fnHeaderState = null; // null | "seen_fn" | "in_params" | "ready_for_brace"
  let fnHeaderParenBase = 0;

  function hasCodeContent(snippet) {
    let idx = 0;
    const len = snippet.length;
    while (idx < len) {
      if (/\s/.test(snippet[idx])) {
        idx += 1;
      } else if (snippet[idx] === "/" && snippet[idx + 1] === "/") {
        idx += 2;
        while (idx < len && snippet[idx] !== "\n") idx += 1;
      } else if (snippet[idx] === "/" && snippet[idx + 1] === "*") {
        idx += 2;
        while (idx < len && !(snippet[idx] === "*" && snippet[idx + 1] === "/")) idx += 1;
        idx = Math.min(len, idx + 2);
      } else {
        return true;
      }
    }
    return false;
  }

  function flushStatement(endIdx) {
    const raw = source.slice(stmtStart, endIdx);
    if (hasCodeContent(raw)) {
      statements.push({ start: stmtStart, end: endIdx, text: raw });
    } else if (raw.length > 0 && statements.length > 0) {
      const prev = statements[statements.length - 1];
      prev.end = endIdx;
      prev.text += raw;
    } else if (raw.length > 0 && endIdx === n && statements.length === 0) {
      // Source contains only comments/whitespace; keep as-is.
    }
    stmtStart = endIdx;
  }

  while (i < n) {
    const ch = source[i];
    const next = i + 1 < n ? source[i + 1] : "";

    // Line comment
    if (ch === "/" && next === "/") {
      i += 2;
      while (i < n && source[i] !== "\n") {
        i += 1;
      }
      continue;
    }

    // Block comment
    if (ch === "/" && next === "*") {
      i += 2;
      while (i < n && !(source[i] === "*" && source[i + 1] === "/")) {
        i += 1;
      }
      i = Math.min(n, i + 2);
      continue;
    }

    // Single / double quoted string
    if (ch === "'" || ch === '"') {
      const quote = ch;
      i += 1;
      while (i < n) {
        if (source[i] === "\\") {
          i += 2;
          continue;
        }
        if (source[i] === quote) {
          i += 1;
          break;
        }
        i += 1;
      }
      prevTokenType = "expr";
      prevWord = "";
      continue;
    }

    // Template literal
    if (ch === "`") {
      i += 1;
      while (i < n) {
        if (source[i] === "\\") {
          i += 2;
          continue;
        }
        if (source[i] === "`") {
          i += 1;
          prevTokenType = "expr";
          prevWord = "";
          break;
        }
        if (source[i] === "$" && source[i + 1] === "{") {
          templateStack.push(braceDepth);
          braceDepth += 1;
          i += 2;
          prevTokenType = "punct";
          prevWord = "";
          break;
        }
        i += 1;
      }
      continue;
    }

    // Regex literal vs division operator
    if (ch === "/" && _canPrecedeRegex(prevTokenType, prevWord)) {
      i += 1;
      let inClass = false;
      while (i < n) {
        const rc = source[i];
        if (rc === "\\") {
          i += 2;
          continue;
        }
        if (rc === "[") {
          inClass = true;
        } else if (rc === "]" && inClass) {
          inClass = false;
        } else if (rc === "/" && !inClass) {
          i += 1;
          while (i < n && /[a-zA-Z]/.test(source[i])) {
            i += 1;
          }
          break;
        } else if (rc === "\n") {
          break;
        }
        i += 1;
      }
      prevTokenType = "expr";
      prevWord = "";
      continue;
    }

    // Resume template literal after matching `${ ... }`
    if (ch === "}" && templateStack.length > 0 && templateStack[templateStack.length - 1] === braceDepth - 1) {
      templateStack.pop();
      braceDepth -= 1;
      i += 1;
      while (i < n) {
        if (source[i] === "\\") {
          i += 2;
          continue;
        }
        if (source[i] === "`") {
          i += 1;
          prevTokenType = "expr";
          prevWord = "";
          break;
        }
        if (source[i] === "$" && source[i + 1] === "{") {
          templateStack.push(braceDepth);
          braceDepth += 1;
          i += 2;
          prevTokenType = "punct";
          prevWord = "";
          break;
        }
        i += 1;
      }
      continue;
    }

    if (ch === "(") {
      if (fnHeaderState === "seen_fn") {
        fnHeaderState = "in_params";
        fnHeaderParenBase = parenDepth;
      }
      parenDepth += 1;
      prevTokenType = "punct";
      prevWord = "";
      i += 1;
      continue;
    }
    if (ch === ")") {
      if (parenDepth > 0) parenDepth -= 1;
      if (fnHeaderState === "in_params" && parenDepth === fnHeaderParenBase) {
        fnHeaderState = "ready_for_brace";
      }
      prevTokenType = "expr";
      prevWord = "";
      i += 1;
      continue;
    }
    if (ch === "[") {
      bracketDepth += 1;
      prevTokenType = "punct";
      prevWord = "";
      i += 1;
      continue;
    }
    if (ch === "]") {
      if (bracketDepth > 0) bracketDepth -= 1;
      prevTokenType = "expr";
      prevWord = "";
      i += 1;
      continue;
    }
    if (ch === "{") {
      if (pendingArrowBrace || fnHeaderState === "ready_for_brace") {
        fnBraceStack.push(braceDepth);
        pendingArrowBrace = false;
        fnHeaderState = null;
      }
      braceDepth += 1;
      prevTokenType = "punct";
      prevWord = "";
      i += 1;
      continue;
    }
    if (ch === "}") {
      if (braceDepth > 0) braceDepth -= 1;
      if (fnBraceStack.length > 0 && fnBraceStack[fnBraceStack.length - 1] === braceDepth) {
        fnBraceStack.pop();
      }
      prevTokenType = "expr";
      prevWord = "";
      i += 1;
      continue;
    }

    if (ch === "=" && next === ">") {
      if (_peekNonWs(source, i + 2) === "{") {
        pendingArrowBrace = true;
      }
      prevTokenType = "punct";
      prevWord = "";
      i += 2;
      continue;
    }

    // Semicolon at true top-level ends a statement
    if (ch === ";" && parenDepth === 0 && bracketDepth === 0 && braceDepth === 0) {
      i += 1;
      flushStatement(i);
      prevTokenType = "punct";
      prevWord = "";
      continue;
    }

    // Newline at true top-level: check if previous statement is syntactically complete (ASI)
    if (ch === "\n" && parenDepth === 0 && bracketDepth === 0 && braceDepth === 0) {
      const candidate = source.slice(stmtStart, i);
      if (
        candidate.trim().length > 0 &&
        !_continuesOnNextLine(source, i + 1) &&
        _canEndStatement(candidate)
      ) {
        i += 1;
        flushStatement(i);
        prevTokenType = "punct";
        prevWord = "";
        continue;
      }
      i += 1;
      continue;
    }

    // Identifiers / keywords
    if (_isIdentStart(ch)) {
      const start = i;
      i += 1;
      while (i < n && _isIdentPart(source[i])) {
        i += 1;
      }
      const word = source.slice(start, i);
      if (word === "function" && prevWord !== ".") {
        fnHeaderState = "seen_fn";
      }
      if (
        word === "await" &&
        fnBraceStack.length === 0 &&
        prevWord !== "."
      ) {
        hasTopLevelAwait = true;
      }
      prevTokenType = "word";
      prevWord = word;
      continue;
    }

    // Numbers
    if (ch >= "0" && ch <= "9") {
      while (i < n && /[0-9a-zA-Z_.]/.test(source[i])) {
        i += 1;
      }
      prevTokenType = "expr";
      prevWord = "";
      continue;
    }

    // Whitespace
    if (/\s/.test(ch)) {
      i += 1;
      continue;
    }

    // Operators & punctuation
    if (ch === "." && prevWord !== undefined) {
      prevTokenType = "punct";
      prevWord = ".";
      i += 1;
      continue;
    }
    if ((ch === "+" && next === "+") || (ch === "-" && next === "-")) {
      prevTokenType = "expr";
      prevWord = "";
      i += 2;
      continue;
    }

    prevTokenType = "punct";
    prevWord = "";
    i += 1;
  }

  flushStatement(n);
  return { statements, hasTopLevelAwait };
}

function _continuesOnNextLine(source, startIdx) {
  let i = startIdx;
  const n = source.length;
  while (i < n) {
    if (/\s/.test(source[i])) {
      i += 1;
    } else if (source[i] === "/" && source[i + 1] === "/") {
      i += 2;
      while (i < n && source[i] !== "\n") i += 1;
    } else if (source[i] === "/" && source[i + 1] === "*") {
      i += 2;
      while (i < n && !(source[i] === "*" && source[i + 1] === "/")) i += 1;
      i = Math.min(n, i + 2);
    } else {
      break;
    }
  }
  if (i >= n) return false;
  const rest = source.slice(i);
  if (/^(else|catch|finally|while)\b/.test(rest)) {
    return true;
  }
  const ch = rest[0];
  return (
    ch === "." ||
    ch === "[" ||
    ch === "(" ||
    ch === "?" ||
    ch === ":" ||
    rest.startsWith("&&") ||
    rest.startsWith("||")
  );
}

function _stripTrailingSemicolon(snippet) {
  let end = snippet.length;
  while (end > 0) {
    // Trim trailing whitespace
    while (end > 0 && /\s/.test(snippet[end - 1])) {
      end -= 1;
    }
    // Trim trailing /* ... */ block comment
    if (end >= 4 && snippet.slice(end - 2, end) === "*/") {
      const open = snippet.lastIndexOf("/*", end - 4);
      if (open >= 0) {
        end = open;
        continue;
      }
    }
    break;
  }
  // Also handle trailing line comment on the last line
  const lastNl = snippet.lastIndexOf("\n", end - 1);
  const lastLineStart = lastNl >= 0 ? lastNl + 1 : 0;
  let codeEnd = end;
  let inStr = null;
  for (let i = lastLineStart; i < end; i += 1) {
    const c = snippet[i];
    if (inStr) {
      if (c === "\\") {
        i += 1;
        continue;
      }
      if (c === inStr) inStr = null;
    } else if (c === "'" || c === '"' || c === "`") {
      inStr = c;
    } else if (c === "/" && snippet[i + 1] === "/") {
      codeEnd = i;
      break;
    }
  }
  while (codeEnd > 0 && /\s/.test(snippet[codeEnd - 1])) {
    codeEnd -= 1;
  }
  if (codeEnd > 0 && snippet[codeEnd - 1] === ";") {
    return snippet.slice(0, codeEnd - 1) + snippet.slice(codeEnd);
  }
  return snippet;
}

function _canEndStatement(snippet) {
  const trimmed = snippet.trim();
  if (!trimmed) return false;
  // Don't split before a trailing binary operator or incomplete header.
  if (/[+\-*/%&|^=<>!?:.,]$/.test(trimmed) && !/(\+\+|--)$/.test(trimmed)) {
    return false;
  }
  try {
    // Wrap in an async function so top-level `await` statements parse cleanly during ASI check.
    new vm.Script(`async function __pp_asi__() {\n${snippet}\n}`);
    return true;
  } catch {
    return false;
  }
}

function _isExpressionSnippet(snippet) {
  const trimmed = _stripTrailingSemicolon(snippet).trim();
  if (!trimmed) return false;
  const firstWordMatch = trimmed.match(/^([A-Za-z_$][A-Za-z0-9_$]*)\b/);
  if (firstWordMatch) {
    const kw = firstWordMatch[1];
    if (
      kw === "var" ||
      kw === "let" ||
      kw === "const" ||
      kw === "class" ||
      kw === "function" ||
      kw === "if" ||
      kw === "for" ||
      kw === "while" ||
      kw === "do" ||
      kw === "switch" ||
      kw === "try" ||
      kw === "throw" ||
      kw === "return" ||
      kw === "break" ||
      kw === "continue" ||
      kw === "debugger"
    ) {
      return false;
    }
    if (kw === "async" && /^async\s+function\b/.test(trimmed)) {
      return false;
    }
  }
  if (trimmed.startsWith("{")) {
    // Distinguish `{ a: 1 }` (expression) from `{ const a = 1; }` (block).
    try {
      new vm.Script(`async () => (\n${trimmed}\n)`);
      return true;
    } catch {
      return false;
    }
  }
  try {
    new vm.Script(`async () => (\n${trimmed}\n)`);
    return true;
  } catch {
    return false;
  }
}

/**
 * Extract bound variable names from a `var`/`let`/`const` declaration header (after the keyword).
 */
function _extractDeclNames(declBody) {
  const names = [];
  let i = 0;
  const n = declBody.length;

  function skipWsAndComments() {
    while (i < n) {
      if (/\s/.test(declBody[i])) {
        i += 1;
      } else if (declBody[i] === "/" && declBody[i + 1] === "/") {
        i += 2;
        while (i < n && declBody[i] !== "\n") i += 1;
      } else if (declBody[i] === "/" && declBody[i + 1] === "*") {
        i += 2;
        while (i < n && !(declBody[i] === "*" && declBody[i + 1] === "/")) i += 1;
        i = Math.min(n, i + 2);
      } else {
        break;
      }
    }
  }

  function skipBalanced(openCh, closeCh) {
    let depth = 1;
    i += 1;
    while (i < n && depth > 0) {
      const ch = declBody[i];
      if (ch === "'" || ch === '"' || ch === "`") {
        const q = ch;
        i += 1;
        while (i < n) {
          if (declBody[i] === "\\") {
            i += 2;
            continue;
          }
          if (declBody[i] === q) {
            i += 1;
            break;
          }
          i += 1;
        }
        continue;
      }
      if (ch === openCh) depth += 1;
      else if (ch === closeCh) depth -= 1;
      i += 1;
    }
  }

  function skipInitializer() {
    let paren = 0;
    let brack = 0;
    let brace = 0;
    while (i < n) {
      const ch = declBody[i];
      if (ch === "'" || ch === '"' || ch === "`") {
        const q = ch;
        i += 1;
        while (i < n) {
          if (declBody[i] === "\\") {
            i += 2;
            continue;
          }
          if (declBody[i] === q) {
            i += 1;
            break;
          }
          i += 1;
        }
        continue;
      }
      if (ch === "(") paren += 1;
      else if (ch === ")" && paren > 0) paren -= 1;
      else if (ch === "[") brack += 1;
      else if (ch === "]" && brack > 0) brack -= 1;
      else if (ch === "{") brace += 1;
      else if (ch === "}" && brace > 0) brace -= 1;
      else if (paren === 0 && brack === 0 && brace === 0 && (ch === "," || ch === ";")) {
        break;
      }
      i += 1;
    }
  }

  function readIdent() {
    skipWsAndComments();
    if (i < n && _isIdentStart(declBody[i])) {
      const start = i;
      i += 1;
      while (i < n && _isIdentPart(declBody[i])) i += 1;
      return declBody.slice(start, i);
    }
    return null;
  }

  function readPattern() {
    skipWsAndComments();
    if (i >= n) return;
    const ch = declBody[i];
    if (ch === "{") {
      i += 1;
      while (i < n) {
        skipWsAndComments();
        if (declBody[i] === "}") {
          i += 1;
          break;
        }
        if (declBody.startsWith("...", i)) {
          i += 3;
          const restId = readIdent();
          if (restId) names.push(restId);
          skipWsAndComments();
          if (declBody[i] === ",") i += 1;
          continue;
        }
        if (declBody[i] === "[") {
          skipBalanced("[", "]");
        } else if (declBody[i] === "'" || declBody[i] === '"') {
          const q = declBody[i];
          i += 1;
          while (i < n) {
            if (declBody[i] === "\\") {
              i += 2;
              continue;
            }
            if (declBody[i] === q) {
              i += 1;
              break;
            }
            i += 1;
          }
        } else {
          const key = readIdent();
          skipWsAndComments();
          if (declBody[i] === ":") {
            i += 1;
            readPattern();
          } else if (key) {
            names.push(key);
          }
        }
        skipWsAndComments();
        if (declBody[i] === ":") {
          i += 1;
          readPattern();
          skipWsAndComments();
        }
        if (declBody[i] === "=") {
          i += 1;
          let p = 0,
            b = 0,
            c = 0;
          while (i < n) {
            const dc = declBody[i];
            if (dc === "(") p += 1;
            else if (dc === ")" && p > 0) p -= 1;
            else if (dc === "[") b += 1;
            else if (dc === "]" && b > 0) b -= 1;
            else if (dc === "{") {
              c += 1;
            } else if (dc === "}") {
              if (c === 0 && p === 0 && b === 0) break;
              c -= 1;
            } else if (dc === "," && p === 0 && b === 0 && c === 0) {
              break;
            }
            i += 1;
          }
        }
        if (declBody[i] === ",") {
          i += 1;
        }
      }
    } else if (ch === "[") {
      i += 1;
      while (i < n) {
        skipWsAndComments();
        if (declBody[i] === "]") {
          i += 1;
          break;
        }
        if (declBody[i] === ",") {
          i += 1;
          continue;
        }
        if (declBody.startsWith("...", i)) {
          i += 3;
        }
        readPattern();
        skipWsAndComments();
        if (declBody[i] === "=") {
          i += 1;
          let p = 0,
            b = 0,
            c = 0;
          while (i < n) {
            const dc = declBody[i];
            if (dc === "(") p += 1;
            else if (dc === ")" && p > 0) p -= 1;
            else if (dc === "[") b += 1;
            else if (dc === "]") {
              if (b === 0 && p === 0 && c === 0) break;
              b -= 1;
            } else if (dc === "{") c += 1;
            else if (dc === "}" && c > 0) c -= 1;
            else if (dc === "," && p === 0 && b === 0 && c === 0) break;
            i += 1;
          }
        }
        if (declBody[i] === ",") {
          i += 1;
        }
      }
    } else {
      const id = readIdent();
      if (id) names.push(id);
    }
  }

  while (i < n) {
    skipWsAndComments();
    if (i >= n || declBody[i] === ";") break;
    readPattern();
    skipWsAndComments();
    if (declBody[i] === "=") {
      i += 1;
      skipInitializer();
    }
    skipWsAndComments();
    if (declBody[i] === ",") {
      i += 1;
      continue;
    }
    break;
  }

  return names;
}

function _rewriteStatement(stmtText, persistToGlobal) {
  const leadingMatch = stmtText.match(/^(\s*(?:\/\/[^\n]*\n|\s*\/\*[\s\S]*?\*\/\s*)*)([\s\S]*)$/);
  const leading = leadingMatch ? leadingMatch[1] : "";
  const body = leadingMatch ? leadingMatch[2] : stmtText;
  const trimmed = body.trimEnd();
  const trailing = body.slice(trimmed.length);

  // Rewrite top-level `const` / `let` -> `var` so redeclarations across cells never fail.
  const varMatch = trimmed.match(/^(const|let|var)\b([\s\S]*)$/);
  if (varMatch) {
    const rest = varMatch[2];
    const cleanRest = rest.replace(/;\s*$/, "");
    let rewritten = `${leading}var${rest}`;
    if (persistToGlobal) {
      const names = _extractDeclNames(cleanRest);
      if (names.length > 0) {
        const syncs = names.map((name) => `globalThis.${name} = ${name};`).join(" ");
        const semi = trimmed.endsWith(";") ? "" : ";";
        rewritten = `${leading}var${rest}${semi} ${syncs}${trailing}`;
      }
    }
    return rewritten;
  }

  // Rewrite top-level `class Foo ...` -> `var Foo = class Foo ...` so redeclarations across cells work.
  const classMatch = trimmed.match(/^class\s+([A-Za-z_$][A-Za-z0-9_$]*)\b([\s\S]*)$/);
  if (classMatch) {
    const name = classMatch[1];
    const bodyWithoutSemi = classMatch[2].replace(/;\s*$/, "");
    const sync = persistToGlobal ? ` globalThis.${name} = ${name};` : "";
    return `${leading}var ${name} = class ${name}${bodyWithoutSemi};${sync}${trailing}`;
  }

  // Persist top-level `function` / `async function` declarations when inside async IIFE.
  const fnMatch = trimmed.match(/^(?:async\s+)?function\s*\*?\s*([A-Za-z_$][A-Za-z0-9_$]*)\b/);
  if (fnMatch && persistToGlobal) {
    const name = fnMatch[1];
    return `${leading}${trimmed}\nglobalThis.${name} = ${name};${trailing}`;
  }

  return stmtText;
}

function _transformCell(source) {
  const { statements, hasTopLevelAwait } = _analyzeTopLevel(source);
  if (statements.length === 0) {
    return { code: source, isAsync: false };
  }

  if (!hasTopLevelAwait) {
    const code = statements.map((s) => _rewriteStatement(s.text, false)).join("");
    return { code, isAsync: false };
  }

  const lastIdx = statements.length - 1;
  const parts = statements.map((s, idx) => {
    if (idx === lastIdx && _isExpressionSnippet(s.text)) {
      const cleaned = _stripTrailingSemicolon(s.text).trim();
      return `return (\n${cleaned}\n);`;
    }
    return _rewriteStatement(s.text, true);
  });

  const wrapped = `(async () => {\n${parts.join("")}\n})()`;
  return { code: wrapped, isAsync: true };
}

// ---------------------------------------------------------------------------
// Error formatting & execution loop
// ---------------------------------------------------------------------------

function _emitError(rid, err, sourceFilename) {
  let ename = "Error";
  let evalue = "";
  let traceback = [];

  if (err && typeof err === "object") {
    ename = err.name ? String(err.name) : "Error";
    evalue = err.message ? String(err.message) : String(err);
    const stack = typeof err.stack === "string" ? err.stack : "";
    if (stack) {
      const lines = stack.split("\n").filter((line) => !line.includes(__filename));
      traceback = lines.length > 0 ? lines : [`${ename}: ${evalue}`];
    } else {
      traceback = [`${ename}: ${evalue}`];
    }
  } else {
    evalue = String(err);
    traceback = [`Error: ${evalue}`];
  }

  if (sourceFilename && traceback.length === 0) {
    traceback = [`${ename}: ${evalue}`];
  }

  _emit({
    type: "error",
    id: rid,
    ename,
    evalue,
    traceback,
  });
}

function _isInterruptedError(err) {
  if (!err || typeof err !== "object") {
    return false;
  }
  return (
    err.code === "ERR_SCRIPT_EXECUTION_INTERRUPTED" ||
    err.name === "InterruptError"
  );
}

process.on("SIGINT", () => {
  if (_activeRid !== null) {
    _interruptedRids.add(_activeRid);
    if (_abortAsync) {
      _abortAsync(_makeInterruptedError("Execution interrupted"));
    }
  }
});

function _applyRequestRuntime(req) {
  const cwd = req.cwd;
  if (typeof cwd === "string" && cwd) {
    process.chdir(cwd);
    const filename = path.join(cwd, "__pp_eval__.cjs");
    _vmContext.__dirname = cwd;
    _vmContext.__filename = filename;
    _vmContext.require = createRequire(filename);
  }
}

async function _handleExec(req) {
  const rid = String(req.id || "");
  _executionCount += 1;
  const execCount = _executionCount;
  _activeRid = rid;
  _interruptedRids.delete(rid);
  _emit({ type: "started", id: rid });

  let status = "ok";
  let cancelled = false;
  const filename =
    typeof req.filename === "string" && req.filename ? req.filename : `<cell-${execCount}>`;

  try {
    _applyRequestRuntime(req);
    const source = String(req.code || "");
    const { code, isAsync } = _transformCell(source);
    const runOpts = {
      filename,
      breakOnSigint: true,
    };
    if (vm.constants && vm.constants.USE_MAIN_CONTEXT_DEFAULT_LOADER) {
      runOpts.importModuleDynamically = vm.constants.USE_MAIN_CONTEXT_DEFAULT_LOADER;
    }

    let result = vm.runInContext(code, _vmContext, runOpts);
    if (isAsync || (result && typeof result.then === "function")) {
      result = await new Promise((resolve, reject) => {
        if (_interruptedRids.has(rid)) {
          reject(_makeInterruptedError("Execution interrupted"));
          return;
        }
        _abortAsync = reject;
        Promise.resolve(result).then(resolve, reject);
      });
    }
    _echoValue(result);
  } catch (err) {
    status = "error";
    if (_isInterruptedError(err) || _interruptedRids.has(rid)) {
      cancelled = true;
      _emitError(rid, _makeInterruptedError("Execution interrupted"), filename);
    } else {
      _emitError(rid, err, filename);
    }
  } finally {
    _abortAsync = null;
    _flushStreams(rid);
    _activeRid = null;
    _interruptedRids.delete(rid);
    _emit({
      type: "done",
      id: rid,
      status,
      executionCount: execCount,
      cancelled,
    });
  }
}

async function main() {
  _startParentWatchdog();
  while (true) {
    const req = _readFrameSync();
    if (!req || req.type === "exit") {
      _shutdown = true;
      process.exit(0);
    }
    if (req.type === "interrupt") {
      const rid = String(req.id || "");
      if (rid) {
        _interruptedRids.add(rid);
      }
      continue;
    }
    if (req.type !== "exec") {
      continue;
    }
    await _handleExec(req);
  }
}

main().catch((err) => {
  try {
    _emitError(_activeRid || "", err);
  } catch {
    // ignore
  }
  process.exit(1);
});
