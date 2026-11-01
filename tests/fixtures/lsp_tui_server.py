"""TUI 专用 stdio LSP 夹具：可控的迟到诊断与格式化。

诊断 pull 在 ``tui-late`` FIFO 上阻塞，测试释放后才返回，从而真实经过
Agent 的 500ms 内联等待与后台迟到投递。没有该 FIFO 时立即返回。
"""

import argparse
import json
import os
import sys
from pathlib import Path


class Server:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.documents: dict[str, dict] = {}

    def send(self, message: dict) -> None:
        data = json.dumps({"jsonrpc": "2.0", **message}).encode()
        sys.stdout.buffer.write(f"Content-Length: {len(data)}\r\n\r\n".encode())
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()

    def record(self, message: dict) -> None:
        if self.args.log:
            with Path(self.args.log).open("a") as stream:
                stream.write(json.dumps({"pid": os.getpid(), **message}) + "\n")

    def barrier(self, name: str) -> None:
        if not self.args.barrier_dir:
            return
        path = Path(self.args.barrier_dir) / name
        if path.exists():
            self.record({"barrier": name})
            with path.open("rb", buffering=0) as stream:
                stream.read(1)

    def diagnostics(self, uri: str) -> list[dict]:
        text = self.documents.get(uri, {}).get("text", "")
        items = []
        for index, line in enumerate(text.split("\n")):
            if "wrong" not in line:
                continue
            items.append(
                {
                    "range": {
                        "start": {"line": index, "character": 0},
                        "end": {"line": index, "character": 1},
                    },
                    "severity": self.args.severity,
                    "message": f"fixture wrong line {index + 1}",
                    "source": "fixture",
                }
            )
        return items

    @staticmethod
    def full_range(text: str) -> dict:
        lines = text.split("\n")
        return {
            "start": {"line": 0, "character": 0},
            "end": {"line": len(lines) - 1, "character": len(lines[-1])},
        }

    @staticmethod
    def clean(text: str) -> str:
        lines = [line.rstrip() for line in text.split("\n")]
        return "\n".join(lines).rstrip("\n") + "\n"

    def handle(self, message: dict) -> bool:
        self.record(message)
        method = message.get("method")
        params = message.get("params") or {}
        request_id = message.get("id")
        if method is None:
            return True
        result = None
        if method == "initialize":
            result = {
                "capabilities": {
                    "textDocumentSync": {"openClose": True, "change": 1},
                    "documentFormattingProvider": True,
                    "diagnosticProvider": {
                        "interFileDependencies": False,
                        "workspaceDiagnostics": False,
                    },
                }
            }
        elif method == "initialized":
            return True
        elif method in ("textDocument/didOpen", "textDocument/didChange"):
            document = params["textDocument"]
            uri = document["uri"]
            if method.endswith("didOpen"):
                self.documents[uri] = document
            else:
                self.documents[uri].update(
                    text=params["contentChanges"][-1]["text"],
                    version=document["version"],
                )
            return True
        elif method == "textDocument/diagnostic":
            self.barrier("tui-late")
            uri = params["textDocument"]["uri"]
            if self.args.pull == "unchanged":
                result = {"kind": "unchanged", "resultId": "fixture-next"}
            else:
                result = {
                    "kind": "full",
                    "items": self.diagnostics(uri),
                    "resultId": "fixture",
                }
        elif method == "textDocument/formatting":
            uri = params["textDocument"]["uri"]
            text = self.documents.get(uri, {}).get("text", "")
            result = [{"range": self.full_range(text), "newText": self.clean(text)}]
        elif method == "shutdown":
            result = None
        elif method == "exit":
            return False
        elif request_id is not None:
            self.send(
                {"id": request_id, "error": {"code": -32601, "message": "unsupported"}}
            )
            return True
        if request_id is not None:
            self.send({"id": request_id, "result": result})
        return True

    def run(self) -> None:
        self.record({"event": "started"})
        while True:
            headers: dict[bytes, bytes] = {}
            while True:
                line = sys.stdin.buffer.readline()
                if not line:
                    return
                if line in (b"\r\n", b"\n"):
                    break
                name, value = line.split(b":", 1)
                headers[name.lower()] = value.strip()
            body = sys.stdin.buffer.read(int(headers[b"content-length"]))
            if not self.handle(json.loads(body)):
                return


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--log")
    parser.add_argument("--barrier-dir")
    parser.add_argument("--pull", default="full", choices=["full", "unchanged"])
    parser.add_argument("--severity", type=int, default=1)
    Server(parser.parse_args()).run()


if __name__ == "__main__":
    main()
