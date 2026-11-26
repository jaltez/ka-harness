#!/usr/bin/env python3
"""Deterministic OpenAI-compatible SSE mock for the session and overhead benches.

Endpoints:
  POST /v1/chat/completions  OpenAI Chat Completions SSE (used by session_bench.py)
  POST /v1/responses         OpenAI Responses API SSE (used by compare_overhead.py,
                             supports both Ka and Codex CLI tool schemas)

Scenarios are routed by a tag in the last user message:
  BENCH:TOOLS4    round 1 -> 4 read/inspect tool calls; later rounds -> text
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


def sse(wfile, obj, event=None):
    if event:
        wfile.write(f"event: {event}\n".encode())
    wfile.write(b"data: " + json.dumps(obj).encode() + b"\n\n")
    wfile.flush()


def _extract_text(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for item in content:
            if isinstance(item, dict):
                t = item.get("text")
                if isinstance(t, str):
                    parts.append(t)
        return "\n".join(parts)
    return ""


def _parse_responses_input(body):
    """Extract (last_user_text, tool_rounds) from a Responses API request body."""
    inp = body.get("input", [])
    if isinstance(inp, str):
        return inp, 0
    last_user = ""
    tool_rounds = 0
    if isinstance(inp, list):
        for item in inp:
            if not isinstance(item, dict):
                continue
            itype = item.get("type", "")
            if itype in ("function_call_output", "custom_tool_call_output"):
                tool_rounds += 1
            elif item.get("role") == "user":
                text = _extract_text(item.get("content"))
                if text:
                    last_user = text
    return last_user, tool_rounds


def _tool_map(body):
    """Return dict of tool_name -> tool_schema from request `tools` array."""
    tools = {}
    for t in body.get("tools") or []:
        if not isinstance(t, dict):
            continue
        name = t.get("name")
        if not name and isinstance(t.get("function"), dict):
            name = t["function"].get("name")
        if name:
            tools[name] = t
    return tools


def _build_tools4_calls(tools):
    """Choose 4 read/inspect tool calls matching the client's registered tools."""
    if "read" in tools:
        return [("read", json.dumps({"path": f"f{i + 1}.txt"})) for i in range(4)]
    if "exec_command" in tools:
        return [
            ("exec_command", json.dumps({"cmd": f"cat f{i + 1}.txt"}))
            for i in range(4)
        ]
    if "shell_command" in tools:
        return [
            ("shell_command", json.dumps({"command": f"cat f{i + 1}.txt"}))
            for i in range(4)
        ]
    if "shell" in tools:
        return [
            ("shell", json.dumps({"command": ["cat", f"f{i + 1}.txt"]}))
            for i in range(4)
        ]
    if "bash" in tools:
        return [
            ("bash", json.dumps({"command": f"cat f{i + 1}.txt"}))
            for i in range(4)
        ]
    if tools:
        # Generic fallback: pick the first function tool and populate its first parameter.
        name, schema = next(iter(tools.items()))
        props = (schema.get("parameters") or {}).get("properties") or {}
        key = next(iter(props), "path")
        return [(name, json.dumps({key: f"f{i + 1}.txt"})) for i in range(4)]
    return [("read", json.dumps({"path": f"f{i + 1}.txt"})) for i in range(4)]


def _build_write200k_calls(tools):
    """Choose a 200 KB write tool call matching the client's registered tools."""
    if "write" in tools:
        return [
            ("write", json.dumps({"path": "out200k.txt", "content": BIG_200K}))
        ]
    if "apply_patch" in tools:
        lines = ["*** Begin Patch", "*** Add File: out200k.txt"]
        for i in range(0, len(BIG_200K), 410):
            lines.append("+" + BIG_200K[i:i + 410])
        lines.append("*** End Patch")
        patch_text = "\n".join(lines) + "\n"
        props = (tools["apply_patch"].get("parameters") or {}).get("properties") or {}
        key = "input" if "input" in props else next(iter(props), "input")
        return [("apply_patch", json.dumps({key: patch_text}))]
    if "exec_command" in tools:
        cmd = (
            "python3 -c \"open('out200k.txt','w').write("
            "('abcdefghij'*40+'0123456789')*512)\""
        )
        return [("exec_command", json.dumps({"cmd": cmd}))]
    if "shell_command" in tools:
        cmd = (
            "python3 -c \"open('out200k.txt','w').write("
            "('abcdefghij'*40+'0123456789')*512)\""
        )
        return [("shell_command", json.dumps({"command": cmd}))]
    if "bash" in tools:
        cmd = (
            "python3 -c \"open('out200k.txt','w').write("
            "('abcdefghij'*40+'0123456789')*512)\""
        )
        return [("bash", json.dumps({"command": cmd}))]
    return [("write", json.dumps({"path": "out200k.txt", "content": BIG_200K}))]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def log_message(self, *a):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")

        path = self.path.split("?", 1)[0].rstrip("/")
        if path.endswith("/responses"):
            self._handle_responses(body)
        else:
            self._handle_chat_completions(body)

    def _handle_chat_completions(self, body):
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

    def _handle_responses(self, body):
        last_user, tool_rounds = _parse_responses_input(body)
        n = re.search(r"n=(\d+)", last_user)
        n = n.group(1) if n else "0"
        marker = f"KQ7ZT{n}X"
        model = body.get("model", "bench-mock")
        tools = _tool_map(body)
        resp_id = f"resp_bench_{n}_{tool_rounds}"

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()

        sse(
            self.wfile,
            {
                "type": "response.created",
                "response": {
                    "id": resp_id,
                    "object": "response",
                    "status": "in_progress",
                    "model": model,
                    "output": [],
                },
            },
            event="response.created",
        )

        output_items = []
        if "BENCH:TOOLS4" in last_user and tool_rounds == 0:
            calls = _build_tools4_calls(tools)
            for idx, (name, args_str) in enumerate(calls):
                item_id = f"fc_{idx}"
                call_id = f"call_{idx}"
                sse(
                    self.wfile,
                    {
                        "type": "response.output_item.added",
                        "output_index": idx,
                        "item": {
                            "id": item_id,
                            "type": "function_call",
                            "status": "in_progress",
                            "call_id": call_id,
                            "name": name,
                            "arguments": "",
                        },
                    },
                    event="response.output_item.added",
                )
                sse(
                    self.wfile,
                    {
                        "type": "response.function_call_arguments.delta",
                        "item_id": item_id,
                        "output_index": idx,
                        "delta": args_str,
                    },
                    event="response.function_call_arguments.delta",
                )
                done_item = {
                    "id": item_id,
                    "type": "function_call",
                    "status": "completed",
                    "call_id": call_id,
                    "name": name,
                    "arguments": args_str,
                }
                output_items.append(done_item)
                sse(
                    self.wfile,
                    {
                        "type": "response.output_item.done",
                        "output_index": idx,
                        "item": done_item,
                    },
                    event="response.output_item.done",
                )
        elif "BENCH:WRITE200K" in last_user and tool_rounds == 0:
            calls = _build_write200k_calls(tools)
            for idx, (name, args_str) in enumerate(calls):
                item_id = f"fc_{idx}"
                call_id = f"call_{idx}"
                sse(
                    self.wfile,
                    {
                        "type": "response.output_item.added",
                        "output_index": idx,
                        "item": {
                            "id": item_id,
                            "type": "function_call",
                            "status": "in_progress",
                            "call_id": call_id,
                            "name": name,
                            "arguments": "",
                        },
                    },
                    event="response.output_item.added",
                )
                for off in range(0, len(args_str), 8192):
                    sse(
                        self.wfile,
                        {
                            "type": "response.function_call_arguments.delta",
                            "item_id": item_id,
                            "output_index": idx,
                            "delta": args_str[off:off + 8192],
                        },
                        event="response.function_call_arguments.delta",
                    )
                done_item = {
                    "id": item_id,
                    "type": "function_call",
                    "status": "completed",
                    "call_id": call_id,
                    "name": name,
                    "arguments": args_str,
                }
                output_items.append(done_item)
                sse(
                    self.wfile,
                    {
                        "type": "response.output_item.done",
                        "output_index": idx,
                        "item": done_item,
                    },
                    event="response.output_item.done",
                )
        else:
            if "BENCH:STREAM60K" in last_user:
                text = BIG_60K_CHARS + " " + marker
                step = 100
            else:
                text = ANSWER_1200 + " " + marker
                step = 200

            item_id = "msg_0"
            sse(
                self.wfile,
                {
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {
                        "id": item_id,
                        "type": "message",
                        "status": "in_progress",
                        "role": "assistant",
                        "content": [],
                    },
                },
                event="response.output_item.added",
            )
            for off in range(0, len(text), step):
                sse(
                    self.wfile,
                    {
                        "type": "response.output_text.delta",
                        "item_id": item_id,
                        "output_index": 0,
                        "content_index": 0,
                        "delta": text[off:off + step],
                    },
                    event="response.output_text.delta",
                )
            done_item = {
                "id": item_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": text, "annotations": []}
                ],
            }
            output_items.append(done_item)
            sse(
                self.wfile,
                {
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": done_item,
                },
                event="response.output_item.done",
            )

        sse(
            self.wfile,
            {
                "type": "response.completed",
                "response": {
                    "id": resp_id,
                    "object": "response",
                    "status": "completed",
                    "model": model,
                    "output": output_items,
                    "usage": {
                        "input_tokens": 1000,
                        "input_tokens_details": {"cached_tokens": 900},
                        "output_tokens": 500,
                        "output_tokens_details": {"reasoning_tokens": 0},
                        "total_tokens": 1500,
                    },
                },
            },
            event="response.completed",
        )
        self.close_connection = True


class Server(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True


if __name__ == "__main__":
    Server(("127.0.0.1", PORT), Handler).serve_forever()
