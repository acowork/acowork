"""Minimal OpenAI-compatible mock endpoint for the attachment E2E.

Purpose: be the LLM the Chat Router calls, so the *request body* it sends —
which contains Block C verbatim — can be captured and inspected. Nothing here
pretends to be a model: every call gets the same short reply, and the router
decides `reply` from it.

Run:  python dev/e2e/mock_openai.py <port> <capture-file>

The capture file defaults into the OS temp dir, not the CWD: this script is
run from the repo root, and a stray `mock-capture.jsonl` there is noise in
`git status` that looks like work in progress.
"""
import json
import os
import sys
import tempfile
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 29999
CAPTURE = (
    sys.argv[2]
    if len(sys.argv) > 2
    else os.path.join(tempfile.gettempdir(), "acowork-mock-capture.jsonl")
)


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):  # noqa: N802 — http.server API
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        try:
            body = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            body = {"_unparsed": raw.decode("utf-8", "replace")}

        # One line per call, so a turn's prompt can be diffed against the next.
        with open(CAPTURE, "a", encoding="utf-8") as fh:
            fh.write(json.dumps({"ts": time.time(), "path": self.path, "body": body},
                                ensure_ascii=False) + "\n")

        reply = {
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "created": int(time.time()),
            "model": body.get("model", "mock-model"),
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "收到，看一看这个附件。"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        }
        payload = json.dumps(reply).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):  # noqa: N802
        # Some clients probe /v1/models before the first call.
        payload = json.dumps({"object": "list", "data": [
            {"id": "mock-model", "object": "model", "owned_by": "mock"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *_args):
        pass


if __name__ == "__main__":
    print(f"mock openai on 127.0.0.1:{PORT}, capturing to {CAPTURE}", flush=True)
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()