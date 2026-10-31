#!/usr/bin/env python3
"""A tiny MCP server over stdio for moco's tests. Behavior is switched with environment variables:
FAKE_MCP_VERSION (protocol revision to answer with), FAKE_MCP_NO_INIT=1 (never answer initialize)."""
import base64, json, os, sys, time

PNG = base64.b64encode(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x20\0\0\0\x10\x08\x06\0\0\0" + b"\0" * 8).decode()
tools = [
    {"name": "echo", "description": "Echo the text back", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
    {"name": "fail", "description": "Always reports an error", "inputSchema": {"type": "object"}},
    {"name": "image", "description": "Returns an image and a caption", "inputSchema": {"type": "object"}},
    {"name": "slow", "description": "Takes five seconds", "inputSchema": {"type": "object"}},
    {"name": "add_tool", "description": "Adds a tool and announces it", "inputSchema": {"type": "object"}},
    {"name": "crash", "description": "Exits abruptly", "inputSchema": {"type": "object"}},
    {"name": "structured", "description": "Only structured content", "inputSchema": {"type": "object"}},
    {"name": "env", "description": "Reads MCP_TEST_VAR and the working directory", "inputSchema": {"type": "object"}},
]
page2 = [{"name": "page two tool!", "description": "Found through pagination", "inputSchema": {"type": "object"}}]


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def log(msg):
    sys.stderr.write(msg + "\n")
    sys.stderr.flush()


sys.stdout.write("starting up, this line is not JSON\n")
sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method, mid = msg.get("method"), msg.get("id")
    if method is None:  # a reply to a request we sent
        if mid == "srv-ping":
            log("pong-ok")
        continue
    if method == "initialize":
        if os.environ.get("FAKE_MCP_NO_INIT"):
            continue
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": os.environ.get("FAKE_MCP_VERSION", msg["params"]["protocolVersion"]),
            "capabilities": {"tools": {"listChanged": True}, "prompts": {}, "resources": {}},
            "serverInfo": {"name": "fake", "version": "1"}}})
    elif method == "notifications/initialized":
        send({"jsonrpc": "2.0", "id": "srv-ping", "method": "ping"})
    elif method == "notifications/cancelled":
        log("cancelled:%s" % msg["params"]["requestId"])
    elif method == "ping":
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
    elif method == "tools/list":
        if msg["params"].get("cursor") == "p2":
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": page2}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": tools, "nextCursor": "p2"}})
    elif method == "tools/call":
        name, args = msg["params"]["name"], msg["params"].get("arguments", {})
        if name == "echo":
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "echo: " + args.get("text", "")}]}})
        elif name == "fail":
            send({"jsonrpc": "2.0", "id": mid, "result": {"isError": True, "content": [{"type": "text", "text": "boom"}]}})
        elif name == "image":
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "image", "data": PNG, "mimeType": "image/png"}, {"type": "text", "text": "a caption"}]}})
        elif name == "slow":
            time.sleep(5)
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "finally"}]}})
        elif name == "add_tool":
            tools.append({"name": "added", "description": "Appeared later", "inputSchema": {"type": "object"}})
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "ok"}]}})
            send({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
        elif name == "crash":
            log("about to crash")
            os._exit(3)
        elif name == "structured":
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [], "structuredContent": {"answer": 42}}})
        elif name == "env":
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "%s|%s" % (os.environ.get("MCP_TEST_VAR", ""), os.getcwd())}]}})
        elif name == "added":
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "the added tool works"}]}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32602, "message": "Unknown tool: " + name}})
    elif method == "prompts/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"prompts": [{"name": "greet", "description": "Greets someone", "arguments": [{"name": "who", "required": True}, {"name": "tone"}]}]}})
    elif method == "prompts/get":
        a = msg["params"].get("arguments", {})
        send({"jsonrpc": "2.0", "id": mid, "result": {"messages": [{"role": "user", "content": {"type": "text", "text": "Say hello to %s in a %s tone." % (a.get("who"), a.get("tone", "neutral"))}}]}})
    elif method == "resources/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"resources": [{"uri": "file:///notes.txt", "name": "notes", "description": "My notes", "mimeType": "text/plain"}]}})
    elif method == "resources/read":
        if msg["params"]["uri"] == "file:///notes.txt":
            send({"jsonrpc": "2.0", "id": mid, "result": {"contents": [{"uri": "file:///notes.txt", "mimeType": "text/plain", "text": "remember the milk"}]}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32002, "message": "Resource not found"}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "Method not found: " + method}})
