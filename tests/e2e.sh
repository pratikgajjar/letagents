#!/usr/bin/env bash
# End-to-end test: a local relay (wrangler dev), two daemons (alice, bob) with no pi-intercom, and the CLI.
# Prints one line per check; exits non-zero if any check fails.
#
#   tests/e2e.sh                    # builds cli/target/debug/letagents first
#   LETAGENTS_BIN=path tests/e2e.sh # use a prebuilt binary
#
# Needs: cargo, node (npx), worker-build, python3, curl.
set -u
set -m # background jobs get their own process group, so cleanup can stop wrangler and workerd

ROOT=$(cd "$(dirname "$0")/.." && pwd)
T=$(mktemp -d)
PIDS=()
FAILS=0

cleanup() {
  for p in "${PIDS[@]}"; do kill -- "-$p" 2>/dev/null || kill "$p" 2>/dev/null; done
  wait 2>/dev/null
  if [ "$FAILS" -ne 0 ] || [ -n "${KEEP:-}" ]; then echo "logs kept in $T"; else rm -rf "$T"; fi
}
trap cleanup EXIT

ok() { echo "ok   $1"; }
fail() { echo "FAIL $1"; FAILS=$((FAILS + 1)); }
check() { # check <name> <command...>
  local name=$1; shift
  if "$@"; then ok "$name"; else fail "$name"; fi
}
die() { echo "FAIL $1"; FAILS=$((FAILS + 1)); for f in "$T"/*.log; do echo "--- $f"; tail -30 "$f"; done; exit 1; }
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
# wait_for <seconds> <command...>: retry every 0.2 s until the command succeeds
# logged FILE PATTERN OP N: re-counts on every call, so wait_for can poll it
logged() { [ "$(count "$1" "$2")" "$3" "$4" ]; }
wait_for() {
  local n=$(($1 * 5)); shift
  for _ in $(seq "$n"); do "$@" >/dev/null 2>&1 && return 0; sleep 0.2; done
  return 1
}
count() { grep -c -- "$2" "$1" 2>/dev/null || true; }

BIN=${LETAGENTS_BIN:-}
if [ -z "$BIN" ]; then
  cargo build -q --manifest-path "$ROOT/cli/Cargo.toml" || die "cli build"
  BIN=$ROOT/cli/target/debug/letagents
fi

# ---------- relay ----------
PORT=$(free_port)
RELAY=http://127.0.0.1:$PORT
(cd "$ROOT/relay" && exec npx --yes wrangler@latest dev --ip 127.0.0.1 --port "$PORT" --persist-to "$T/state" --show-interactive-dev-session=false >"$T/relay.log" 2>&1) &
PIDS+=($!)
wait_for 300 curl -fsS "$RELAY/health" || die "relay did not start on $RELAY"

# ---------- two machines ----------
mkdir -p "$T/pi" # empty pi dir: no intercom broker, so mail is queued for inbox readers
la() { # la <machine> <letagents args...>
  local m=$1; shift
  LETAGENTS_HOME="$T/$m" PI_CODING_AGENT_DIR="$T/pi" LETAGENTS_RELAY="$RELAY" LETAGENTS_AUTH_HOST=letagents.talk "$BIN" "$@"
}
la alice init alice >/dev/null || die "init alice"
la bob init bob >/dev/null || die "init bob"
addr() { la "$1" whoami | sed -n 's/^address: *//p'; }
A_ADDR=$(addr alice)
B_ADDR=$(addr bob)
la alice contact add bob "$B_ADDR" >/dev/null || die "alice adds bob"
la bob contact add alice "$A_ADDR" >/dev/null || die "bob adds alice"

la alice daemon >"$T/alice.log" 2>&1 &
PIDS+=($!)
la bob daemon >"$T/bob.log" 2>&1 &
PIDS+=($!)
wait_for 30 grep -q "inbox: connected" "$T/alice.log" || die "alice daemon did not connect"
wait_for 30 grep -q "inbox: connected" "$T/bob.log" || die "bob daemon did not connect"

# inbox_has <machine> <agent> <text>: read the agent's inbox (waits up to 10 s) and look for text
inbox_has() { la "$1" inbox --as "$2" --wait 10 >"$T/inbox.out" 2>&1 && grep -q -- "$3" "$T/inbox.out"; }

# ---------- send ----------
out=$(la bob send alice/planner "hello from bob" --as worker 2>&1)
check "send: receipt queued ($out)" grep -q "alice/planner -> queued" <<<"$out"
check "send: message in alice/planner inbox" inbox_has alice planner "hello from bob"

# ---------- ask / inbox / reply ----------
la bob ask alice/planner "what is 2+2?" --as worker --timeout 30 >"$T/ask.out" 2>&1 &
ASK=$!
ask_id() {
  la alice inbox --as planner --wait 10 >"$T/ask_inbox.out" 2>&1 || return 1
  python3 -c 'import json,sys
for l in sys.stdin:
    m = json.loads(l)
    if m["kind"] == "ask": print(m["id"]); break' <"$T/ask_inbox.out"
}
ID=$(ask_id)
if [ -n "$ID" ] && la alice reply "$ID" "4" --as planner >/dev/null 2>&1 && wait $ASK && [ "$(cat "$T/ask.out")" = 4 ]; then
  ok "ask/inbox/reply: bob got the reply \"4\""
else
  fail "ask/inbox/reply round trip (id=$ID, ask output: $(cat "$T/ask.out"))"
fi

# ---------- per-agent rule refusal ----------
# Rules are checked in order; the first match decides. vault may only get sends; everything else stays open.
la alice rule add bob --to vault --allow send >/dev/null
la alice rule add bob --allow send,ask,reply,list >/dev/null
start=$(date +%s)
out=$(la bob ask alice/vault "open the vault" --as worker --timeout 30 2>&1)
took=$(($(date +%s) - start))
if grep -q "refused: rule: bob/worker may not ask vault" <<<"$out" && [ "$took" -lt 10 ]; then
  ok "rule refusal closes the ask in ${took}s"
else
  fail "rule refusal (took ${took}s, output: $out)"
fi

# ---------- group send ----------
la bob group set team alice/planner alice/ops >/dev/null
out=$(la bob send @team "standup at 10" --as worker 2>&1)
if [ "$(grep -c -- "-> queued" <<<"$out")" = 2 ]; then ok "group send: 2 receipts queued"; else fail "group send receipts ($out)"; fi
check "group send: alice/ops got it" inbox_has alice ops "standup at 10"
check "group send: alice/planner got it" inbox_has alice planner "standup at 10"

# ---------- replayed and tampered envelopes ----------
# Capture one real sealed envelope (bob's hello to alice) with a one-shot fake relay, then post it to the
# real relay ourselves: twice (replay) and once with a flipped ciphertext byte (tamper).
python3 - "$T/cap.port" "$T/env.bin" <<'PY' &
import http.server, sys
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        open(sys.argv[2], "wb").write(self.rfile.read(int(self.headers["content-length"])))
        self.send_response(202); self.end_headers()
    def log_message(self, *a): pass
srv = http.server.HTTPServer(("127.0.0.1", 0), H)
open(sys.argv[1], "w").write(str(srv.server_address[1]))
srv.handle_request()
PY
CAP=$!
wait_for 10 test -s "$T/cap.port" || die "capture server did not start"
LETAGENTS_HOME="$T/bob" LETAGENTS_RELAY="http://127.0.0.1:$(cat "$T/cap.port")" "$BIN" hello alice >/dev/null 2>&1
wait $CAP
[ -s "$T/env.bin" ] || die "no envelope captured"

A_ID=$(cut -d. -f2 <<<"$A_ADDR")
A_DROP=$(cut -d. -f4 <<<"$A_ADDR")
post() { curl -s -o /dev/null -w '%{http_code}' -X POST -H "x-la-drop: $A_DROP" -H "content-type: application/octet-stream" --data-binary "@$1" "$RELAY/v1/send/$A_ID"; }
seen_before=$(count "$T/alice.log" "hello from a known contact")
replays_before=$(count "$T/alice.log" "dropped replay")
c1=$(post "$T/env.bin")
c2=$(post "$T/env.bin")
# The same envelope must be processed at most once: at least one copy is dropped as a replay,
# and the hello is handled at most once (the CLI may already have delivered it through the daemon).
if [ "$c1$c2" = 202202 ] && wait_for 30 logged "$T/alice.log" "dropped replay" -ge 1 \
  && [ "$(count "$T/alice.log" "hello from a known contact")" -le $((seen_before + 1)) ]; then
  ok "replayed envelope dropped (processed at most once)"
else
  fail "replay (relay answered $c1 $c2)"; tail -n 20 "$T/alice.log" >&2
fi

python3 -c 'import sys; b = bytearray(open(sys.argv[1], "rb").read()); b[100] ^= 1; open(sys.argv[2], "wb").write(b)' "$T/env.bin" "$T/tampered.bin"
tampered_before=$(count "$T/alice.log" "dropped envelope: cannot decrypt")
c3=$(post "$T/tampered.bin")
if [ "$c3" = 202 ] && wait_for 10 logged "$T/alice.log" "dropped envelope: cannot decrypt" -gt "$tampered_before"; then
  ok "tampered envelope dropped (cannot decrypt)"
else
  fail "tamper (relay answered $c3; see $T/alice.log)"
fi

if [ "$FAILS" -eq 0 ]; then echo "e2e: all checks passed"; else echo "e2e: $FAILS check(s) failed"; fi
[ "$FAILS" -eq 0 ]
