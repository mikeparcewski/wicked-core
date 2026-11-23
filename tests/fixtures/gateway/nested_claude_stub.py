"""A scripted Anthropic Messages API on loopback, in the seat's place of the gateway
(wicked-garden#1365, `container_floor` live test). No credential is real and nothing leaves the
container (`--network none`).

The outer turn gets one Bash tool call that starts a nested `claude -p`, the way a harness's rule
model runs inside a seat. The nested turn gets the text INNER-OK. Every request's auth-bearing
headers are logged as one JSON line to the log file named on the command line.
"""
import json
import sys
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT, LOG_PATH = int(sys.argv[1]), sys.argv[2]
NESTED = "echo PROBE-INNER | claude -p 2>&1 | head -3"


def message(content, stop):
    base = {"id": "msg_" + uuid.uuid4().hex[:12], "type": "message", "role": "assistant",
            "model": "claude-stub", "content": [], "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 1, "output_tokens": 1}}
    events = [("message_start", {"type": "message_start", "message": base})]
    for i, block in enumerate(content):
        if block["type"] == "text":
            start = {"type": "text", "text": ""}
            delta = {"type": "text_delta", "text": block["text"]}
        else:
            start = {**block, "input": {}}
            delta = {"type": "input_json_delta", "partial_json": json.dumps(block["input"])}
        events += [
            ("content_block_start", {"type": "content_block_start", "index": i,
                                     "content_block": start}),
            ("content_block_delta", {"type": "content_block_delta", "index": i, "delta": delta}),
            ("content_block_stop", {"type": "content_block_stop", "index": i}),
        ]
    events += [
        ("message_delta", {"type": "message_delta",
                           "delta": {"stop_reason": stop, "stop_sequence": None},
                           "usage": {"output_tokens": 1}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return events, {**base, "content": content, "stop_reason": stop}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        self.send_response(404)
        self.end_headers()

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length") or 0))
        headers = {k.lower(): v for k, v in self.headers.items()}
        with open(LOG_PATH, "a") as log:
            log.write(json.dumps({
                "path": self.path,
                "authorization": headers.get("authorization", ""),
                "x-api-key": headers.get("x-api-key", ""),
                "unit": headers.get("x-wicked-unit", ""),
                "beta": headers.get("anthropic-beta", ""),
            }) + "\n")
        if not self.path.startswith("/v1/messages") or "count_tokens" in self.path:
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"input_tokens": 1}')
            return
        request = json.loads(body or b"{}")
        text = json.dumps(request.get("messages", []))
        # The outer continuation first: it carries the Bash command, which names PROBE-INNER.
        if "tool_result" in text:
            content, stop = [{"type": "text", "text": "OUTER-DONE"}], "end_turn"
        elif "PROBE-INNER" in text:
            content, stop = [{"type": "text", "text": "INNER-OK"}], "end_turn"
        elif "PROBE-OUTER" in text:
            content = [{"type": "tool_use", "id": "toolu_" + uuid.uuid4().hex[:12],
                        "name": "Bash", "input": {"command": NESTED, "description": "nested"}}]
            stop = "tool_use"
        else:
            content, stop = [{"type": "text", "text": "OTHER"}], "end_turn"
        events, full = message(content, stop)
        self.send_response(200)
        if request.get("stream"):
            self.send_header("content-type", "text/event-stream")
            self.end_headers()
            for name, data in events:
                self.wfile.write(f"event: {name}\ndata: {json.dumps(data)}\n\n".encode())
                self.wfile.flush()
        else:
            self.send_header("content-type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps(full).encode())


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
