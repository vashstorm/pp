"""受隔离测试控制的 stdio LSP；barrier 使用 FIFO，不以 sleep 推测时序。

默认行为保持最小、确定：navigation 由 CLI 修正后的同一 URI 位置回答，
rename/references/codeAction/executeCommand 仍是 method-not-found。显式传入
``--real-navigation`` 时才使用文档状态做真实跨文件语义；服务器主动
``workspace/applyEdit`` 只在 ``--apply-edit-on`` 指定方法时发送。
"""

import argparse
import json
import os
import re
import sys
import threading
from pathlib import Path

IDENT = re.compile(r"[A-Za-z0-9_$]+")
DECLARATION = re.compile(r"\b(?:fn|def|function|class|struct|enum|trait|let|const|var)\s+$")


class Server:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.documents: dict[str, dict] = {}
        self.output_lock = threading.Lock()
        self.log_lock = threading.Lock()
        self.callback_sent = False
        if self.args.scan_extension:
            self.scan_workspace()

    def scan_workspace(self) -> None:
        root = Path(self.args.workspace_dir or os.getcwd())
        suffix = self.args.scan_extension
        if not root.is_dir():
            return
        for path in sorted(root.rglob(f"*{suffix}")):
            if path.is_file():
                self.documents[path.resolve().as_uri()] = {
                    "uri": path.resolve().as_uri(),
                    "text": path.read_text(encoding="utf-8"),
                    "version": 0,
                }

    def send(self, message: dict) -> None:
        data = json.dumps({"jsonrpc": "2.0", **message}).encode()
        with self.output_lock:
            sys.stdout.buffer.write(f"Content-Length: {len(data)}\r\n\r\n".encode())
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()

    def record(self, message: dict) -> None:
        if self.args.log:
            with self.log_lock, Path(self.args.log).open("a") as stream:
                stream.write(json.dumps({"pid": os.getpid(), **message}) + "\n")

    def barrier(self, name: str) -> None:
        if self.args.barrier_dir:
            path = Path(self.args.barrier_dir) / name
            if path.exists():
                self.record({"barrier": name})
                with path.open("rb", buffering=0) as stream:
                    stream.read(1)

    # -- UTF-16 document helpers -------------------------------------------
    @staticmethod
    def lines(text: str) -> list[str]:
        return text.split("\n")

    @staticmethod
    def utf16_column(line: str, index: int) -> int:
        return sum(2 if ord(ch) > 0xFFFF else 1 for ch in line[:index])

    @staticmethod
    def py_index(line: str, character: int) -> int | None:
        units = 0
        for index, ch in enumerate(line):
            if units == character:
                return index
            units += 2 if ord(ch) > 0xFFFF else 1
            if units > character:
                return None
        return len(line) if units == character else None

    def document(self, uri: str) -> dict | None:
        return self.documents.get(uri)

    def symbol_at(self, uri: str, position: dict) -> str | None:
        document = self.document(uri)
        if document is None:
            return None
        lines = self.lines(document["text"])
        number = position.get("line", 0)
        if number >= len(lines):
            return None
        line = lines[number]
        index = self.py_index(line, position.get("character", 0))
        if index is None:
            return None
        if index >= len(line) or not IDENT.match(line, index):
            index -= 1
        if index < 0:
            return None
        start = index
        while start > 0 and IDENT.match(line, start - 1):
            start -= 1
        end = index
        while end < len(line) and IDENT.match(line, end):
            end += 1
        return line[start:end] or None

    def occurrences(self, symbol: str) -> list[tuple[str, int, int]]:
        found: list[tuple[str, int, int]] = []
        for uri, document in self.documents.items():
            for number, line in enumerate(self.lines(document["text"])):
                for match in IDENT.finditer(line):
                    if match.group() == symbol:
                        found.append((uri, number, match.start()))
        found.sort(key=lambda item: (item[0], item[1], item[2]))
        return found

    def location(self, uri: str, number: int, start: int) -> dict:
        line = self.lines(self.documents[uri]["text"])[number]
        match = IDENT.match(line, start)
        length = len(match.group()) if match else 0
        return {
            "uri": uri,
            "range": {
                "start": {"line": number, "character": self.utf16_column(line, start)},
                "end": {
                    "line": number,
                    "character": self.utf16_column(line, start + length),
                },
            },
        }

    def declaration(self, symbol: str) -> tuple[str, int, int] | None:
        for uri, number, start in self.occurrences(symbol):
            line = self.lines(self.documents[uri]["text"])[number]
            if DECLARATION.search(line[:start]):
                return uri, number, start
        return None

    def real_definition(self, uri: str, position: dict) -> list[dict] | None:
        symbol = self.symbol_at(uri, position)
        if symbol is None:
            return None
        declaration = self.declaration(symbol)
        if declaration is not None:
            return [self.location(*declaration)]
        occurrences = self.occurrences(symbol)
        if not occurrences:
            return None
        return [self.location(*occurrences[0])]

    def real_references(self, uri: str, position: dict) -> list[dict] | None:
        symbol = self.symbol_at(uri, position)
        if symbol is None:
            return None
        return [self.location(*occurrence) for occurrence in self.occurrences(symbol)]

    def real_rename(self, uri: str, position: dict, new_name: str) -> dict | None:
        symbol = self.symbol_at(uri, position)
        if symbol is None:
            return None
        changes: dict[str, list[dict]] = {}
        for target, number, start in self.occurrences(symbol):
            document = self.documents[target]
            line = self.lines(document["text"])[number]
            changes.setdefault(target, []).append(
                {
                    "range": {
                        "start": {
                            "line": number,
                            "character": self.utf16_column(line, start),
                        },
                        "end": {
                            "line": number,
                            "character": self.utf16_column(
                                line, start + len(symbol)
                            ),
                        },
                    },
                    "newText": new_name,
                }
            )
        if self.args.rename_version is not None:
            return {"documentChanges": [{"textDocument": {"uri": target, "version": self.args.rename_version}, "edits": edits} for target, edits in changes.items()]}
        return {"changes": changes} if changes else None

    # -- diagnostics/formatting --------------------------------------------
    def diagnostics(self, uri: str) -> list[dict]:
        text = self.documents.get(uri, {}).get("text", "")
        if "wrong" not in text:
            return []
        return [
            {
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 1},
                },
                "severity": 1,
                "message": "fixture: wrong value",
                "source": "fixture",
            }
        ]

    @staticmethod
    def full_range(text: str) -> dict:
        lines = text.split("\n")
        return {
            "start": {"line": 0, "character": 0},
            "end": {"line": len(lines) - 1, "character": len(lines[-1])},
        }

    @staticmethod
    def clean(text: str) -> str:
        """确定性的“格式化”结果：去行尾空白，保留单个结尾换行。"""
        lines = [line.rstrip() for line in text.split("\n")]
        return "\n".join(lines).rstrip("\n") + "\n"

    def formatting_result(self, params: dict) -> dict | None:
        uri = params["textDocument"]["uri"]
        text = self.documents.get(uri, {}).get("text", "")
        if self.args.format == "null":
            return None
        if self.args.format == "malformed":
            return [{"newText": "missing range"}]
        if self.args.format == "edits":
            return [{"range": self.full_range(text), "newText": self.clean(text)}]
        return []

    # -- server-initiated edit ---------------------------------------------
    def callback_edit(self) -> dict:
        uri = self.args.apply_edit_uri
        text = self.args.apply_edit_text
        document = self.documents.get(uri)
        if document is not None:
            source = document["text"]
        else:
            path = uri.removeprefix("file://")
            try:
                source = Path(path).read_text(encoding="utf-8")
            except OSError:
                source = ""
        return {
            "changes": {
                uri: [{"range": self.full_range(source), "newText": text}]
            }
        }

    def send_apply_edit(self) -> None:
        if self.callback_sent:
            return
        self.callback_sent = True
        self.send(
            {
                "id": "fixture-apply-edit",
                "method": "workspace/applyEdit",
                "params": {"label": "fixture callback", "edit": self.callback_edit()},
            }
        )
        # 必须等客户端真正处理完回调再回答触发请求；否则客户端可能先收到
        # 触发请求的响应并在结束 operation 时把回调统一拒绝。
        self.wait_for_reply("fixture-apply-edit")

    def read_message(self) -> dict | None:
        headers: dict[bytes, bytes] = {}
        while True:
            line = sys.stdin.buffer.readline()
            if not line:
                return None
            if line in (b"\r\n", b"\n"):
                break
            name, value = line.split(b":", 1)
            headers[name.lower()] = value.strip()
        body = sys.stdin.buffer.read(int(headers[b"content-length"]))
        return json.loads(body)

    def wait_for_reply(self, request_id: str) -> None:
        while True:
            message = self.read_message()
            if message is None:
                return
            self.record(message)
            if message.get("id") == request_id and message.get("method") is None:
                return

    def handle(self, message: dict) -> bool:
        self.record(message)
        method = message.get("method")
        params = message.get("params") or {}
        request_id = message.get("id")
        if method is None:
            # 服务器请求的响应；record() 已记录，无需回话。
            return True
        if (
            self.args.apply_edit_on
            and method == self.args.apply_edit_on
            and request_id is not None
        ):
            self.send_apply_edit()
        result = None
        if method == "initialize":
            self.barrier("initialize")
            capabilities = {
                "textDocumentSync": {"openClose": True, "change": 1},
                "hoverProvider": True,
                "definitionProvider": True,
                "referencesProvider": True,
                "renameProvider": True,
                "workspaceSymbolProvider": True,
                "executeCommandProvider": {"commands": ["fixture.reloadProjects"]},
                "diagnosticProvider": {
                    "interFileDependencies": False,
                    "workspaceDiagnostics": False,
                },
                "experimental": {"fixtureExtension": {"value": 42}},
                "workspace": {
                    "workspaceFolders": {
                        "supported": True,
                        "changeNotifications": True,
                    }
                },
            }
            if not self.args.no_format_capability:
                capabilities["documentFormattingProvider"] = True
            if self.args.real_navigation:
                capabilities["codeActionProvider"] = {"resolveProvider": True}
                capabilities["workspace"]["fileOperations"] = {"willRename": {"filters": [{"pattern": {"glob": "**/*"}}]}}
            result = {"capabilities": capabilities}
        elif method == "initialized":
            # 服务器在 initialized 之后立刻查询配置/工作区根：这两项必须在握手
            # 之前就已写入 transport，否则客户端会先回答 null/空列表。
            if self.args.probe_initialized:
                self.send(
                    {
                        "id": "probe-configuration",
                        "method": "workspace/configuration",
                        "params": {
                            "items": [
                                {"section": "fixture"},
                                {"section": "fixture.nested"},
                                {"section": "missing"},
                            ]
                        },
                    }
                )
                self.send(
                    {
                        "id": "probe-folders",
                        "method": "workspace/workspaceFolders",
                        "params": None,
                    }
                )
        elif method in ("textDocument/didOpen", "textDocument/didChange"):
            document = params["textDocument"]
            uri = document["uri"]
            if method.endswith("didOpen"):
                self.documents[uri] = dict(document, opened=True)
            else:
                existing = self.documents.setdefault(
                    uri, {"uri": uri, "text": "", "version": 0}
                )
                existing.update(
                    text=params["contentChanges"][-1]["text"],
                    version=document["version"],
                    opened=True,
                )
            self.send(
                {
                    "method": "textDocument/publishDiagnostics",
                    "params": {
                        "uri": uri,
                        "version": document["version"],
                        "diagnostics": self.diagnostics(uri),
                    },
                }
            )
        elif method == "textDocument/diagnostic":
            self.barrier("diagnostics")
            uri = params["textDocument"]["uri"]
            if self.args.pull == "malformed":
                result = {"kind": "full", "items": "invalid"}
            elif self.args.pull == "missing":
                result = {"kind": "full"}
            elif self.args.pull == "unchanged":
                result = {"kind": "unchanged", "resultId": "fixture-next"}
            else:
                result = {
                    "kind": "full",
                    "items": self.diagnostics(uri),
                    "resultId": "fixture",
                }
        elif method == "textDocument/definition":
            uri = params["textDocument"]["uri"]
            if self.args.real_navigation:
                result = self.real_definition(uri, params["position"])
            else:
                result = [
                    {
                        "uri": uri,
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 1},
                        },
                    }
                ]
        elif method == "textDocument/references":
            if not self.args.real_navigation:
                self.send(
                    {
                        "id": request_id,
                        "error": {"code": -32601, "message": "unsupported"},
                    }
                )
                return True
            result = self.real_references(
                params["textDocument"]["uri"], params["position"]
            )
        elif method == "textDocument/rename":
            if not self.args.real_navigation:
                self.send(
                    {
                        "id": request_id,
                        "error": {"code": -32601, "message": "unsupported"},
                    }
                )
                return True
            result = self.real_rename(
                params["textDocument"]["uri"],
                params["position"],
                params["newName"],
            )
        elif method == "textDocument/codeAction":
            if not self.args.real_navigation:
                self.send(
                    {
                        "id": request_id,
                        "error": {"code": -32601, "message": "unsupported"},
                    }
                )
                return True
            result = [
                {
                    "title": "fixture apply edit",
                    "kind": "quickfix",
                    "command": {
                        "title": "fixture apply edit",
                        "command": "fixture.applyEdit",
                        "arguments": [],
                    },
                }
            ]
            if self.args.code_action_mode != "command":
                result = [{"title": "fixture suggestion", "kind": "quickfix", "data": {"uri": params["textDocument"]["uri"]}}]
        elif method == "codeAction/resolve":
            uri = params["data"]["uri"]
            result = dict(params, edit={"changes": {uri: [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "new"}]}})
            if self.args.code_action_mode == "mixed":
                result["command"] = {"title": "finish", "command": "fixture.finish"}
        elif method == "workspace/willRenameFiles":
            uri = params["files"][0]["oldUri"]
            result = {"documentChanges": [{"textDocument": {"uri": uri, "version": self.args.rename_version}, "edits": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "new"}]}]}
        elif method == "workspace/executeCommand":
            if not self.args.real_navigation:
                self.send(
                    {
                        "id": request_id,
                        "error": {"code": -32601, "message": "unsupported"},
                    }
                )
                return True
            result = None
        elif method == "textDocument/hover":
            result = {"contents": {"kind": "markdown", "value": "fixture hover"}}
        elif method == "textDocument/formatting":
            if self.args.format == "error":
                if request_id is not None:
                    self.send(
                        {
                            "id": request_id,
                            "error": {
                                "code": -32603,
                                "message": "fixture format failure",
                            },
                        }
                    )
                return True
            result = self.formatting_result(params)
        elif method == "exit":
            return False
        elif method not in ("shutdown", "initialized") and request_id is not None:
            self.send(
                {"id": request_id, "error": {"code": -32601, "message": "unsupported"}}
            )
            return True
        if request_id is not None and method is not None:
            self.send({"id": request_id, "result": result})
        return True

    def run(self) -> None:
        self.record({"event": "started", "cwd": os.getcwd()})
        while True:
            message = self.read_message()
            if message is None:
                return
            if not self.handle(message):
                return


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rename-version", type=int)
    parser.add_argument("--code-action-mode", choices=["command", "resolve", "mixed"], default="command")
    parser.add_argument("--log")
    parser.add_argument("--barrier-dir")
    parser.add_argument("--pull", default="full")
    parser.add_argument(
        "--format",
        default="empty",
        choices=["empty", "edits", "null", "malformed", "error"],
    )
    parser.add_argument("--no-format-capability", action="store_true")
    parser.add_argument("--probe-initialized", action="store_true")
    parser.add_argument("--real-navigation", action="store_true")
    parser.add_argument("--scan-extension")
    parser.add_argument("--workspace-dir")
    parser.add_argument("--apply-edit-on")
    parser.add_argument("--apply-edit-uri")
    parser.add_argument("--apply-edit-text", default="callback edit\n")
    Server(parser.parse_args()).run()


if __name__ == "__main__":
    main()
