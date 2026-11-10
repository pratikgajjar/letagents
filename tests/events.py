#!/usr/bin/env python3
"""MCP Events check for tests/e2e.sh: drive `letagents mcp --as hooky` on alice's machine over stdio, with a
local webhook receiver, and send it a message from bob.

    LETAGENTS_HOME=<alice> LETAGENTS_EVENTS_ALLOW_PRIVATE=1 ... python3 tests/events.py <bin> <bob home>

Prints one line per check ("ok ..." / "FAIL ..."); exits 1 if any check fails.
"""
import base64, hashlib, hmac, http.server, json, os, queue, subprocess, sys, threading, time

BIN, BOB = sys.argv[1], sys.argv[2]
fails = 0


def check(name, cond):
    global fails
    print(("ok   " if cond else "FAIL ") + name)
    fails += 0 if cond else 1


# ---------- webhook receiver (Standard Webhooks verification) ----------
SECRET = os.urandom(32)
WHSEC = "whsec_" + base64.b64encode(SECRET).decode()
hooks = queue.Queue()


class Hook(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers["content-length"]))
        wid, ts = self.headers["webhook-id"], self.headers["webhook-timestamp"]
        want = "v1," + base64.b64encode(hmac.new(SECRET, f"{wid}.{ts}.".encode() + body, hashlib.sha256).digest()).decode()
        signed = want in self.headers.get("webhook-signature", "").split(" ")
        msg = json.loads(body)
        hooks.put({"signed": signed, "sub": self.headers.get("X-MCP-Subscription-Id"), "body": msg})
        reply = json.dumps({"challenge": msg["challenge"]}).encode() if msg.get("type") == "verification" else b""
        self.send_response(200)
        self.send_header("content-length", str(len(reply)))
        self.end_headers()
        self.wfile.write(reply)

    def log_message(self, *a):
        pass


srv = http.server.HTTPServer(("127.0.0.1", 0), Hook)
threading.Thread(target=srv.serve_forever, daemon=True).start()
URL = f"http://127.0.0.1:{srv.server_address[1]}/hook"

# ---------- the MCP server ----------
p = subprocess.Popen([BIN, "mcp", "--as", "hooky"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
lines = queue.Queue()
threading.Thread(target=lambda: [lines.put(json.loads(l)) for l in p.stdout], daemon=True).start()
pending = []  # messages read while waiting for something else


def send(msg):
    p.stdin.write(json.dumps(msg) + "\n")
    p.stdin.flush()


def wait(pred, secs=15):
    for i, m in enumerate(pending):
        if pred(m):
            return pending.pop(i)
    end = time.time() + secs
    while time.time() < end:
        try:
            m = lines.get(timeout=max(0.1, end - time.time()))
        except queue.Empty:
            break
        if pred(m):
            return m
        pending.append(m)
    return None


def rpc(i, method, params=None):
    send({"jsonrpc": "2.0", "id": i, "method": method, "params": params or {}})
    return wait(lambda m: m.get("id") == i)


r = rpc(1, "initialize", {"protocolVersion": "2026-07-28", "clientInfo": {"name": "events-test"}, "capabilities": {}})
check("initialize declares the events capability", r and "events" in r["result"]["capabilities"])
r = rpc(2, "events/list")
check("events/list has message.received with poll, push, webhook",
      r and r["result"]["events"][0]["name"] == "message.received" and r["result"]["events"][0]["delivery"] == ["poll", "push", "webhook"])

r = rpc(3, "events/poll", {"name": "message.received", "arguments": {"include_text": True}, "cursor": None})
cursor = r and r["result"]["cursor"]
check(f"events/poll from now: no events, a cursor ({cursor})", r and r["result"]["events"] == [] and cursor)

send({"jsonrpc": "2.0", "id": 4, "method": "events/stream", "params": {"name": "message.received", "arguments": {"kind": "send"}, "cursor": None}})
a = wait(lambda m: m.get("method") == "notifications/events/active")
check("events/stream: notifications/events/active", a and a["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"] == 4)

r = rpc(5, "events/subscribe", {"name": "message.received", "arguments": {}, "delivery": {"mode": "webhook", "url": URL, "secret": WHSEC}, "ttlMs": 600000})
v = hooks.get(timeout=5) if not hooks.empty() or True else None
check("webhook: signed verification challenge before the subscription", v["signed"] and v["body"].get("type") == "verification")
sub = r and r.get("result", {}).get("id")
check(f"events/subscribe: id and refreshBefore ({sub})", sub and r["result"]["refreshBefore"])

r = rpc(6, "events/subscribe", {"name": "message.received", "delivery": {"url": URL, "secret": "whsec_short"}})
check("events/subscribe: bad secret is InvalidParams", r and r.get("error", {}).get("code") == -32602)
r = rpc(7, "events/subscribe", {"name": "nope", "delivery": {"url": URL, "secret": WHSEC}})
check("events/subscribe: unknown event is NotFound (-32023)", r and r.get("error", {}).get("code") == -32023)

# ---------- a message arrives ----------
env = dict(os.environ, LETAGENTS_HOME=BOB)
subprocess.run([BIN, "send", "alice/hooky", "hello events", "--as", "worker"], env=env, capture_output=True, timeout=30)

e = wait(lambda m: m.get("method") == "notifications/events/event", 20)
check("push: notifications/events/event from bob/worker, no text by default",
      e and e["params"]["data"]["from"] == "bob/worker" and e["params"]["data"]["kind"] == "send" and "text" not in e["params"]["data"])
h = None
try:
    h = hooks.get(timeout=20)
except queue.Empty:
    pass
check("webhook: signed event delivery with X-MCP-Subscription-Id",
      h and h["signed"] and h["sub"] == sub and h["body"]["name"] == "message.received" and h["body"]["data"]["from"] == "bob/worker")
check("same eventId on push and webhook", e and h and e["params"]["eventId"] == h["body"]["eventId"])

r = rpc(8, "events/poll", {"name": "message.received", "arguments": {"include_text": True}, "cursor": cursor})
evs = r and r["result"]["events"]
check("events/poll with cursor: the event, with text (include_text)", evs and evs[0]["data"]["text"] == "hello events")

send({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 4}})
r = rpc(9, "events/unsubscribe", {"name": "message.received", "arguments": {}, "delivery": {"url": URL}})
check("events/unsubscribe", r and r.get("result") == {})
r = rpc(10, "events/unsubscribe", {"name": "message.received", "arguments": {}, "delivery": {"url": URL}})
check("events/unsubscribe again: NotFound", r and r.get("error", {}).get("code") == -32023)

# The message stays in the inbox until the agent reads it.
out = subprocess.run([BIN, "inbox", "--as", "hooky"], capture_output=True, text=True, timeout=30).stdout
check("the message still waits in the inbox", "hello events" in out)

p.stdin.close()
p.wait(timeout=10)
srv.shutdown()
sys.exit(1 if fails else 0)
