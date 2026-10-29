#!/usr/bin/env python
"""Fake OpenAI-Responses endpoint for the ADR-103 S5 smoke test.

Serves the three shapes the probe distinguishes:

  POST /v1/responses
    - body.input == "ping"                -> baseline (L1/L2 pass)
    - body.tools (tool_choice == "auto")  -> function_call item (L3 pass)
    - body contains function_call_output  -> replay accepted

`FAKE_MODE` env var:
  healthy           (default) every tier passes
  no_tool           baseline passes, L3 returns no function_call
  missing           /v1/responses is 404 "unknown endpoint" (Chat-Completions host)
  reject_reasoning  any body with a `reasoning` block is 400 (an endpoint that
                    does not implement the field at all); the trimmed retry
                    passes — ADR-103 D8/S7
"""

import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODE = os.environ.get("FAKE_MODE", "healthy")
HITS = {"baseline": 0, "round_trip": 0, "replay": 0, "rejected": 0}


def rejects_reasoning(body):
    """Does this body carry the one field D8 is allowed to trim?"""
    reasoning = body.get("reasoning")
    return isinstance(reasoning, dict) and "effort" in reasoning


def reasoning_400():
    return {
        "error": {
            "code": "invalid_parameter",
            "message": "invalid parameter: `reasoning`",
            "param": "reasoning",
            "type": "invalid_request_error",
        }
    }


def baseline():
    return {
        "id": "resp_fake_1",
        "object": "response",
        "status": "completed",
        "output": [
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "pong"}],
            }
        ],
        "usage": {"input_tokens": 7, "output_tokens": 1},
    }


def text_only():
    return {
        "id": "resp_fake_2",
        "object": "response",
        "status": "completed",
        "output": [{"type": "message", "role": "assistant", "content": []}],
        "usage": {"input_tokens": 30, "output_tokens": 4},
    }


def function_call():
    return {
        "id": "resp_fake_3",
        "object": "response",
        "status": "completed",
        "output": [
            {
                "type": "function_call",
                "call_id": "call_probe_fake",
                "name": "acowork_probe_echo",
                "arguments": '{"value": 1}',
            }
        ],
        "usage": {"input_tokens": 30, "output_tokens": 12},
    }


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):  # keep the smoke output readable
        sys.stderr.write("[fake-provider] " + (fmt % args) + "\n")

    def _json(self, status, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        if not self.path.endswith("/responses"):
            self._json(404, {"error": {"message": f"unknown endpoint {self.path}"}})
            return
        length = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(length).decode()
        try:
            body = json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            self._json(400, {"error": {"message": "invalid json"}})
            return

        if MODE == "missing":
            self._json(
                404,
                {"error": {"message": f"Unknown endpoint: {self.path}", "type": "invalid_request_error"}},
            )
            return

        if MODE == "reject_reasoning" and rejects_reasoning(body):
            HITS["rejected"] += 1
            self._json(400, reasoning_400())
            return

        if "function_call_output" in raw:
            HITS["replay"] += 1
            self._json(200, baseline())
        elif body.get("tool_choice"):
            HITS["round_trip"] += 1
            self._json(200, text_only() if MODE == "no_tool" else function_call())
        else:
            HITS["baseline"] += 1
            self._json(200, baseline())

    def do_GET(self):
        if self.path == "/hits":
            self._json(200, HITS)
        else:
            self._json(404, {"error": {"message": "not found"}})


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 28899
    print(f"fake Responses provider on 127.0.0.1:{port} (mode={MODE})", flush=True)
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()