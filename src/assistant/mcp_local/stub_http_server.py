# A stub MCP server for gz's tests: Streamable HTTP at /mcp (with an API
# key), /listen/mcp (with a GET stream for the server's own messages),
# /hang/mcp (answers initialize, then never answers), /flood/mcp (an
# endless event in reply to a call), /hop/mcp and
# /moved/mcp (redirects within and out of the origin), and /secure/mcp,
# /plain/mcp, /evil/mcp and /badissuer/mcp behind OAuth; the older HTTP+SSE
# transport at /sse; stub OAuth authorization servers under /auth (OAuth
# metadata) and /oidc (OpenID metadata). POST /control changes settings.
# Prints its port, then serves.
import base64, hashlib, json, queue, sys, threading, time, urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

STATE = {
    "sessions": set(),
    "next_session": 1,
    "extra_tool": False,
    "valid_tokens": set(),
    "codes": {},
    "registrations": 0,
    "legacy": {},
    "seen_headers": [],
    "token_requests": [],
    "answers": [],
    "resumed_from": [],
    "deletes": [],
    "listens": 0,
    # Changed through POST /control.
    "basic": False,
    "expires_in": 3600,
}
# OAuth-protected endpoints: whether their 401 names the metadata, and where.
PROTECTED = {
    "/secure/mcp": "/.well-known/oauth-protected-resource/secure/mcp",
    "/plain/mcp": None,
    "/evil/mcp": "/evil-metadata",
    "/badissuer/mcp": "/.well-known/oauth-protected-resource/badissuer/mcp",
}
LOCK = threading.Lock()
API_KEY = "k-123"

def tools():
    listed = [{"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}},
              {"name": "change", "inputSchema": {"type": "object"}},
              {"name": "expire", "inputSchema": {"type": "object"}},
              {"name": "rotate", "inputSchema": {"type": "object"}},
              {"name": "big", "inputSchema": {"type": "object"}},
              {"name": "expire_and_change", "inputSchema": {"type": "object"}}]
    if STATE["extra_tool"]:
        listed.append({"name": "added", "inputSchema": {"type": "object"}})
    return listed

def answer(message):
    """The JSON-RPC answer and any messages to send before it."""
    method, mid = message.get("method"), message.get("id")
    params = message.get("params") or {}
    before = []
    if method == "initialize":
        result = {"protocolVersion": params["protocolVersion"],
                  "capabilities": {"tools": {"listChanged": True}},
                  "serverInfo": {"name": "stub", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": tools()}
    elif method == "tools/call":
        name, args = params["name"], params.get("arguments") or {}
        if name == "echo":
            result = {"content": [{"type": "text", "text": args.get("text", "")}]}
        elif name == "change":
            STATE["extra_tool"] = True
            before.append({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
            before.append({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"})
            result = {"content": [{"type": "text", "text": "changed"}]}
        elif name == "expire":
            STATE["sessions"].clear()
            result = {"content": [{"type": "text", "text": "expired"}]}
        elif name == "rotate":
            STATE["valid_tokens"].clear()
            result = {"content": [{"type": "text", "text": "rotated"}]}
        elif name == "expire_and_change":
            STATE["sessions"].clear()
            STATE["extra_tool"] = True
            before.append({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
            result = {"content": [{"type": "text", "text": "expired and changed"}]}
        elif name == "big":
            result = {"content": [{"type": "text", "text": "x" * 5000}]}
        else:
            return before, {"jsonrpc": "2.0", "id": mid,
                            "error": {"code": -32602, "message": "Unknown tool: " + name}}
    elif method == "ping":
        result = {}
    else:
        return before, {"jsonrpc": "2.0", "id": mid,
                        "error": {"code": -32601, "message": "no " + str(method)}}
    return before, {"jsonrpc": "2.0", "id": mid, "result": result}

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def log_message(self, *args):
        pass

    def base(self):
        return "http://127.0.0.1:%d" % self.server.server_address[1]

    def send_json(self, status, value, headers=()):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        for key, val in headers:
            self.send_header(key, val)
        self.end_headers()
        self.wfile.write(body)

    def send_empty(self, status, headers=()):
        self.send_response(status)
        self.send_header("Content-Length", "0")
        for key, val in headers:
            self.send_header(key, val)
        self.end_headers()

    def body(self):
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length)

    # --- OAuth authorization server ---
    def do_GET(self):
        url = urllib.parse.urlparse(self.path)
        query = dict(urllib.parse.parse_qsl(url.query))
        base = self.base()
        if url.path == "/.well-known/oauth-protected-resource/secure/mcp":
            return self.send_json(200, {"resource": base + "/secure/mcp",
                                        "authorization_servers": [base + "/auth"],
                                        "scopes_supported": ["tools"]})
        if url.path == "/.well-known/oauth-protected-resource/plain/mcp":
            # The resource is the whole origin, which covers /plain/mcp.
            return self.send_json(200, {"resource": base,
                                        "authorization_servers": [base + "/oidc"]})
        if url.path == "/evil-metadata":
            return self.send_json(200, {"resource": "https://elsewhere.test/mcp",
                                        "authorization_servers": [base + "/auth"]})
        if url.path == "/.well-known/oauth-protected-resource/badissuer/mcp":
            return self.send_json(200, {"resource": base + "/badissuer/mcp",
                                        "authorization_servers": [base + "/bad"]})
        if url.path == "/.well-known/oauth-authorization-server/bad":
            return self.send_json(200, dict(self.endpoints(base + "/bad"),
                                            issuer=base + "/someone-else"))
        if url.path == "/.well-known/oauth-authorization-server/auth":
            return self.send_json(200, self.endpoints(base + "/auth"))
        if url.path == "/.well-known/openid-configuration/oidc":
            return self.send_json(200, self.endpoints(base + "/oidc"))
        if url.path == "/auth/authorize":
            code = "code-%d" % (len(STATE["codes"]) + 1)
            STATE["codes"][code] = (query["code_challenge"], query["client_id"],
                                    query["redirect_uri"], query.get("resource"),
                                    query.get("scope"))
            target = query["redirect_uri"] + "?" + urllib.parse.urlencode(
                {"code": code, "state": query["state"]})
            return self.send_empty(302, [("Location", target)])
        if url.path == "/state":
            with LOCK:
                snapshot = {"registrations": STATE["registrations"],
                            "codes": {k: list(v) for k, v in STATE["codes"].items()},
                            "seen_headers": STATE["seen_headers"],
                            "token_requests": STATE["token_requests"],
                            "answers": STATE["answers"],
                            "resumed_from": STATE["resumed_from"],
                            "deletes": STATE["deletes"]}
            return self.send_json(200, snapshot)
        if url.path == "/sse":
            return self.legacy_stream()
        if url.path == "/listen/mcp":
            return self.listen_stream()
        if url.path == "/mcp" or url.path in PROTECTED:
            return self.send_empty(405)
        return self.send_empty(404)

    def endpoints(self, issuer):
        base = self.base()
        return {"issuer": issuer,
                "authorization_endpoint": base + "/auth/authorize",
                "token_endpoint": base + "/auth/token",
                "registration_endpoint": base + "/auth/register",
                "code_challenge_methods_supported": ["S256"]}

    def listen_stream(self):
        """The first GET announces a tool change and pings, then drops; a
        reopened stream records where it resumed from and stays quiet."""
        resumed = self.headers.get("Last-Event-ID")
        with LOCK:
            STATE["listens"] += 1
            first = STATE["listens"] == 1
            if resumed:
                STATE["resumed_from"].append(resumed)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        if not first:
            time.sleep(30)
            return
        # Late enough that gz has listed the tools before they change.
        time.sleep(0.5)
        STATE["extra_tool"] = True
        changed = {"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}
        ping = {"jsonrpc": "2.0", "id": "srv-get", "method": "ping"}
        self.wfile.write(("id: 1\ndata: %s\n\n" % json.dumps(changed)).encode())
        self.wfile.write(("id: 2\ndata: %s\n\n" % json.dumps(ping)).encode())
        self.wfile.flush()

    def legacy_stream(self):
        if self.headers.get("X-Api-Key") != API_KEY:
            return self.send_empty(403)
        with LOCK:
            sid = "L%d" % (len(STATE["legacy"]) + 1)
            outbox = queue.Queue()
            STATE["legacy"][sid] = outbox
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        self.wfile.write(("event: endpoint\ndata: /messages?session=%s\n\n" % sid).encode())
        self.wfile.flush()
        while True:
            try:
                message = outbox.get(timeout=30)
            except queue.Empty:
                return
            if message is None:
                return
            # Split the frame in two writes to exercise chunk reassembly.
            frame = ("event: message\ndata: %s\n\n" % json.dumps(message)).encode()
            self.wfile.write(frame[:7])
            self.wfile.flush()
            self.wfile.write(frame[7:])
            self.wfile.flush()

    def do_DELETE(self):
        with LOCK:
            STATE["deletes"].append({"path": self.path,
                                     "auth": self.headers.get("Authorization")})
        STATE["sessions"].discard(self.headers.get("Mcp-Session-Id"))
        return self.send_empty(200)

    def do_POST(self):
        url = urllib.parse.urlparse(self.path)
        query = dict(urllib.parse.parse_qsl(url.query))
        raw = self.body()
        if url.path == "/control":
            with LOCK:
                STATE.update(json.loads(raw))
            return self.send_empty(204)
        if url.path == "/auth/register":
            request = json.loads(raw)
            with LOCK:
                STATE["registrations"] += 1
                number = STATE["registrations"]
            reply = {"client_id": "client-%d" % number,
                     "redirect_uris": request["redirect_uris"]}
            if STATE["basic"]:
                reply["client_secret"] = "secret-%d" % number
                reply["token_endpoint_auth_method"] = "client_secret_basic"
            return self.send_json(201, reply)
        if url.path == "/auth/token":
            form = dict(urllib.parse.parse_qsl(raw.decode()))
            recorded = dict(form)
            recorded["_authorization"] = self.headers.get("Authorization")
            STATE["token_requests"].append(recorded)
            if "client_id" not in form:
                basic = (self.headers.get("Authorization") or "")[len("Basic "):]
                client, _, _secret = base64.b64decode(basic).decode().partition(":")
                form["client_id"] = client
            if form["grant_type"] == "authorization_code":
                challenge, client, redirect, resource, scope = STATE["codes"].pop(form["code"])
                digest = hashlib.sha256(form["code_verifier"].encode()).digest()
                expected = base64.urlsafe_b64encode(digest).rstrip(b"=").decode()
                if expected != challenge or client != form["client_id"] \
                        or redirect != form["redirect_uri"]:
                    return self.send_json(400, {"error": "invalid_grant"})
                STATE["valid_tokens"].add("tok-1")
                return self.send_json(200, {"access_token": "tok-1", "token_type": "Bearer",
                                            "refresh_token": "ref-1",
                                            "expires_in": STATE["expires_in"]})
            if form["grant_type"] == "refresh_token" and form["refresh_token"] == "ref-1":
                STATE["valid_tokens"].add("tok-2")
                return self.send_json(200, {"access_token": "tok-2", "token_type": "Bearer",
                                            "expires_in": STATE["expires_in"]})
            return self.send_json(400, {"error": "invalid_grant"})
        if url.path == "/messages":
            outbox = STATE["legacy"].get(query.get("session"))
            if outbox is None:
                return self.send_empty(404)
            message = json.loads(raw)
            if "method" in message:
                before, reply = answer(message)
                for item in before:
                    outbox.put(item)
                if "id" in message:
                    outbox.put(reply)
            return self.send_empty(202)
        if url.path in PROTECTED:
            auth = self.headers.get("Authorization") or ""
            if not auth.startswith("Bearer ") or auth[7:] not in STATE["valid_tokens"]:
                meta = PROTECTED[url.path]
                challenge = 'Bearer resource_metadata="%s"' % (self.base() + meta) \
                    if meta else "Bearer"
                return self.send_empty(401, [("WWW-Authenticate", challenge)])
            return self.streamable(raw)
        if url.path == "/listen/mcp":
            return self.streamable(raw)
        if url.path == "/hang/mcp":
            return self.hanging(raw)
        if url.path == "/flood/mcp":
            return self.flooding(raw)
        if url.path == "/hop/mcp":
            return self.send_empty(307, [("Location", "/mcp")])
        if url.path == "/moved/mcp":
            port = self.server.server_address[1]
            return self.send_empty(307, [("Location", "http://localhost:%d/mcp" % port)])
        if url.path == "/mcp":
            with LOCK:
                STATE["seen_headers"].append({"X-Api-Key": self.headers.get("X-Api-Key"),
                                              "version": self.headers.get("MCP-Protocol-Version")})
            if self.headers.get("X-Api-Key") != API_KEY:
                return self.send_json(403, {"error": "bad api key"})
            return self.streamable(raw)
        return self.send_empty(404)

    def streamable(self, raw):
        message = json.loads(raw)
        method = message.get("method")
        session = self.headers.get("Mcp-Session-Id")
        if method == "initialize":
            with LOCK:
                session = "S%d" % STATE["next_session"]
                STATE["next_session"] += 1
                STATE["sessions"].add(session)
            _, reply = answer(message)
            return self.send_json(200, reply, [("Mcp-Session-Id", session)])
        if session not in STATE["sessions"]:
            return self.send_empty(404 if session else 400)
        if "id" not in message:
            return self.send_empty(202)
        if "method" not in message:
            with LOCK:
                STATE["answers"].append(message["id"])
            return self.send_empty(202)
        before, reply = answer(message)
        if method == "tools/call":
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            for item in before + [reply]:
                self.wfile.write(("data: %s\n\n" % json.dumps(item)).encode())
                self.wfile.flush()
            return
        return self.send_json(200, reply)

    def hanging(self, raw):
        """Answers initialize and takes notifications/initialized; then
        holds every other message without ever answering."""
        message = json.loads(raw)
        method = message.get("method")
        if method == "initialize":
            _, reply = answer(message)
            return self.send_json(200, reply)
        if method == "notifications/initialized":
            return self.send_empty(202)
        time.sleep(60)
        return self.send_empty(202)

    def flooding(self, raw):
        """Answers tools/call with an event that never ends: data lines,
        past 8 MiB, and no blank line."""
        message = json.loads(raw)
        if message.get("method") != "tools/call":
            if "id" not in message:
                return self.send_empty(202)
            _, reply = answer(message)
            return self.send_json(200, reply)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        line = ("data: " + "x" * 65536 + "\n").encode()
        try:
            for _ in range(160):
                self.wfile.write(line)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
server.daemon_threads = True
print(server.server_address[1], flush=True)
server.serve_forever()
