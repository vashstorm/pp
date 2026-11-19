"""pp Python eval runner — persistent NDJSON REPL kernel with duplex tool bridge."""

from __future__ import annotations

import ast
import asyncio
import base64
import builtins
import contextvars
import functools
import importlib
import inspect
import io
import json
import linecache
import os
import queue
import re
import select
import shlex
import shutil
import signal
import subprocess
import sys
import threading
import time
import traceback
from pathlib import Path
from typing import Any, Callable


# ---------------------------------------------------------------------------
# Private control FDs & stdio isolation
# ---------------------------------------------------------------------------

try:
    _FRAME_FD = os.dup(sys.__stdout__.fileno())
    _FRAME_OUT = os.fdopen(_FRAME_FD, "w", encoding="utf-8", errors="backslashreplace")
    _CAP_OUT_R, _CAP_OUT_W = os.pipe()
    os.dup2(_CAP_OUT_W, sys.__stdout__.fileno())
    os.close(_CAP_OUT_W)
except (AttributeError, OSError, ValueError, io.UnsupportedOperation):
    _FRAME_OUT = sys.__stdout__
    _CAP_OUT_R = None

try:
    _RAW_STDERR_FD = os.dup(sys.__stderr__.fileno())
    _RAW_STDERR = os.fdopen(_RAW_STDERR_FD, "w", encoding="utf-8", errors="backslashreplace")
    _CAP_ERR_R, _CAP_ERR_W = os.pipe()
    os.dup2(_CAP_ERR_W, sys.__stderr__.fileno())
    os.close(_CAP_ERR_W)
except (AttributeError, OSError, ValueError, io.UnsupportedOperation):
    _RAW_STDERR = sys.__stderr__
    _CAP_ERR_R = None

try:
    _CONTROL_FD = os.dup(sys.__stdin__.fileno())
    _CONTROL_IN = os.fdopen(_CONTROL_FD, "r", encoding="utf-8", errors="replace")
    _DEVNULL_FD = os.open(os.devnull, os.O_RDONLY)
    os.dup2(_DEVNULL_FD, sys.__stdin__.fileno())
    os.close(_DEVNULL_FD)
except (AttributeError, OSError, ValueError, io.UnsupportedOperation):
    _CONTROL_IN = sys.__stdin__

_OUT_LOCK = threading.Lock()
_CURRENT_RID: contextvars.ContextVar[str | None] = contextvars.ContextVar("pp_eval_rid", default=None)
_SHOWN_FIGURES: contextvars.ContextVar[set[int] | None] = contextvars.ContextVar("pp_eval_figs", default=None)


class _KernelState:
    def __init__(self) -> None:
        self.loop: asyncio.AbstractEventLoop | None = None
        self.active_rid: str | None = None
        self.active_task: asyncio.Task[Any] | None = None
        self.interrupted_rids: set[str] = set()
        self.execution_count: int = 0
        self.user_ns: dict[str, Any] = {"__name__": "__main__"}
        self.lock = threading.Lock()
        self.shutdown = False


_STATE = _KernelState()


def _json_default(obj: Any) -> Any:
    try:
        return repr(obj)
    except Exception:
        return f"<unrepr {type(obj).__name__}>"


def _emit(frame: dict[str, Any]) -> None:
    line = json.dumps(frame, ensure_ascii=False, default=_json_default)
    with _OUT_LOCK:
        _FRAME_OUT.write(line)
        _FRAME_OUT.write("\n")
        _FRAME_OUT.flush()


class _StreamProxy(io.TextIOBase):
    """Buffered per-request stdout/stderr frame emitter."""

    _MAX_BUFFER = 8192

    def __init__(self, kind: str) -> None:
        super().__init__()
        self._kind = kind
        self._lock = threading.Lock()
        self._buffers: dict[str, str] = {}

    def writable(self) -> bool:
        return True

    def isatty(self) -> bool:
        return False

    def write(self, data: Any) -> int:  # type: ignore[override]
        if not isinstance(data, str):
            data = str(data)
        if not data:
            return 0
        rid = _CURRENT_RID.get() or _STATE.active_rid
        if rid is None:
            if _RAW_STDERR is not None:
                _RAW_STDERR.write(data)
                _RAW_STDERR.flush()
            return len(data)
        with self._lock:
            buf = self._buffers.get(rid, "") + data
            nl = buf.rfind("\n")
            if nl >= 0:
                emit_part = buf[: nl + 1]
                self._buffers[rid] = buf[nl + 1 :]
            elif len(buf) >= self._MAX_BUFFER:
                emit_part = buf
                self._buffers[rid] = ""
            else:
                self._buffers[rid] = buf
                emit_part = ""
        if emit_part:
            _emit({"type": self._kind, "id": rid, "data": emit_part})
        return len(data)

    def flush(self) -> None:
        rid = _CURRENT_RID.get() or _STATE.active_rid
        if rid is not None:
            self.flush_rid(rid)

    def flush_rid(self, rid: str) -> None:
        with self._lock:
            pending = self._buffers.pop(rid, "")
        if pending:
            _emit({"type": self._kind, "id": rid, "data": pending})


_CAP_DRAIN_LOCK = threading.Lock()


def _drain_fd_nonblocking(fd: int | None, kind: str, rid_override: str | None = None) -> None:
    if fd is None:
        return
    with _CAP_DRAIN_LOCK:
        while True:
            try:
                chunk = os.read(fd, 8192)
            except BlockingIOError:
                break
            except OSError:
                break
            if not chunk:
                break
            text = chunk.decode("utf-8", errors="replace")
            rid = rid_override or _CURRENT_RID.get() or _STATE.active_rid
            if rid is not None:
                _emit({"type": kind, "id": rid, "data": text})
            elif _RAW_STDERR is not None:
                try:
                    _RAW_STDERR.write(text)
                    _RAW_STDERR.flush()
                except Exception:
                    pass


def _flush_stream_proxies(rid: str) -> None:
    for stream in (sys.stdout, sys.stderr):
        if isinstance(stream, _StreamProxy):
            stream.flush_rid(rid)
    _drain_fd_nonblocking(_CAP_OUT_R, "stdout", rid_override=rid)
    _drain_fd_nonblocking(_CAP_ERR_R, "stderr", rid_override=rid)


def _start_fd_drains() -> None:
    pairs = [(fd, kind) for fd, kind in ((_CAP_OUT_R, "stdout"), (_CAP_ERR_R, "stderr")) if fd is not None]
    for fd, _ in pairs:
        os.set_blocking(fd, False)
    if not pairs:
        return

    def _watch() -> None:
        fds = [fd for fd, _ in pairs]
        kind_by_fd = dict(pairs)
        while not _STATE.shutdown and fds:
            try:
                readable, _, _ = select.select(fds, [], [], 0.2)
            except (OSError, ValueError):
                break
            for fd in readable:
                _drain_fd_nonblocking(fd, kind_by_fd[fd])

    threading.Thread(target=_watch, name="pp-eval-fd-drain", daemon=True).start()


def _start_parent_watchdog() -> None:
    if os.name != "posix":
        return
    original_ppid = os.getppid()
    if original_ppid <= 1:
        return

    def _watch() -> None:
        while not _STATE.shutdown:
            try:
                if os.getppid() != original_ppid:
                    os._exit(0)
            except Exception:
                return
            time.sleep(2)

    threading.Thread(target=_watch, name="pp-eval-parent-watch", daemon=True).start()


# ---------------------------------------------------------------------------
# Duplex tool bridge & prelude helpers
# ---------------------------------------------------------------------------

_PENDING_LOCK = threading.Lock()
_PENDING_TOOLS: dict[str, tuple[threading.Event, dict[str, Any]]] = {}
_CALL_SEQ = 0


class ToolError(RuntimeError):
    """Raised when a bridged pp host tool reports ok=False."""

    def __init__(self, result: dict[str, Any]) -> None:
        self.result = result
        message = str(result.get("error") or "tool call failed")
        super().__init__(message)


class ToolResult(dict):
    """Dictionary result that works both synchronously and with `await`."""

    def __await__(self):
        async def _identity():
            return self

        return _identity().__await__()


class ShellError(RuntimeError):
    """Raised by `sh(...)` on non-zero exit or timeout."""

    def __init__(self, message: str, output: str, exit_code: int | None) -> None:
        super().__init__(message)
        self.output = output
        self.exit_code = exit_code


def _abort_pending_tools(reason: str) -> None:
    with _PENDING_LOCK:
        items = list(_PENDING_TOOLS.values())
    for event, holder in items:
        holder.setdefault("error", reason)
        holder["interrupted"] = True
        event.set()


def _call_bridge(name: str, args: dict[str, Any]) -> ToolResult:
    global _CALL_SEQ
    rid = _CURRENT_RID.get() or _STATE.active_rid
    if rid is None:
        raise RuntimeError("tool bridge cannot be called outside an active cell")
    if rid in _STATE.interrupted_rids:
        raise KeyboardInterrupt("Execution interrupted")

    event = threading.Event()
    holder: dict[str, Any] = {}
    with _PENDING_LOCK:
        _CALL_SEQ += 1
        call_id = f"bridge-{_CALL_SEQ}"
        _PENDING_TOOLS[call_id] = (event, holder)

    _flush_stream_proxies(rid)
    try:
        _emit(
            {
                "type": "tool_call",
                "id": rid,
                "call_id": call_id,
                "name": name,
                "args": args,
            }
        )
        while not event.wait(timeout=0.05):
            if rid in _STATE.interrupted_rids or _STATE.shutdown:
                raise KeyboardInterrupt("Execution interrupted")
    finally:
        with _PENDING_LOCK:
            _PENDING_TOOLS.pop(call_id, None)

    if holder.get("interrupted"):
        raise KeyboardInterrupt(str(holder.get("error") or "Execution interrupted"))
    if "error" in holder:
        raise RuntimeError(str(holder["error"]))

    value = holder.get("value")
    if isinstance(value, dict):
        return ToolResult(value)
    return ToolResult({"ok": True, "value": value})


class _ToolCallable:
    __slots__ = ("_name",)

    def __init__(self, name: str) -> None:
        self._name = name

    def __repr__(self) -> str:
        return f"<tool.{self._name}>"

    def __call__(self, args: dict[str, Any] | None = None, /, **kwargs: Any) -> ToolResult:
        if args is None:
            merged: dict[str, Any] = {}
        elif isinstance(args, dict):
            merged = dict(args)
        else:
            raise TypeError(
                f"tool.{self._name}(...) expects a dict of arguments (got {type(args).__name__})"
            )
        merged.update(kwargs)
        result = _call_bridge(self._name, merged)
        if result.get("ok") is False:
            raise ToolError(result)
        return result


class _ToolNamespace:
    def __getattr__(self, name: str) -> _ToolCallable:
        if name.startswith("_"):
            raise AttributeError(name)
        return _ToolCallable(name)

    def __getitem__(self, name: str) -> _ToolCallable:
        return _ToolCallable(name)

    def __repr__(self) -> str:
        return "<pp tool bridge: call tool.<name>(...) or await tool.<name>(...)>"


def _read_helper(
    path: str | Path,
    offset: int | None = None,
    limit: int | None = None,
    *,
    raw: bool = True,
) -> str:
    path_str = str(path)
    has_scheme = "://" in path_str and not path_str.startswith(("http://", "https://"))
    has_selector = False
    if ":" in path_str and not has_scheme:
        # Leave explicit selectors intact; otherwise append :raw / range.
        tail = path_str.rsplit(":", 1)[-1]
        if tail == "raw" or re.match(r"^-?\d+([+-]\d+|-\d*)?$", tail):
            has_selector = True

    if not has_selector:
        suffix = ":raw" if raw and not path_str.startswith(("http://", "https://")) else ""
        if offset is not None or limit is not None:
            start = max(1, offset or 1)
            if limit is not None:
                if limit <= 0:
                    return ""
                path_str = f"{path_str}{suffix}:{start}+{limit}"
            else:
                path_str = f"{path_str}{suffix}:{start}"
        elif suffix:
            path_str = f"{path_str}{suffix}"

    result = _call_bridge("read", {"path": path_str})
    if result.get("ok") is False:
        raise ToolError(result)
    content = result.get("content")
    return content if isinstance(content, str) else json.dumps(result, ensure_ascii=False)


def _write_helper(path: str | Path, content: str) -> ToolResult:
    result = _call_bridge("write", {"path": str(path), "content": content})
    if result.get("ok") is False:
        raise ToolError(result)
    return result


def _rg_helper(pattern: str, path: str | Path | None = None, **kwargs: Any) -> ToolResult:
    args: dict[str, Any] = {"pattern": pattern, **kwargs}
    if path is not None:
        args["path"] = str(path)
    result = _call_bridge("rg", args)
    if result.get("ok") is False:
        raise ToolError(result)
    return result


def _glob_helper(pattern: str, path: str | Path | None = None, **kwargs: Any) -> ToolResult:
    args: dict[str, Any] = {"pattern": pattern, **kwargs}
    if path is not None:
        args["path"] = str(path)
    result = _call_bridge("glob", args)
    if result.get("ok") is False:
        raise ToolError(result)
    return result


def _sh_helper(command: str, timeout_seconds: int | None = None) -> str:
    if not isinstance(command, str):
        raise TypeError(f"sh() expects a command string (got {type(command).__name__})")
    args: dict[str, Any] = {"command": command}
    if timeout_seconds is not None:
        args["timeout_seconds"] = int(timeout_seconds)
    result = _call_bridge("bash", args)
    output = str(result.get("output") or "")
    exit_code = result.get("exit_code")
    if result.get("ok") is True:
        return output
    err = result.get("error")
    if isinstance(err, str) and err:
        raise ShellError(err, output, exit_code if isinstance(exit_code, int) else None)
    raise ShellError(
        f"$ {command}: exited with code {exit_code}\n{output.rstrip()}".rstrip(),
        output,
        exit_code if isinstance(exit_code, int) else None,
    )


def _env_helper(key: str | None = None, value: str | None = None) -> Any:
    if key is None:
        return dict(sorted(os.environ.items()))
    if value is not None:
        os.environ[key] = value
        return value
    return os.environ.get(key)


# ---------------------------------------------------------------------------
# Rich image capture & print integration
# ---------------------------------------------------------------------------

_REAL_PRINT = builtins.print


def _emit_image(image: dict[str, Any]) -> bool:
    rid = _CURRENT_RID.get() or _STATE.active_rid
    if rid is None:
        return False
    _flush_stream_proxies(rid)
    _emit({"type": "image", "id": rid, "image": image})
    return True


def _image_base64(data: Any) -> str | None:
    if isinstance(data, (bytes, bytearray, memoryview)):
        return base64.b64encode(bytes(data)).decode("ascii") if data else None
    if isinstance(data, str):
        return data or None
    return None


def _image_payload(value: Any) -> dict[str, Any] | None:
    if not isinstance(value, dict) or not isinstance(value.get("mimeType"), str):
        return None
    data = _image_base64(value.get("data"))
    if data is None:
        return None
    return {"mimeType": value["mimeType"], "data": data}


def _omp_image(value: Any) -> dict[str, Any] | None:
    if isinstance(value, dict) and value.get("type") == "image":
        return _image_payload(value)
    return None


def _is_matplotlib_figure(value: Any) -> bool:
    figure_module = sys.modules.get("matplotlib.figure")
    figure_cls = getattr(figure_module, "Figure", None)
    if isinstance(figure_cls, type) and isinstance(value, figure_cls):
        return True
    value_type = type(value)
    return value_type.__module__ == "matplotlib.figure" and value_type.__name__ == "Figure"


def _png_payload(save: Callable[[io.BytesIO], Any]) -> dict[str, Any]:
    buf = io.BytesIO()
    save(buf)
    return {
        "mimeType": "image/png",
        "data": base64.b64encode(buf.getvalue()).decode("ascii"),
    }


def _value_image(value: Any) -> dict[str, Any] | None:
    if _is_matplotlib_figure(value):
        image = _png_payload(lambda buf: value.savefig(buf, format="png", bbox_inches="tight"))
        shown = _SHOWN_FIGURES.get()
        if shown is not None:
            shown.add(id(value))
        return image
    pil_image = getattr(sys.modules.get("PIL.Image"), "Image", None)
    if isinstance(pil_image, type) and isinstance(value, pil_image):
        try:
            return _png_payload(lambda buf: value.save(buf, format="PNG"))
        except Exception:
            pass
    for attr, mime in (("_repr_png_", "image/png"), ("_repr_jpeg_", "image/jpeg")):
        repr_fn = getattr(value, attr, None)
        if not callable(repr_fn):
            continue
        data = repr_fn()
        if isinstance(data, tuple):
            data = data[0] if data else None
        encoded = _image_base64(data)
        if encoded is not None:
            return {"mimeType": mime, "data": encoded}
    return None


def _rich_value(value: Any, to_text: Callable[[Any], str]) -> tuple[str, list[dict[str, Any]]]:
    if type(value) in (str, int, float, bool, type(None)):
        return to_text(value), []
    image = _omp_image(value)
    if image is not None:
        return f"<image {image['mimeType']}>", [image]
    if not isinstance(value, type):
        try:
            image = _value_image(value)
        except Exception:
            image = None
        if image is not None:
            return to_text(value), [image]
    return to_text(value), []


@functools.wraps(_REAL_PRINT)
def _rich_print(
    *args: Any,
    sep: str | None = " ",
    end: str | None = "\n",
    file: Any = None,
    flush: bool = False,
) -> None:
    target = sys.stdout if file is None else file
    if not isinstance(target, _StreamProxy) or target._kind != "stdout":
        _REAL_PRINT(*args, sep=sep, end=end, file=file, flush=flush)
        return
    rendered = [_rich_value(arg, str) for arg in args]
    _REAL_PRINT(*(text for text, _ in rendered), sep=sep, end=end, file=file, flush=flush)
    for _, images in rendered:
        for img in images:
            _emit_image(img)


builtins.print = _rich_print


def _echo_value(value: Any) -> None:
    text, images = _rich_value(value, repr)
    sys.stdout.write(f"{text}\n")
    for img in images:
        _emit_image(img)


def _flush_matplotlib_figures() -> None:
    plt = sys.modules.get("matplotlib.pyplot")
    if plt is None:
        return
    try:
        fignums = list(plt.get_fignums())
    except Exception:
        return
    shown = _SHOWN_FIGURES.get() or set()
    for num in fignums:
        try:
            fig = plt.figure(num)
            if id(fig) not in shown:
                _emit_image(
                    _png_payload(lambda buf: fig.savefig(buf, format="png", bbox_inches="tight"))
                )
            plt.close(fig)
        except Exception:
            continue


os.environ.setdefault("MPLBACKEND", "Agg")


# ---------------------------------------------------------------------------
# Magic transformation & builtins
# ---------------------------------------------------------------------------

_MAGIC_ASSIGN_RE = re.compile(
    r"^(\s*)([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(%[A-Za-z_][A-Za-z0-9_]*|!)(.*)$"
)
_LINE_MAGIC_RE = re.compile(r"^(\s*)%([A-Za-z_][A-Za-z0-9_]*)(.*)$")
_CELL_MAGIC_RE = re.compile(r"^%%([A-Za-z_][A-Za-z0-9_]*)(.*)$")


def _fold_continuations(lines: list[str], start: int) -> tuple[str, int]:
    parts = [lines[start]]
    idx = start
    while parts[-1].rstrip().endswith("\\") and idx + 1 < len(lines):
        parts[-1] = parts[-1].rstrip()[:-1] + " "
        idx += 1
        parts.append(lines[idx].lstrip())
    return "".join(parts), idx + 1


def _transform_cell(source: str) -> str:
    lines = source.splitlines(keepends=False)
    if not lines:
        return source

    first_stripped = lines[0].lstrip()
    cell_match = _CELL_MAGIC_RE.match(first_stripped)
    if cell_match:
        name = cell_match.group(1)
        args = cell_match.group(2).strip()
        body = "\n".join(lines[1:])
        return f"__pp_magic_cell({name!r}, {args!r}, {body!r})"

    out: list[str] = []
    i = 0
    while i < len(lines):
        line, next_i = _fold_continuations(lines, i)
        stripped = line.lstrip()
        indent = line[: len(line) - len(stripped)]

        assign = _MAGIC_ASSIGN_RE.match(line)
        if assign:
            a_indent, target, op, rest = assign.groups()
            rest = rest.strip()
            if op == "!":
                out.append(f"{a_indent}{target} = __pp_shell({rest!r})")
            else:
                name = op[1:]
                fn = "__pp_magic_async" if name == "load" else "__pp_magic"
                prefix = "await " if name == "load" else ""
                out.append(f"{a_indent}{target} = {prefix}{fn}({name!r}, {rest!r})")
            i = next_i
            continue

        if stripped.startswith("!"):
            cmd = stripped[1:].strip()
            out.append(f"{indent}__pp_shell({cmd!r})")
            i = next_i
            continue

        m = _LINE_MAGIC_RE.match(line)
        if m:
            m_indent, name, rest = m.groups()
            rest = rest.strip()
            if name == "load":
                out.append(f"{m_indent}await __pp_magic_async({name!r}, {rest!r})")
            else:
                out.append(f"{m_indent}__pp_magic({name!r}, {rest!r})")
            i = next_i
            continue

        out.append(line)
        i = next_i

    return "\n".join(out)


def _has_pip() -> bool:
    try:
        proc = subprocess.run(
            [sys.executable, "-m", "pip", "--version"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        return proc.returncode == 0
    except Exception:
        return False


def _run_streamed(cmd: list[str]) -> int:
    proc = subprocess.Popen(
        cmd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        errors="replace",
    )
    assert proc.stdout is not None
    try:
        for line in proc.stdout:
            sys.stdout.write(line)
            sys.stdout.flush()
    finally:
        proc.stdout.close()
    return proc.wait()


def _magic_pip(args: str) -> None:
    argv = shlex.split(args)
    if not argv:
        raise ValueError("Usage: %pip <pip arguments>")
    if _has_pip():
        code = _run_streamed([sys.executable, "-m", "pip", *argv])
    else:
        boot = subprocess.run(
            [sys.executable, "-m", "ensurepip", "--upgrade"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
        )
        if boot.returncode == 0 and _has_pip():
            code = _run_streamed([sys.executable, "-m", "pip", *argv])
        else:
            uv = shutil.which("uv")
            if uv is None:
                raise RuntimeError(
                    "pip is not installed in this Python interpreter and neither ensurepip nor uv succeeded"
                )
            code = _run_streamed([uv, "pip", *argv, "--python", sys.executable])
    importlib.invalidate_caches()
    if code != 0:
        raise RuntimeError(f"pip exited with code {code}")


def _magic_cd(args: str) -> str:
    target = shlex.split(args)[0] if args.strip() else os.path.expanduser("~")
    path = os.path.expanduser(target)
    os.chdir(path)
    return os.getcwd()


def _magic_pwd(_args: str) -> str:
    return os.getcwd()


def _magic_ls(args: str) -> list[str]:
    target = shlex.split(args)[0] if args.strip() else "."
    return sorted(os.listdir(os.path.expanduser(target)))


def _magic_env(args: str) -> Any:
    raw = args.strip()
    if not raw:
        return _env_helper()
    if "=" in raw:
        key, value = raw.split("=", 1)
        return _env_helper(key.strip(), value.strip())
    return _env_helper(raw)


def _magic_set_env(args: str) -> str:
    parts = args.strip().split(None, 1)
    if len(parts) != 2:
        raise ValueError("Usage: %set_env KEY VALUE")
    return _env_helper(parts[0], parts[1])


def _magic_time(args: str) -> Any:
    code = args.strip()
    if not code:
        raise ValueError("Usage: %time <statement>")
    t0 = time.perf_counter()
    compiled = compile(code, "<time>", "exec")
    exec(compiled, _STATE.user_ns)
    dt = time.perf_counter() - t0
    print(f"Wall time: {dt * 1000:.2f} ms")


def _magic_who(_args: str) -> list[str]:
    return sorted(
        k
        for k in _STATE.user_ns
        if not k.startswith("_") and k not in _PRELUDE_NAMES
    )


def _magic_whos(_args: str) -> list[tuple[str, str]]:
    items = []
    for k in _magic_who(""):
        v = _STATE.user_ns[k]
        items.append((k, type(v).__name__))
    return items


def _magic_reset(_args: str) -> None:
    _STATE.user_ns.clear()
    _STATE.user_ns["__name__"] = "__main__"
    _install_prelude(_STATE.user_ns)


async def _magic_load(args: str) -> None:
    parts = shlex.split(args)
    if len(parts) != 1:
        raise ValueError("Usage: %load <path>")
    file_path = os.path.abspath(os.path.expanduser(parts[0]))
    with open(file_path, "r", encoding="utf-8") as fh:
        source = fh.read()
    await _exec_source_async(source, _STATE.user_ns, filename=file_path, linecache_source=source)


def _magic_run(args: str) -> None:
    import runpy

    parts = shlex.split(args)
    if not parts:
        raise ValueError("Usage: %run <path>")
    file_path = os.path.abspath(os.path.expanduser(parts[0]))
    result_ns = runpy.run_path(file_path, init_globals={"__name__": "__main__"})
    _STATE.user_ns.update(result_ns)


class _ShellResult(list):
    @property
    def n(self) -> str:
        return "\n".join(self)

    @property
    def s(self) -> str:
        return " ".join(self)


def __pp_shell(cmd: str) -> _ShellResult:
    proc = subprocess.Popen(
        cmd,
        shell=True,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        errors="replace",
    )
    lines: list[str] = []
    assert proc.stdout is not None
    try:
        for line in proc.stdout:
            sys.stdout.write(line)
            lines.append(line.rstrip("\r\n"))
    finally:
        proc.stdout.close()
    rc = proc.wait()
    if rc != 0:
        raise ShellError(f"$ {cmd}: exited with code {rc}", "\n".join(lines), rc)
    return _ShellResult(lines)


_LINE_MAGICS: dict[str, Callable[[str], Any]] = {
    "pip": _magic_pip,
    "cd": _magic_cd,
    "pwd": _magic_pwd,
    "ls": _magic_ls,
    "env": _magic_env,
    "set_env": _magic_set_env,
    "time": _magic_time,
    "who": _magic_who,
    "whos": _magic_whos,
    "reset": _magic_reset,
    "run": _magic_run,
}


def __pp_magic(name: str, args: str) -> Any:
    fn = _LINE_MAGICS.get(name)
    if fn is None:
        raise NameError(f"UsageError: Line magic function `%{name}` not found.")
    return fn(args)


async def __pp_magic_async(name: str, args: str) -> Any:
    if name == "load":
        return await _magic_load(args)
    return __pp_magic(name, args)


def __pp_magic_cell(name: str, args: str, body: str) -> Any:
    if name == "bash":
        return __pp_shell(body)
    if name == "writefile":
        parts = shlex.split(args)
        if len(parts) != 1:
            raise ValueError("Usage: %%writefile <path>")
        target = Path(os.path.expanduser(parts[0]))
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(body, encoding="utf-8")
        return f"Writing {target}"
    if name == "capture":
        var_name = args.strip() or None
        buf_out, buf_err = io.StringIO(), io.StringIO()
        old_out, old_err = sys.stdout, sys.stderr
        try:
            sys.stdout, sys.stderr = buf_out, buf_err  # type: ignore[assignment]
            exec(compile(body, "<%%capture>", "exec"), _STATE.user_ns)
        finally:
            sys.stdout, sys.stderr = old_out, old_err
        captured = buf_out.getvalue() + buf_err.getvalue()
        if var_name:
            _STATE.user_ns[var_name] = captured
        return None
    if name == "timeit":
        import timeit

        timer = timeit.Timer(body, globals=_STATE.user_ns)
        loops, total = timer.autorange()
        per_loop_us = (total / loops) * 1e6
        print(f"{loops} loops, best of 1: {per_loop_us:.2f} us per loop")
        return None
    raise NameError(f"UsageError: Cell magic function `%%{name}` not found.")


_PRELUDE_NAMES = {
    "tool",
    "ToolError",
    "ToolResult",
    "ShellError",
    "read",
    "write",
    "rg",
    "glob",
    "sh",
    "env",
    "__pp_magic",
    "__pp_magic_async",
    "__pp_magic_cell",
    "__pp_shell",
}


def _install_prelude(ns: dict[str, Any]) -> None:
    ns["tool"] = _ToolNamespace()
    ns["ToolError"] = ToolError
    ns["ToolResult"] = ToolResult
    ns["ShellError"] = ShellError
    ns["read"] = _read_helper
    ns["write"] = _write_helper
    ns["rg"] = _rg_helper
    ns["glob"] = _glob_helper
    ns["sh"] = _sh_helper
    ns["env"] = _env_helper
    ns["__pp_magic"] = __pp_magic
    ns["__pp_magic_async"] = __pp_magic_async
    ns["__pp_magic_cell"] = __pp_magic_cell
    ns["__pp_shell"] = __pp_shell


_install_prelude(_STATE.user_ns)


# ---------------------------------------------------------------------------
# AST compilation & execution
# ---------------------------------------------------------------------------

_TLA_FLAG = getattr(ast, "PyCF_ALLOW_TOP_LEVEL_AWAIT", 0x2000)


def _compile_source(source: str, filename: str = "<cell>") -> tuple[Any, Any | None, bool]:
    module = ast.parse(source, filename, "exec")
    if not module.body:
        return None, None, False
    last = module.body[-1]
    if isinstance(last, ast.Expr):
        body_module = ast.Module(body=module.body[:-1], type_ignores=[])
        expr_module = ast.Expression(body=last.value)
        ast.copy_location(expr_module, last)
        ast.fix_missing_locations(body_module)
        ast.fix_missing_locations(expr_module)
        body_code = compile(body_module, filename, "exec", flags=_TLA_FLAG)
        expr_code = compile(expr_module, filename, "eval", flags=_TLA_FLAG)
        return body_code, expr_code, True
    return compile(module, filename, "exec", flags=_TLA_FLAG), None, False


def _prepare_file_source(
    source: str, ns: dict[str, Any], filename: str, linecache_source: str | None
) -> None:
    display_source = source if linecache_source is None else linecache_source
    lines = display_source.splitlines(keepends=True)
    if display_source and (not lines or not lines[-1].endswith(("\n", "\r"))):
        lines[-1] += "\n"
    linecache.cache[filename] = (len(display_source), None, lines, filename)
    if not (filename.startswith("<") and filename.endswith(">")):
        ns["__file__"] = filename
        script_dir = os.path.dirname(filename)
        if script_dir:
            try:
                sys.path.remove(script_dir)
            except ValueError:
                pass
            sys.path.insert(0, script_dir)


async def _run_compiled_async(code: Any, ns: dict[str, Any], *, want_value: bool) -> Any:
    if code.co_flags & inspect.CO_COROUTINE:
        result = await eval(code, ns)
        return result if want_value else None
    if want_value:
        return eval(code, ns)
    exec(code, ns)
    return None


async def _exec_source_async(
    source: str,
    ns: dict[str, Any],
    *,
    filename: str = "<cell>",
    linecache_source: str | None = None,
) -> None:
    body_code, expr_code, has_expr = _compile_source(source, filename)
    _prepare_file_source(source, ns, filename, linecache_source)
    if body_code is None:
        return
    await _run_compiled_async(body_code, ns, want_value=False)
    if has_expr and expr_code is not None:
        value = await _run_compiled_async(expr_code, ns, want_value=True)
        if value is not None:
            _echo_value(value)


def _emit_error(rid: str, exc: BaseException, source_filename: str | None = None) -> None:
    if isinstance(exc, SyntaxError) and source_filename is not None and exc.filename == source_filename:
        tb_lines = traceback.format_exception_only(type(exc), exc)
    else:
        formatted = traceback.TracebackException.from_exception(exc)
        user_frames = [frame for frame in formatted.stack if frame.filename != __file__]
        if user_frames:
            formatted.stack = traceback.StackSummary.from_list(user_frames)
        tb_lines = list(formatted.format())
    if isinstance(exc, ModuleNotFoundError) and exc.name:
        tb_lines.append(
            f"Install the distribution that provides module {exc.name!r} with `%pip install <distribution-name>`."
        )
    _emit(
        {
            "type": "error",
            "id": rid,
            "ename": type(exc).__name__,
            "evalue": str(exc),
            "traceback": [line.rstrip("\n") for line in tb_lines],
        }
    )


def _install_idle_sigint() -> None:
    try:
        signal.signal(signal.SIGINT, signal.SIG_IGN)
    except (OSError, ValueError):
        pass


def _exec_sigint_handler(_signum: int, _frame: Any) -> None:
    loop = _STATE.loop
    active = _STATE.active_task
    if loop is None or active is None:
        return
    if asyncio.current_task(loop) is active:
        raise KeyboardInterrupt("Execution interrupted")
    if _STATE.active_rid is not None:
        _STATE.interrupted_rids.add(_STATE.active_rid)
    _abort_pending_tools("Execution interrupted")
    loop.call_soon_threadsafe(active.cancel)


def _install_exec_sigint() -> None:
    try:
        signal.signal(signal.SIGINT, _exec_sigint_handler)
    except (OSError, ValueError):
        pass


def _apply_request_runtime(req: dict[str, Any]) -> None:
    cwd = req.get("cwd")
    if isinstance(cwd, str) and cwd:
        os.chdir(cwd)
        try:
            sys.path.remove(cwd)
        except ValueError:
            pass
        sys.path.insert(0, cwd)


async def _handle_exec(req: dict[str, Any]) -> None:
    rid = str(req.get("id") or "")
    _STATE.execution_count += 1
    exec_count = _STATE.execution_count
    _STATE.active_rid = rid
    _STATE.interrupted_rids.discard(rid)
    rid_token = _CURRENT_RID.set(rid)
    fig_token = _SHOWN_FIGURES.set(set())
    _emit({"type": "started", "id": rid})

    status = "ok"
    cancelled = False
    filename = f"<cell-{exec_count}>"
    try:
        _apply_request_runtime(req)
        source = str(req.get("code") or "")
        req_filename = req.get("filename")
        if isinstance(req_filename, str) and req_filename:
            filename = req_filename
        transformed = _transform_cell(source)
        _install_exec_sigint()
        _STATE.active_task = asyncio.current_task()
        try:
            await _exec_source_async(
                transformed,
                _STATE.user_ns,
                filename=filename,
                linecache_source=source,
            )
        except KeyboardInterrupt:
            cancelled = True
            status = "error"
            _emit_error(rid, KeyboardInterrupt("Execution interrupted"))
        except asyncio.CancelledError as exc:
            status = "error"
            cancelled = rid in _STATE.interrupted_rids
            _emit_error(
                rid,
                KeyboardInterrupt("Execution interrupted") if cancelled else exc,
            )
        except SystemExit as exc:
            status = "error"
            _emit_error(rid, exc)
        except BaseException as exc:  # noqa: BLE001
            status = "error"
            _emit_error(rid, exc, source_filename=filename)
        finally:
            _STATE.active_task = None
            _install_idle_sigint()
            try:
                _flush_matplotlib_figures()
            except Exception:
                pass
    except SyntaxError as exc:
        status = "error"
        _emit_error(rid, exc, source_filename=filename)
    except BaseException as exc:  # noqa: BLE001
        status = "error"
        _emit_error(rid, exc)
    finally:
        _flush_stream_proxies(rid)
        _STATE.active_rid = None
        _STATE.interrupted_rids.discard(rid)
        _CURRENT_RID.reset(rid_token)
        _SHOWN_FIGURES.reset(fig_token)
        _emit(
            {
                "type": "done",
                "id": rid,
                "status": status,
                "executionCount": exec_count,
                "cancelled": cancelled,
            }
        )


def _control_reader(exec_queue: queue.Queue[dict[str, Any] | None], loop: asyncio.AbstractEventLoop) -> None:
    try:
        for raw in _CONTROL_IN:
            line = raw.strip()
            if not line:
                continue
            try:
                req = json.loads(line)
            except json.JSONDecodeError as exc:
                _emit(
                    {
                        "type": "error",
                        "id": "",
                        "ename": "ProtocolError",
                        "evalue": f"Invalid JSON request: {exc}",
                        "traceback": [],
                    }
                )
                continue
            msg_type = req.get("type")
            if msg_type == "exit":
                _STATE.shutdown = True
                _abort_pending_tools("kernel shutting down")
                exec_queue.put(None)
                return
            if msg_type == "tool_result":
                call_id = str(req.get("call_id") or "")
                with _PENDING_LOCK:
                    entry = _PENDING_TOOLS.get(call_id)
                if entry is not None:
                    event, holder = entry
                    if "error" in req and req["error"] is not None:
                        holder["error"] = req["error"]
                    else:
                        holder["value"] = req.get("value")
                    event.set()
                continue
            if msg_type == "interrupt":
                rid = str(req.get("id") or "")
                if rid:
                    _STATE.interrupted_rids.add(rid)
                _abort_pending_tools("Execution interrupted")
                task = _STATE.active_task
                if task is not None:
                    loop.call_soon_threadsafe(task.cancel)
                continue
            exec_queue.put(req)
    finally:
        _STATE.shutdown = True
        _abort_pending_tools("control channel closed")
        exec_queue.put(None)


async def _serve_async() -> None:
    loop = asyncio.get_running_loop()
    _STATE.loop = loop
    exec_queue: queue.Queue[dict[str, Any] | None] = queue.Queue()
    reader = threading.Thread(
        target=_control_reader,
        args=(exec_queue, loop),
        name="pp-eval-control",
        daemon=True,
    )
    reader.start()

    while True:
        req = await loop.run_in_executor(None, exec_queue.get)
        if req is None:
            break
        cell_task = asyncio.create_task(_handle_exec(req))
        try:
            await cell_task
        except asyncio.CancelledError:
            pass


def main() -> None:
    sys.stdout = _StreamProxy("stdout")  # type: ignore[assignment]
    sys.stderr = _StreamProxy("stderr")  # type: ignore[assignment]
    _install_idle_sigint()
    _start_parent_watchdog()
    _start_fd_drains()
    if _CONTROL_IN is None:
        return
    asyncio.run(_serve_async())


if __name__ == "__main__":
    main()
