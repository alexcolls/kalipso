#!/usr/bin/env python3
"""A fake language server for tests. Speaks real LSP framing on stdin and stdout.

It reports BAD and WARN lines as problems, and answers go-to-definition by finding
`fn <word>` in the open files. Positions are UTF-16, the protocol's default, so a client
that gets its column conversion wrong finds the wrong word. If MOCK_LSP_LOG is set, every
message from the client is appended there as a line of JSON.
"""
import json
import os
import re
import sys

log_path = os.environ.get("MOCK_LSP_LOG")
docs = {}
saved = set()


def log(entry):
    if log_path:
        with open(log_path, "a") as out:
            out.write(json.dumps(entry) + "\n")


def send(message):
    body = json.dumps(message).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n%s" % (len(body), body))
    sys.stdout.buffer.flush()


def receive():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            break
        name, _, value = line.partition(b":")
        if name.lower() == b"content-length":
            length = int(value)
    return json.loads(sys.stdin.buffer.read(length))


def publish(uri):
    problems = []
    for number, line in enumerate(docs[uri].split("\n")):
        for word, severity in (("BAD", 1), ("WARN", 2)):
            if word in line:
                problems.append({
                    "range": {"start": {"line": number, "character": 0}, "end": {"line": number, "character": 1}},
                    "severity": severity,
                    "message": f"mock problem: {word}\nsecond line",
                })
    if uri in saved:
        problems.append({
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "severity": 3,
            "message": "mock saw the save",
        })
    send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": uri, "diagnostics": problems}})


def word_at(uri, position):
    line = docs[uri].split("\n")[position["line"]]
    units = line.encode("utf-16-le")
    rest = units[position["character"] * 2:].decode("utf-16-le")
    found = re.match(r"\w+", rest)
    return found.group(0) if found else None


def definition(params):
    word = word_at(params["textDocument"]["uri"], params["position"])
    if not word:
        return None
    for uri, text in docs.items():
        for number, line in enumerate(text.split("\n")):
            found = re.search(r"fn (%s)\b" % re.escape(word), line)
            if found:
                column = len(line[: found.start(1)].encode("utf-16-le")) // 2
                start = {"line": number, "character": column}
                return [{"uri": uri, "range": {"start": start, "end": start}}]
    return None


log({"event": "started", "pid": os.getpid(), "secret": os.environ.get("OPENROUTER_API_KEY")})
while True:
    message = receive()
    if message is None:
        break
    log(message)
    method = message.get("method")
    params = message.get("params") or {}
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": {
            "textDocumentSync": 1, "definitionProvider": True, "positionEncoding": "utf-16"}}})
    elif method == "initialized":
        # Some real servers won't continue until requests like this one are answered.
        send({"jsonrpc": "2.0", "id": 900, "method": "workspace/configuration",
              "params": {"items": [{"section": "a"}, {"section": "b"}]}})
    elif method == "textDocument/didOpen":
        document = params["textDocument"]
        docs[document["uri"]] = document["text"]
        publish(document["uri"])
    elif method == "textDocument/didChange":
        uri = params["textDocument"]["uri"]
        docs[uri] = params["contentChanges"][-1]["text"]
        publish(uri)
    elif method == "textDocument/didSave":
        uri = params["textDocument"]["uri"]
        saved.add(uri)
        publish(uri)
    elif method == "textDocument/didClose":
        docs.pop(params["textDocument"]["uri"], None)
    elif method == "textDocument/definition":
        send({"jsonrpc": "2.0", "id": message["id"], "result": definition(params)})
    elif method == "kalipso/crash":
        sys.exit(1)
