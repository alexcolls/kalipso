#!/usr/bin/env python3
"""A fake Claude API for tests/e2e.sh. Writes its port to the file given as argv[1]."""
import json
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODELS = {"data": [
    {"id": "claude-opus-5-5", "capabilities": {
        "thinking": {"types": {"adaptive": {"supported": True}}}, "effort": {"supported": True}}},
    {"id": "claude-sonnet-mock", "capabilities": {
        "thinking": {"types": {"adaptive": {"supported": False}}}, "effort": {"supported": False}}},
]}


def sse(events):
    return "".join(f"data: {json.dumps(event)}\n\n" for event in events)


def turn(blocks, stop):
    events = [{"type": "message_start", "message": {"usage": {"input_tokens": 123, "output_tokens": 1}}}]
    for index, block in enumerate(blocks):
        if block["type"] == "text":
            events.append({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}})
            events.append({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": block["text"]}})
        elif block["type"] == "thinking":
            events.append({"type": "content_block_start", "index": index, "content_block": {"type": "thinking", "thinking": "", "signature": ""}})
            events.append({"type": "content_block_delta", "index": index, "delta": {"type": "thinking_delta", "thinking": block["thinking"]}})
            events.append({"type": "content_block_delta", "index": index, "delta": {"type": "signature_delta", "signature": "sig"}})
        else:
            start = {"type": "tool_use", "id": block["id"], "name": block["name"], "input": {}}
            events.append({"type": "content_block_start", "index": index, "content_block": start})
            events.append({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": json.dumps(block["input"])}})
        events.append({"type": "content_block_stop", "index": index})
    events.append({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 45}})
    events.append({"type": "message_stop"})
    return sse(events)


def reply(body):
    """The script: ask to write a file, then finish once a tool result comes back."""
    if "tools" not in body:
        return turn([{"type": "text", "text": "Summary of the session."}], "end_turn")
    last = body["messages"][-1]
    text = json.dumps(last["content"])
    if "what is open" in text:
        found = re.search(r"Editor state, for context only: (\S+) is open", text)
        return turn([{"type": "text", "text": f"You have {found.group(1) if found else 'nothing'} open."}], "end_turn")
    if "say something new" in text:
        return turn([{"type": "text", "text": "A new reply after the summary."}], "end_turn")
    if isinstance(last["content"], list) and "tool_result" in text:
        return turn([{"type": "text", "text": "All done. Here is the code:\n```rust\nfn main() {}\n```"}], "end_turn")
    if "make a file" in text:
        return turn([
            {"type": "thinking", "thinking": "I should write the file."},
            {"type": "text", "text": "Writing it now."},
            {"type": "tool_use", "id": "t1", "name": "write_file",
             "input": {"path": "agent.txt", "contents": "made by agent\n"}},
        ], "tool_use")
    if "run a command" in text:
        return turn([{"type": "tool_use", "id": "t2", "name": "run_command",
                      "input": {"command": "echo key=[$ANTHROPIC_API_KEY] > command.txt"}}], "tool_use")
    return turn([{"type": "text", "text": "Hello from the mock."}], "end_turn")


class Handler(BaseHTTPRequestHandler):
    def send_text(self, text, kind):
        data = text.encode()
        self.send_response(200)
        self.send_header("content-type", kind)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self.send_text(json.dumps(MODELS), "application/json")

    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        body = json.loads(self.rfile.read(length))
        self.send_text(reply(body), "text/event-stream")

    def log_message(self, *args):
        pass


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(sys.argv[1], "w") as out:
    out.write(str(server.server_port))
server.serve_forever()
