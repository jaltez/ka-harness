#!/usr/bin/env python3
"""Deterministic OpenAI-chat-compatible SSE mock for the session bench.

Scenarios are routed by a tag in the last user message:
  BENCH:TOOLS4    round 1 -> 4 read tool calls; later rounds -> text
  BENCH:WRITE200K round 1 -> write tool call with a 204_800-byte payload
  BENCH:STREAM60K one 60_000-char answer streamed in 100-char chunks
  anything else   ~1_200-char text answer

Final text answers end with the marker KQ7ZT<n>X where n comes from
"n=<k>" in the prompt, so the driver can detect turn completion.

Usage: mock_server.py <port>
"""
import json
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8123

FILLER = (
    "The build error comes from a mismatch between the declared feature set "
    "and the crate graph. Check the workspace manifest first, then the "
    "per-crate feature unions. Most of the time the fix is to move the "
    "feature to the crate that actually owns the code. "
)
ANSWER_1200 = (FILLER * 12)[:1200]
BIG_200K = ("abcdefghij" * 40 + "0123456789") * 512  # 204_800 bytes
BIG_60K_CHARS = (FILLER * 800)[:60_000]


def sse(wfile, obj):
    wfile.write(b"data: " + json.dumps(obj).encode() + b"\n\n")
    wfile.flush()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def log_message(self, *a):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        messages = body.get("messages", [])
        last_user = ""
        for m in messages:
            if m.get("role") == "user" and isinstance(m.get("content"), str):
                last_user = m["content"]
        n = re.search(r"n=(\d+)", last_user)
        n = n.group(1) if n else "0"
        marker = f"KQ7ZT{n}X"
        tool_rounds = sum(1 for m in messages if m.get("role") == "tool")

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()

        if "BENCH:TOOLS4" in last_user and tool_rounds == 0:
            calls = [
                {"index": i, "id": f"call-{i}", "type": "function",
                 "function": {"name": "read",
                              "arguments": json.dumps({"path": f"f{i + 1}.txt"})}}
                for i in range(4)
            ]
            sse(self.wfile, {"choices": [{"index": 0, "delta": {
                "role": "assistant", "tool_calls": calls}, "finish_reason": None}]})
            sse(self.wfile, {"choices": [{"index": 0, "delta": {},
                                          "finish_reason": "tool_calls"}]})
        elif "BENCH:WRITE200K" in last_user and tool_rounds == 0:
            sse(self.wfile, {"choices": [{"index": 0, "delta": {
                "role": "assistant",
                "tool_calls": [{"index": 0, "id": "call-0", "type": "function",
                                "function": {"name": "write", "arguments": json.dumps(
                                    {"path": "out200k.txt", "content": BIG_200K})}}]},
                "finish_reason": None}]})
            sse(self.wfile, {"choices": [{"index": 0, "delta": {},
                                          "finish_reason": "tool_calls"}]})
        elif "BENCH:STREAM60K" in last_user:
            text = BIG_60K_CHARS + " " + marker
            for i in range(0, len(text), 100):
                sse(self.wfile, {"choices": [{"index": 0, "delta": {
                    "content": text[i:i + 100]}, "finish_reason": None}]})
            sse(self.wfile, {"choices": [{"index": 0, "delta": {},
                                          "finish_reason": "stop"}]})
        else:
            text = ANSWER_1200 + " " + marker
            for i in range(0, len(text), 200):
                sse(self.wfile, {"choices": [{"index": 0, "delta": {
                    "content": text[i:i + 200]}, "finish_reason": None}]})
            sse(self.wfile, {"choices": [{"index": 0, "delta": {},
                                          "finish_reason": "stop"}]})

        sse(self.wfile, {"choices": [], "usage": {
            "prompt_tokens": 1000, "completion_tokens": 500,
            "prompt_tokens_details": {"cached_tokens": 900}}})
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()
        self.close_connection = True


class Server(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True


if __name__ == "__main__":
    Server(("127.0.0.1", PORT), Handler).serve_forever()
