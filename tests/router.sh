#!/usr/bin/env bash
# The router watch and --no-intercom, on one daemon with no relay and no pi-intercom: a local mailbox (such
# as Sova) holds `watch` with `router: true` and takes every message no watcher takes, with its target in
# `to`. What it refuses waits in the spool and is offered again when a router connects.
#
#   tests/router.sh                    # builds cli/target/debug/letagents first
#   LETAGENTS_BIN=path tests/router.sh
#
# Needs: cargo, python3.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
T=$(mktemp -d)
FAILS=0
DAEMON=
cleanup() {
  [ -n "$DAEMON" ] && kill "$DAEMON" 2>/dev/null
  wait 2>/dev/null
  if [ "$FAILS" -ne 0 ] || [ -n "${KEEP:-}" ]; then echo "logs kept in $T"; else rm -rf "$T"; fi
}
trap cleanup EXIT
ok() { echo "ok   $1"; }
fail() { echo "FAIL $1"; FAILS=$((FAILS + 1)); }
die() { fail "$1"; cat "$T/daemon.log"; exit 1; }

BIN=${LETAGENTS_BIN:-}
if [ -z "$BIN" ]; then
  cargo build -q --manifest-path "$ROOT/cli/Cargo.toml" || die "cli build"
  BIN=$ROOT/cli/target/debug/letagents
fi
mkdir -p "$T/pi"
export LETAGENTS_HOME="$T/a" PI_CODING_AGENT_DIR="$T/pi" LETAGENTS_RELAY=http://127.0.0.1:9 LETAGENTS_AUTH_HOST=letagents.talk
"$BIN" init alice >/dev/null || die "init"
"$BIN" daemon --no-intercom >"$T/daemon.log" 2>&1 &
DAEMON=$!
for _ in $(seq 50); do [ -S "$T/a/daemon.sock" ] && break; sleep 0.2; done
grep -q "pi-intercom off" "$T/daemon.log" && ok "--no-intercom: the daemon stays off the broker" || fail "--no-intercom not logged"

python3 - "$T/a/daemon.sock" <<'PY'
import json, socket, struct, sys, threading, time
SOCK = sys.argv[1]
fails = 0
def check(name, cond):
    global fails
    print(("ok   " if cond else "FAIL ") + name)
    fails += 0 if cond else 1
def conn():
    s = socket.socket(socket.AF_UNIX); s.connect(SOCK); s.settimeout(10); return s
def put(s, v):
    b = json.dumps(v).encode(); s.sendall(struct.pack(">I", len(b)) + b)
def get(s):
    n = struct.unpack(">I", s.recv(4, socket.MSG_WAITALL))[0]
    return json.loads(s.recv(n, socket.MSG_WAITALL))
def call(req):
    s = conn(); put(s, req); v = get(s); s.close(); return v

class Router:
    """Takes pushes for the targets in `known`, refuses the rest."""
    def __init__(self, known):
        self.known, self.got = known, []
        self.s = conn(); put(self.s, {"op": "watch", "as": "sova", "router": True})
        assert get(self.s)["ok"]
        threading.Thread(target=self.loop, daemon=True).start()
    def loop(self):
        try:
            while True:
                p = get(self.s)["push"]
                self.got.append(p)
                put(self.s, {"ok": p["to"] in self.known})
        except Exception:
            pass

r = Router({"planner"})
check("status: router on, intercom off", (lambda v: v["router"] and not v["intercom"])(call({"op": "status"})))
v = call({"op": "send", "as": "worker", "to": "alice/planner", "text": "hello planner"})
check("send to a local agent with no watcher: pushed to the router (%s)" % v["sent"][0]["receipt"], v["sent"][0]["receipt"]["status"] == "pushed")
p = r.got[-1]
check("the push names its target and sender", p["to"] == "planner" and p["from"] == "alice/worker" and p["text"].endswith("hello planner"))
v = call({"op": "send", "as": "worker", "to": "alice/ghost", "text": "for ghost"})
check("a target the router refuses waits in the spool (%s)" % v["sent"][0]["receipt"]["status"], v["sent"][0]["receipt"]["status"] == "queued" and call({"op": "status"})["waiting"] == 1)
check("the router is not an agent in the roster", "sova" not in [a["name"] for a in call({"op": "list", "wait_ms": 1})["local"]])
r.s.shutdown(socket.SHUT_RDWR); r.s.close(); time.sleep(0.5)
check("status: router gone when its connection closes", not call({"op": "status"})["router"])
r2 = Router({"ghost"})
for _ in range(50):
    if any(p["to"] == "ghost" for p in r2.got): break
    time.sleep(0.1)
check("a new router gets the spooled mail", any(p["to"] == "ghost" and p["text"].endswith("for ghost") for p in r2.got))
time.sleep(0.3)
check("the spool is empty once the router took it", call({"op": "status"})["waiting"] == 0)
sys.exit(1 if fails else 0)
PY
[ $? -eq 0 ] || FAILS=$((FAILS + 1))
if grep -q "hello planner\|for ghost" "$T/daemon.log"; then fail "the daemon log holds message text"; else ok "no message text in the daemon log"; fi
[ "$FAILS" -eq 0 ] && echo "all router checks passed" || exit 1
