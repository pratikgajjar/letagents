#!/usr/bin/env bash
# Interop test for the zero-install client (web/la.mjs) against the Rust daemon, through a local relay.
# alice runs the Rust daemon (no pi-intercom); zed runs only `node web/la.mjs`, with no daemon.
#
#   tests/zero.sh                    # builds cli/target/debug/letagents first
#   LETAGENTS_BIN=path tests/zero.sh # use a prebuilt binary
#   LA_RUNTIME=bun tests/zero.sh     # run la.mjs with another runtime (node, bun, "deno run -A")
#
# Needs: cargo, node (npx), python3, curl.
set -u
set -m

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
check() { local name=$1; shift; if "$@"; then ok "$name"; else fail "$name"; fi; }
die() { echo "FAIL $1"; FAILS=$((FAILS + 1)); for f in "$T"/*.log; do echo "--- $f"; tail -30 "$f"; done; exit 1; }
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
wait_for() {
  local n=$(($1 * 5)); shift
  for _ in $(seq "$n"); do "$@" >/dev/null 2>&1 && return 0; sleep 0.2; done
  return 1
}
count() { grep -c -- "$2" "$1" 2>/dev/null || true; }
json_field() { python3 -c 'import json,sys
for l in sys.stdin:
    try: m = json.loads(l)
    except ValueError: continue
    if m.get("kind") == sys.argv[1]: print(m[sys.argv[2]]); break' "$1" "$2"; }

BIN=${LETAGENTS_BIN:-}
if [ -z "$BIN" ]; then
  cargo build -q --manifest-path "$ROOT/cli/Cargo.toml" || die "cli build"
  BIN=$ROOT/cli/target/debug/letagents
fi

PORT=$(free_port)
RELAY=http://127.0.0.1:$PORT
(cd "$ROOT/relay" && exec npx --yes wrangler@latest dev --ip 127.0.0.1 --port "$PORT" --persist-to "$T/state" --show-interactive-dev-session=false >"$T/relay.log" 2>&1) &
PIDS+=($!)
wait_for 300 curl -fsS "$RELAY/health" || die "relay did not start on $RELAY"

mkdir -p "$T/pi"
la() { local m=$1; shift; LETAGENTS_HOME="$T/$m" PI_CODING_AGENT_DIR="$T/pi" LETAGENTS_RELAY="$RELAY" LETAGENTS_AUTH_HOST=letagents.talk "$BIN" "$@"; }
# zed: the same relay, but the client is la.mjs fed on stdin, as `curl ... | node -` would.
zed() { LETAGENTS_HOME="$T/zed" LETAGENTS_RELAY="$RELAY" LETAGENTS_AUTH_HOST=letagents.talk node - "$@" <"$ROOT/web/la.mjs"; }
if [ -n "${LA_RUNTIME:-}" ]; then zed() { LETAGENTS_HOME="$T/zed" LETAGENTS_RELAY="$RELAY" LETAGENTS_AUTH_HOST=letagents.talk $LA_RUNTIME "$ROOT/web/la.mjs" "$@"; }; fi

la alice init alice >/dev/null || die "init alice"
zed init zed >/dev/null || die "init zed (la.mjs)"
A_ADDR=$(la alice whoami | sed -n 's/^address: *//p')
Z_ADDR=$(zed whoami | sed -n 's/^address: *//p')
check "same address from la.mjs and the binary" test "$(la zed whoami | sed -n 's/^address: *//p')" = "$Z_ADDR"
la alice contact add zed "$Z_ADDR" >/dev/null || die "alice adds zed"
zed contact add alice "$A_ADDR" >/dev/null || die "zed adds alice (la.mjs)"

la alice daemon >"$T/alice.log" 2>&1 &
PIDS+=($!)
wait_for 30 grep -q "inbox: connected" "$T/alice.log" || die "alice daemon did not connect"

# ---------- la.mjs -> daemon: send ----------
zed send alice/planner "hello from zed" --as cloud >"$T/send.out" 2>&1
check "la.mjs send: accepted by the relay" grep -q "^sent " "$T/send.out"
la alice inbox --as planner --wait 10 >"$T/a_inbox.out" 2>&1
check "la.mjs send: in alice/planner inbox from zed/cloud" grep -q '"from":"zed/cloud".*hello from zed' "$T/a_inbox.out"

# ---------- daemon -> la.mjs: send; receipts from the daemon are tolerated ----------
la alice send zed/cloud "hello from alice" --as planner >/dev/null 2>&1
zed inbox --as cloud --wait 10 >"$T/z_inbox.out" 2>"$T/z_inbox.err"
check "daemon send: zed inbox shows it" grep -q '"from":"alice/planner".*hello from alice' "$T/z_inbox.out"
check "daemon receipt for zed's send tolerated" grep -q 'receipt from alice:.*queued' "$T/z_inbox.err"
check "zed sent a receipt the daemon logged" wait_for 10 grep -q "receipt from zed: pushed" "$T/alice.log"

# ---------- la.mjs ask -> daemon reply ----------
zed ask alice/planner "what is 2+2?" --as cloud --timeout 30 >"$T/ask.out" 2>&1 &
ASK=$!
la alice inbox --as planner --wait 10 >"$T/ask_inbox.out" 2>&1
ID=$(json_field ask id <"$T/ask_inbox.out")
if [ -n "$ID" ] && la alice reply "$ID" "4" --as planner >/dev/null 2>&1 && wait $ASK && [ "$(cat "$T/ask.out")" = 4 ]; then
  ok "la.mjs ask, daemon reply: zed got \"4\""
else
  fail "la.mjs ask round trip (id=$ID, output: $(cat "$T/ask.out"))"
fi

# ---------- daemon ask -> la.mjs reply ----------
la alice ask zed/cloud "what is 6*7?" --as planner --timeout 30 >"$T/ask2.out" 2>&1 &
ASK=$!
zed inbox --as cloud --wait 10 >"$T/ask2_inbox.out" 2>&1
ID=$(json_field ask id <"$T/ask2_inbox.out")
if [ -n "$ID" ] && zed reply "$ID" "42" --as cloud >/dev/null 2>&1 && wait $ASK && [ "$(cat "$T/ask2.out")" = 42 ]; then
  ok "daemon ask, la.mjs reply: alice got \"42\""
else
  fail "daemon ask round trip (id=$ID, output: $(cat "$T/ask2.out"))"
fi

# ---------- list both ways ----------
zed list alice >"$T/list.out" 2>&1
check "la.mjs list alice: roster has planner" grep -q "alice/planner" "$T/list.out"
la alice list zed --json >"$T/list2.out" 2>&1 &
LST=$!
sleep 1
zed inbox --as cloud >/dev/null 2>&1
wait $LST
check "daemon list zed: roster has zed/cloud (la.mjs)" grep -q '"zed/cloud"' "$T/list2.out"

# ---------- rule refusal reaches la.mjs ----------
la alice rule add zed --to vault --allow send >/dev/null
la alice rule add zed --allow send,ask,reply,list >/dev/null
out=$(zed ask alice/vault "open the vault" --as cloud --timeout 20 2>&1); code=$?
if [ "$code" = 3 ] && grep -q "refused: rule: zed/cloud may not ask vault" <<<"$out"; then ok "rule refusal ends a la.mjs ask"; else fail "rule refusal (exit $code: $out)"; fi

# ---------- tampered and replayed envelopes ----------
# capture <file> <client command...>: run it against a one-shot fake relay and save the envelope it posts.
capture() {
  local out=$1; shift
  rm -f "$T/cap.port"
  python3 - "$T/cap.port" "$out" <<'PY' &
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
  local cap=$!
  wait_for 10 test -s "$T/cap.port" || return 1
  RELAY="http://127.0.0.1:$(cat "$T/cap.port")" "$@" >/dev/null 2>&1
  wait $cap
  test -s "$out"
}
flip() { python3 -c 'import sys; b = bytearray(open(sys.argv[1], "rb").read()); b[100] ^= 1; open(sys.argv[2], "wb").write(b)' "$1" "$2"; }
post() { # post <address> <file>
  curl -s -o /dev/null -w '%{http_code}' -X POST -H "x-la-drop: $(cut -d. -f4 <<<"$1")" -H "content-type: application/octet-stream" --data-binary "@$2" "$RELAY/v1/send/$(cut -d. -f2 <<<"$1")"
}

# la.mjs-sealed envelope, tampered, into the daemon
capture "$T/z_env.bin" zed hello alice || die "no envelope captured from la.mjs"
flip "$T/z_env.bin" "$T/z_bad.bin"
before=$(count "$T/alice.log" "dropped envelope: cannot decrypt")
c=$(post "$A_ADDR" "$T/z_bad.bin")
check "daemon drops a tampered la.mjs envelope ($c)" wait_for 10 test "$(count "$T/alice.log" "dropped envelope: cannot decrypt")" -gt "$before"
before=$(count "$T/alice.log" "hello from a known contact")
c=$(post "$A_ADDR" "$T/z_env.bin")
check "daemon opens the untampered la.mjs envelope ($c)" wait_for 10 test "$(count "$T/alice.log" "hello from a known contact")" -gt "$before"

# daemon-sealed envelope, tampered and replayed, into la.mjs
capture "$T/a_env.bin" la alice hello zed || die "no envelope captured from the binary"
flip "$T/a_env.bin" "$T/a_bad.bin"
# One inbox read for all three (the relay allows 10 inbox logins per minute per IP): tampered, good, replay.
for f in a_bad a_env a_env; do post "$Z_ADDR" "$T/$f.bin" >/dev/null; done
zed inbox >/dev/null 2>"$T/tamper.err"
check "la.mjs drops a tampered daemon envelope" grep -q "dropped envelope: cannot decrypt" "$T/tamper.err"
check "la.mjs accepts the good copy once, drops the replay" test "$(count "$T/tamper.err" "dropped")" = 2 -a "$(count "$T/tamper.err" "dropped replay")" = 1

if [ "$FAILS" -eq 0 ]; then echo "zero: all checks passed"; else echo "zero: $FAILS check(s) failed"; fi
[ "$FAILS" -eq 0 ]
