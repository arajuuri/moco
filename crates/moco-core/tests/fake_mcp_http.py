#!/usr/bin/env python3
"""A tiny MCP server over Streamable HTTP, optionally with an OAuth authorization server, for moco's tests.

Usage: fake_mcp_http.py LOGFILE [--auth] [--no-get] [--expire-session-once] [--expires N] [--no-metadata] [--deny]
Prints its port on the first line. Everything notable is appended to LOGFILE; protocol mistakes by the client are
logged as lines starting with "VIOLATION:"."""
import base64, hashlib, json, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

LOG = sys.argv[1]
FLAGS = set(a for a in sys.argv[2:] if a.startswith("--"))
EXPIRES = int(sys.argv[sys.argv.index("--expires") + 1]) if "--expires" in sys.argv else 3600
LOCK = threading.Lock()
COND = threading.Condition(LOCK)
STATE = {"sessions": set(), "next_session": 1, "expired_once": False, "tokens": set(), "refresh": set(), "codes": {}, "clients": {}, "next_token": 1,
         "notifications": [], "tools": ["echo", "sse_echo", "slow", "add_tool", "boom", "structured"]}
PORT = {"n": 0}


def log(line):
    with open(LOG, "a") as f:
        f.write(line + "\n")


def base():
    return "http://127.0.0.1:%d" % PORT["n"]


def tool_defs():
    d = {"echo": "Echo the text back", "sse_echo": "Echo, answered over an SSE stream", "slow": "Takes five seconds", "add_tool": "Adds a tool and announces it", "boom": "HTTP 500", "structured": "Structured only"}
    out = [{"name": t, "description": d.get(t, "added later"), "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}} for t in STATE["tools"]]
    return out


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    # --- helpers
    def send_json(self, status, obj, headers=None):
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def empty(self, status, headers=None):
        self.send_response(status)
        self.send_header("Content-Length", "0")
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()

    def body(self):
        return self.rfile.read(int(self.headers.get("Content-Length") or 0))

    def authorized(self):
        if "--auth" not in FLAGS:
            return True
        auth = self.headers.get("Authorization", "")
        with LOCK:
            ok = auth.startswith("Bearer ") and auth[7:] in STATE["tokens"]
        if not ok:
            self.send_response(401)
            if "--no-metadata" not in FLAGS:
                self.send_header("WWW-Authenticate", 'Bearer resource_metadata="%s/.well-known/oauth-protected-resource"' % base())
            else:
                self.send_header("WWW-Authenticate", "Bearer")
            self.send_header("Content-Length", "0")
            self.end_headers()
        return ok

    def moved(self, method):
        """/old is where the endpoint used to be: a same-origin redirect to /mcp."""
        if method != "GET":
            self.body()
        log("redirected %s /old" % method)
        self.empty(307, {"Location": "/mcp"})

    # --- routing
    def do_GET(self):
        u = urlparse(self.path)
        if u.path == "/old":
            return self.moved("GET")
        if u.path == "/.well-known/oauth-protected-resource" and "--no-metadata" not in FLAGS:
            return self.send_json(200, {"resource": base() + "/mcp", "authorization_servers": [base()], "scopes_supported": ["mcp.read"]})
        if u.path == "/.well-known/oauth-authorization-server" and "--no-metadata" not in FLAGS:
            return self.send_json(200, {"issuer": base(), "authorization_endpoint": base() + "/authorize", "token_endpoint": base() + "/token",
                                        "registration_endpoint": base() + "/register", "code_challenge_methods_supported": ["S256"]})
        if u.path == "/authorize":
            return self.authorize(parse_qs(u.query))
        if u.path == "/mcp":
            return self.mcp_get()
        self.empty(404)

    def do_POST(self):
        u = urlparse(self.path)
        if u.path == "/old":
            return self.moved("POST")
        if u.path == "/register":
            reg = json.loads(self.body())
            if not any(r.startswith("http://127.0.0.1:") for r in reg.get("redirect_uris", [])):
                log("VIOLATION: registration without a loopback redirect uri")
            with LOCK:
                cid = "cid-%d" % (len(STATE["clients"]) + 1)
                STATE["clients"][cid] = reg["redirect_uris"]
            log("registered " + cid)
            return self.send_json(201, {"client_id": cid})
        if u.path == "/token":
            return self.token(parse_qs(self.body().decode()))
        if u.path == "/mcp":
            return self.mcp_post()
        self.empty(404)

    def do_DELETE(self):
        if urlparse(self.path).path == "/old":
            return self.moved("DELETE")
        log("DELETE session=%s" % self.headers.get("Mcp-Session-Id"))
        self.empty(200)

    # --- OAuth
    def authorize(self, q):
        g = lambda k: q.get(k, [""])[0]
        problems = []
        if g("response_type") != "code": problems.append("response_type")
        if g("code_challenge_method") != "S256" or not g("code_challenge"): problems.append("pkce")
        if g("resource") != base() + "/mcp": problems.append("resource=%r" % g("resource"))
        if not g("state"): problems.append("state")
        with LOCK:
            known = g("client_id") in STATE["clients"] or g("client_id") == "preset-client"
            registered = g("redirect_uri") in STATE["clients"].get(g("client_id"), [g("redirect_uri")])
        if not known: problems.append("unknown client")
        if not registered: problems.append("redirect_uri not registered")
        if problems:
            log("VIOLATION: authorize: " + ", ".join(problems))
            return self.empty(400)
        if "--deny" in FLAGS:
            loc = "%s?error=access_denied&error_description=user+said+no&state=%s" % (g("redirect_uri"), g("state"))
        else:
            code = "code-%d" % (len(STATE["codes"]) + 1)
            with LOCK:
                STATE["codes"][code] = {"challenge": g("code_challenge"), "client": g("client_id"), "redirect": g("redirect_uri"), "resource": g("resource")}
            loc = "%s?code=%s&state=%s" % (g("redirect_uri"), code, g("state"))
        log("authorize ok scope=%s" % g("scope"))
        self.send_response(302)
        self.send_header("Location", loc)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def token(self, q):
        g = lambda k: q.get(k, [""])[0]
        with LOCK:
            if g("grant_type") == "authorization_code":
                c = STATE["codes"].pop(g("code"), None)
                if not c:
                    return self.send_json(400, {"error": "invalid_grant", "error_description": "unknown code"})
                digest = base64.urlsafe_b64encode(hashlib.sha256(g("code_verifier").encode()).digest()).rstrip(b"=").decode()
                if digest != c["challenge"]: log("VIOLATION: token: PKCE verifier does not match the challenge")
                if g("redirect_uri") != c["redirect"]: log("VIOLATION: token: redirect_uri differs")
                if g("client_id") != c["client"]: log("VIOLATION: token: client_id differs")
                if g("resource") != c["resource"]: log("VIOLATION: token: resource differs")
                n = STATE["next_token"]; STATE["next_token"] += 1
                STATE["tokens"].add("tok-%d" % n); STATE["refresh"].add("ref-%d" % n)
                log("token issued tok-%d" % n)
                return self.send_json(200, {"access_token": "tok-%d" % n, "token_type": "Bearer", "expires_in": EXPIRES, "refresh_token": "ref-%d" % n})
            if g("grant_type") == "refresh_token":
                if g("refresh_token") not in STATE["refresh"]:
                    log("refresh refused")
                    return self.send_json(400, {"error": "invalid_grant"})
                STATE["refresh"].discard(g("refresh_token"))
                n = STATE["next_token"]; STATE["next_token"] += 1
                STATE["tokens"].add("tok-%d" % n); STATE["refresh"].add("ref-%d" % n)
                log("refreshed -> tok-%d" % n)
                return self.send_json(200, {"access_token": "tok-%d" % n, "token_type": "Bearer", "expires_in": EXPIRES, "refresh_token": "ref-%d" % n})
        self.send_json(400, {"error": "unsupported_grant_type"})

    # --- MCP
    def check_session(self, method):
        if method == "initialize":
            return True
        sid = self.headers.get("Mcp-Session-Id")
        with LOCK:
            if sid not in STATE["sessions"]:
                self.empty(404)
                return False
            if "--expire-session-once" in FLAGS and not STATE["expired_once"]:
                STATE["expired_once"] = True
                STATE["sessions"].discard(sid)
                log("session expired")
                self.empty(404)
                return False
        if not self.headers.get("MCP-Protocol-Version"):
            log("VIOLATION: no MCP-Protocol-Version header on " + method)
        return True

    def mcp_post(self):
        if not self.authorized():
            return
        msg = json.loads(self.body())
        method, mid = msg.get("method"), msg.get("id")
        if self.headers.get("X-Custom"):
            log("custom=" + self.headers["X-Custom"])
        if method is None:  # a reply to something we sent
            if mid == "srv-ping":
                log("pong-ok")
            return self.empty(202)
        if mid is None:  # notification
            if method == "notifications/cancelled":
                log("cancelled:%s" % msg["params"]["requestId"])
            return self.empty(202)
        if not self.check_session(method):
            return
        log("POST " + method)
        if method == "initialize":
            with LOCK:
                sid = "sess-%d" % STATE["next_session"]; STATE["next_session"] += 1
                STATE["sessions"].add(sid)
            return self.send_json(200, {"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": msg["params"]["protocolVersion"],
                "capabilities": {"tools": {"listChanged": True}, "prompts": {}, "resources": {}}, "serverInfo": {"name": "fake-http", "version": "1"}}},
                {"Mcp-Session-Id": sid})
        ok = lambda result: self.send_json(200, {"jsonrpc": "2.0", "id": mid, "result": result})
        if method == "tools/list":
            return ok({"tools": tool_defs()})
        if method == "prompts/list":
            return ok({"prompts": [{"name": "greet", "description": "Greets", "arguments": [{"name": "who", "required": True}]}]})
        if method == "prompts/get":
            return ok({"messages": [{"role": "user", "content": {"type": "text", "text": "Say hi to %s." % msg["params"]["arguments"].get("who")}}]})
        if method == "resources/list":
            return ok({"resources": [{"uri": "mem://note", "name": "note", "description": "A note"}]})
        if method == "resources/read":
            return ok({"contents": [{"uri": "mem://note", "text": "remote note text"}]})
        if method == "tools/call":
            name, args = msg["params"]["name"], msg["params"].get("arguments", {})
            if name == "echo":
                return ok({"content": [{"type": "text", "text": "echo: " + args.get("text", "")}]})
            if name == "structured":
                return ok({"content": [], "structuredContent": {"answer": 42}})
            if name == "boom":
                self.send_response(500); self.send_header("Content-Length", "4"); self.end_headers(); self.wfile.write(b"oops"); return
            if name == "slow":
                time.sleep(5)
                return ok({"content": [{"type": "text", "text": "finally"}]})
            if name == "add_tool":
                with COND:
                    STATE["tools"].append("added")
                    STATE["notifications"].append({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
                    COND.notify_all()
                return ok({"content": [{"type": "text", "text": "ok"}]})
            if name == "sse_echo":
                # Answer as an SSE stream: a notification, a request to the client (ping), then the response.
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                ev = lambda o: self.wfile.write(("event: message\ndata: %s\n\n" % json.dumps(o)).encode())
                ev({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": "hello"}})
                ev({"jsonrpc": "2.0", "id": "srv-ping", "method": "ping"})
                self.wfile.flush()
                time.sleep(0.4)  # let the client's pong arrive on its own connection
                ev({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "sse: " + args.get("text", "")}]}})
                self.wfile.flush()
                self.close_connection = True
                return
            return self.send_json(200, {"jsonrpc": "2.0", "id": mid, "error": {"code": -32602, "message": "Unknown tool: " + name}})
        self.send_json(200, {"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "Method not found: " + method}})

    def mcp_get(self):
        if "--no-get" in FLAGS:
            return self.empty(405)
        if not self.authorized():
            return
        with LOCK:
            if self.headers.get("Mcp-Session-Id") not in STATE["sessions"]:
                return self.empty(404)
        log("GET stream opened")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        try:
            while True:
                with COND:
                    if not STATE["notifications"]:
                        COND.wait(0.5)
                    pending, STATE["notifications"] = STATE["notifications"], []
                if pending:
                    for n in pending:
                        self.wfile.write(("data: %s\n\n" % json.dumps(n)).encode())
                else:
                    self.wfile.write(b": keepalive\n\n")
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            return


s = ThreadingHTTPServer(("127.0.0.1", 0), H)
s.daemon_threads = True
PORT["n"] = s.server_address[1]
print(PORT["n"], flush=True)
s.serve_forever()
