#!/usr/bin/env bash
# Attack suite: real attacks against a local relay (wrangler dev), real daemons, a real pi-intercom broker and
# the real letagents binary. Each check is an attack that must fail (or a control that must work).
# The attacker's tools are in tests/attack.mjs. Prints one line per check; exits non-zero if any fails.
#
#   tests/attack.sh                  # local relay + daemons (builds cli/target/debug/letagents first)
#   tests/attack.sh --prod           # read-only probes of https://letagents.talk (a few GETs, no load)
#   LETAGENTS_BIN=path PI_INTERCOM_DIR=path tests/attack.sh
#
# Machines: alice (pi-intercom broker, sessions planner and vault), bob (alice's contact), carol (alice's
# contact; bob does not know her), dana (no daemon), ivy (an inviter), xg and yg (guests, each bound to one of
# alice's agents), mallory (the attacker: she knows
# alice's full address, so she can drop mail; she has no contact key). Needs: cargo, node 22+, python3, curl,
# worker-build, git.
set -u
set -m
# The harness this runs in must not choose the agent name (letagents takes it from these).
unset LETAGENTS_AS PI_INTERCOM_SESSION_ID CLAUDE_CODE_SESSION_ID CODEX_THREAD_ID CODEX_SESSION_ID LETAGENTS_EVENTS_CONFORMANCE LETAGENTS_EVENTS_ALLOW_PRIVATE

ROOT=$(cd "$(dirname "$0")/.." && pwd)
AT="$ROOT/tests/attack.mjs"
FAILS=0
PASSES=0
ok() { echo "ok   $1"; PASSES=$((PASSES + 1)); }
fail() { echo "FAIL $1"; FAILS=$((FAILS + 1)); }
check() { local name=$1; shift; if "$@"; then ok "$name"; else fail "$name"; fi; }
finish() { if [ "$FAILS" -eq 0 ]; then echo "attack: all $PASSES checks passed"; else echo "attack: $FAILS check(s) failed, $PASSES passed"; fi; [ "$FAILS" -eq 0 ]; }

# ---------- F. production, read-only ----------
if [ "${1:-}" = --prod ]; then
  P=https://letagents.talk
  hdr() { curl -sS -o /dev/null -D - -H 'Accept: text/html' "$1" | tr -d '\r' | tr 'A-Z' 'a-z'; }
  H=$(hdr "$P/")
  check "prod page: HSTS" grep -q '^strict-transport-security: max-age=' <<<"$H"
  check "prod page: CSP default-src 'none', frame-ancestors 'none'" bash -c "grep -q \"^content-security-policy: default-src 'none'.*frame-ancestors 'none'\" <<<\"\$0\"" "$H"
  check "prod page: nosniff, no-referrer" bash -c 'grep -q "^x-content-type-options: nosniff" <<<"$0" && grep -q "^referrer-policy: no-referrer" <<<"$0"' "$H"
  # Right after a deploy, an old worker can still answer for a few seconds: retry for up to a minute.
  for _ in 1 2 3 4 5 6; do
    code=$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' http://letagents.talk/install.sh)
    [ "$code" = "301 https://letagents.talk/install.sh" ] && break
    sleep 10
  done
  check "prod http://install.sh redirects to https ($code)" test "$code" = "301 https://letagents.talk/install.sh"
  code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST http://letagents.talk/v1/send/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA)
  check "prod http API refused ($code)" test "$code" = 403
  out=$(curl -sS -w ' %{http_code}' "$P/v1/inbox/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA?ts=$(($(date +%s) * 1000))&drop_hash=x&sig=y")
  check "prod unsigned inbox: 401 with a short error ($out)" grep -qx '{"error":"bad auth"} 401' <<<"$out"
  out=$(curl -sS -w ' %{http_code}' "$P/nope/../../etc/passwd")
  check "prod unknown path: JSON 404, nothing else ($out)" grep -qx '{"error":"not found"} 404' <<<"$out"
  S=$(curl -sS "$P/stats.json")
  check "prod stats.json: counts only (no address, id or IP)" python3 -c '
import json, re, sys
s = json.loads(sys.argv[1]); t = json.dumps(s)
assert not re.search(r"la1\.|[A-Za-z0-9_-]{43}|\b\d+\.\d+\.\d+\.\d+\b", t)' "$S"
  check "prod security.txt" bash -c "curl -fsS $P/.well-known/security.txt | grep -q '^Contact: '"
  finish
  exit
fi

# ---------- local setup ----------
T=$(mktemp -d)
PIDS=()
cleanup() {
  for p in "${PIDS[@]}"; do kill -- "-$p" 2>/dev/null || kill "$p" 2>/dev/null; done
  wait 2>/dev/null
  if [ "$FAILS" -ne 0 ] || [ -n "${KEEP:-}" ]; then echo "logs kept in $T"; else rm -rf "$T"; fi
}
trap cleanup EXIT
die() { echo "FAIL $1"; FAILS=$((FAILS + 1)); for f in "$T"/*.log; do echo "--- $f"; tail -20 "$f"; done; exit 1; }
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
wait_for() { local n=$(($1 * 5)); shift; for _ in $(seq "$n"); do "$@" >/dev/null 2>&1 && return 0; sleep 0.2; done; return 1; }
count() { grep -c -- "$2" "$1" 2>/dev/null || true; }
logged() { [ "$(count "$1" "$2")" "$3" "$4" ]; }
mark() { echo "ATK-$1-$RANDOM$RANDOM"; }

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
export LETAGENTS_RELAY=$RELAY LETAGENTS_AUTH_HOST=letagents.talk ATK_HOMES=$T

# The relay operator's view: alice's and bob's daemons talk to the relay through this proxy, which records
# every byte they send.
node "$AT" proxy "$T/proxy.port" "$PORT" "$T/wire.bin" &
PIDS+=($!)
wait_for 10 test -s "$T/proxy.port" || die "capture proxy did not start"
WIRE=http://127.0.0.1:$(cat "$T/proxy.port")

# A private pi-intercom broker (never the user's own).
PIC=${PI_INTERCOM_DIR:-}
if [ -z "$PIC" ]; then
  PIC="$T/pi-intercom"
  git init -q "$PIC" && git -C "$PIC" fetch -q --depth 1 https://github.com/pratikgajjar/pi-intercom.git 8f0750bb9c8be0506417a91271b2d8d70735fa29 \
    && git -C "$PIC" checkout -q FETCH_HEAD && (cd "$PIC" && npm ci --silent --no-audit --no-fund >/dev/null 2>&1) || die "pi-intercom checkout"
fi
mkdir -p "$T/pi" "$T/nopi"
(cd "$PIC" && PI_CODING_AGENT_DIR="$T/pi" exec node node_modules/tsx/dist/cli.mjs broker/broker.ts >"$T/broker.log" 2>&1) &
PIDS+=($!)
wait_for 20 test -S "$T/pi/intercom/broker.sock" || die "pi-intercom broker did not start"
BROKER="$T/pi/intercom/broker.sock"

# la <machine> <args>: alice uses the broker; alice and bob go through the capture proxy.
la() {
  local m=$1; shift
  local pi="$T/nopi" relay=$RELAY
  [ "$m" = alice ] && pi="$T/pi"
  case $m in alice | bob) relay=$WIRE ;; esac
  LETAGENTS_HOME="$T/$m" PI_CODING_AGENT_DIR="$pi" LETAGENTS_RELAY="$relay" "$BIN" "$@"
}
addr() { la "$1" whoami | sed -n 's/^address: *//p'; }
for m in alice bob carol dana ivy mallory; do la $m init $m >/dev/null || die "init $m"; done
A=$(addr alice); B=$(addr bob); C=$(addr carol); D=$(addr dana); M=$(addr mallory)
A_ID=$(cut -d. -f2 <<<"$A"); B_ID=$(cut -d. -f2 <<<"$B"); D_ID=$(cut -d. -f2 <<<"$D")
la alice contact add bobmach "$B" >/dev/null && la bob contact add alicemach "$A" >/dev/null || die "contacts"
la alice contact add carolmach "$C" >/dev/null && la carol contact add alicemach "$A" >/dev/null || die "contacts"
la dana check >/dev/null 2>&1 # opens dana's mailbox

# Two pi sessions on alice's broker; they print every message they get.
node "$AT" pi "$BROKER" planner 600 >"$T/pi-planner.out" 2>&1 &
PIDS+=($!)
node "$AT" pi "$BROKER" vault 600 >"$T/pi-vault.out" 2>&1 &
PIDS+=($!)

start_daemon() { la "$1" daemon >>"$T/$1.log" 2>&1 & LAST=$!; PIDS+=($LAST); }
start_daemon alice; ALICE=$LAST
start_daemon bob
start_daemon carol
start_daemon ivy
for m in alice bob carol ivy; do wait_for 30 grep -q "inbox: connected" "$T/$m.log" || die "$m daemon did not connect"; done
wait_for 15 grep -q "pi-intercom connected" "$T/alice.log" || die "alice did not join the broker"
sleep 2

# ---------- E. control: a contact's message reaches the named pi session ----------
SECRET=$(mark secret)
out=$(la bob send alicemach/planner "$SECRET" --as worker 2>&1)
check "control: bob -> alice/planner pushed ($out)" grep -q "pushed" <<<"$out"
check "control: the planner session got it" wait_for 10 grep -q "$SECRET" "$T/pi-planner.out"

# ---------- A. outsider with the relay URL (mallory also knows alice's full address) ----------
q=$(node "$AT" login mallory | sed "s#/v1/inbox/[^?]*#/v1/inbox/$A_ID#")
out=$(node "$AT" get "$q" ip=10.1.0.1)
check "A1 HTTPS poll of alice's mailbox with mallory's key: 401, no mail" bash -c 'head -1 <<<"$0" | grep -qx 401 && ! grep -q msgs <<<"$0"' "$out"
out=$(node "$AT" ws "$q" ip=10.1.0.2 secs=2)
check "A2 WSS login to alice's mailbox with mallory's key: refused, no frames" bash -c '! grep -q "^open\|\"t\"" <<<"$0"' "$out"

# A captured login (from the wire, as the relay or a TLS-breaking proxy would see it) cannot be used again.
# dana logs in once through the capture proxy:
LETAGENTS_HOME="$T/dana" LETAGENTS_RELAY=$WIRE "$BIN" check >/dev/null 2>&1
cap=$(grep -ao "/v1/inbox/$D_ID?[^ ]*" "$T/wire.bin" | tail -1)
out=$(node "$AT" get "$cap" ip=10.1.0.3 | head -1)$(node "$AT" get "$cap" ip=10.1.0.3 | tail -1)
check "A3 replayed HTTPS login (captured on the wire): 401 replayed ($out)" grep -q '^401{"error":"replayed"}' <<<"$out"
capw=$(grep -ao "/v1/inbox/$A_ID?[^ ]*" "$T/wire.bin" | head -1)
out=$(node "$AT" ws "$capw" ip=10.1.0.4 secs=2)
check "A3 replayed WSS login of alice's daemon: refused, no frames" bash -c '! grep -q "^open\|\"t\"" <<<"$0"' "$out"
now=$(($(date +%s) * 1000))
for skew in -180000 180000; do
  out=$(node "$AT" get "$(node "$AT" login dana ts=$((now + skew)))" ip=10.1.0.5 | tail -1)
  check "A4 login signed with ts ${skew} ms: stale ($out)" grep -q stale <<<"$out"
done
out=$(node "$AT" get "$(node "$AT" login dana host=evil.example)" ip=10.1.0.6 | tail -1)
check "A4 login signed for another host: bad signature ($out)" grep -q "bad signature" <<<"$out"

# A forged ack cannot delete mail; junk logins cannot lock the owner out.
node "$AT" seal mallory "$D" kind=send body=still-here >"$T/d.env"
node "$AT" post "$D" "$T/d.env" ip=10.1.0.7 >/dev/null
out=$(node "$AT" get "$(node "$AT" ack mallory 1,2,3,4,5 | sed "s#/v1/ack/[^?]*#/v1/ack/$D_ID#")" method=POST ip=10.1.0.8 | head -1)
check "A5 ack forged by mallory: 401 ($out)" test "$out" = 401
# (The owner's login comes from its own IP, as on the internet: in wrangler dev all local binaries share one.)
for i in $(seq 1 12); do curl -s -o /dev/null -H "cf-connecting-ip: 10.2.0.$i" "$RELAY/v1/inbox/$D_ID?ts=$now&drop_hash=x&sig=y"; done
out=$(node "$AT" get "$(node "$AT" login dana)" ip=10.2.1.1 | sed -n '1p;$p' | tr '\n' ' ')
check "A6 12 junk logins from 12 IPs do not lock dana out; the mail is still there (${out:0:40})" grep -q '^200 {.*"msgs":\[{' <<<"$out"

# Sender authenticity inside the envelope.
before=$(count "$T/alice.log" "bad signature")
X=$(mark forged)
node "$AT" seal mallory "$A" from="$(cut -d. -f1-3 <<<"$B")" kind=send to_agent=planner body="$X" >"$T/f.env"
node "$AT" post "$A" "$T/f.env" ip=10.1.1.1 >/dev/null
check "A7 mallory signs as bob: dropped (bad signature)" wait_for 10 logged "$T/alice.log" "bad signature" -gt "$before"
before=$(count "$T/alice.log" "bad signature")
node "$AT" seal carol "$A" from="$(cut -d. -f1-3 <<<"$B")" kind=send to_agent=planner body="$X" >"$T/f.env"
node "$AT" post "$A" "$T/f.env" ip=10.1.1.2 >/dev/null
check "A8 contact carol signs as contact bob: dropped (bad signature)" wait_for 10 logged "$T/alice.log" "bad signature" -gt "$before"
for kind in send ask list; do
  node "$AT" seal mallory "$A" kind=$kind to_agent=planner body="$X" >"$T/f.env"
  node "$AT" post "$A" "$T/f.env" ip=10.1.1.3 >/dev/null
done
check "A9 non-contact send/ask/list with her own key: refused (unknown sender)" wait_for 10 logged "$T/alice.log" "REFUSED list .*unknown sender" -ge 1
sleep 1
check "A9 nothing forged reached a pi session" bash -c "! grep -q '$X' '$T/pi-planner.out' '$T/pi-vault.out'"
la alice inbox --wait 1 >"$T/machine.out" 2>&1
check "A9 nothing forged in alice's machine inbox" bash -c "! grep -q '$X' '$T/machine.out'"

# Envelope limits and the drop secret.
head -c 2048 /dev/urandom | { printf LA1; cat; } >"$T/ok.env"
head -c 40000 /dev/urandom | { printf LA1; cat; } >"$T/big.env"
sendc() { curl -s -o /dev/null -w '%{http_code}' -X POST -H "cf-connecting-ip: 10.1.2.$1" "${@:2}"; }
DROP=$(cut -d. -f4 <<<"$D")
check "A10 send without the drop secret: 404" test "$(sendc 1 --data-binary @"$T/ok.env" "$RELAY/v1/send/$D_ID")" = 404
check "A10 send with a wrong drop secret: 404" test "$(sendc 2 -H 'x-la-drop: AAAAAAAAAAAAAAAAAAAAAA' --data-binary @"$T/ok.env" "$RELAY/v1/send/$D_ID")" = 404
check "A10 40 KB envelope: 413" test "$(sendc 3 -H "x-la-drop: $DROP" --data-binary @"$T/big.env" "$RELAY/v1/send/$D_ID")" = 413
check "A10 40 KB envelope, chunked (no length): 413" test "$(sendc 4 -H "x-la-drop: $DROP" -H 'Transfer-Encoding: chunked' --data-binary @"$T/big.env" "$RELAY/v1/send/$D_ID")" = 413
check "A10 not an envelope: 400" test "$(sendc 5 -H "x-la-drop: $DROP" --data-binary 'hello world, plain text' "$RELAY/v1/send/$D_ID")" = 400
check "A10 mailbox that does not exist: 404" test "$(sendc 6 -H "x-la-drop: $DROP" --data-binary @"$T/ok.env" "$RELAY/v1/send/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB")" = 404

# Path tricks on the static and API routes: no file outside the fixed list, no 5xx, no stack traces.
bad=""
for p in "/../../etc/passwd" "/%2e%2e/%2e%2e/etc/passwd" "/agents.md/" "/agents.md/../identity.json" "/v1/send/$D_ID/x" "/v1/inbox/..%2F$D_ID" \
  "/v1/inbox/$D_ID%2F..%2Fsend" "/v1/ack/$D_ID/../../inbox" "//v1//inbox//$D_ID" "/build/index.js" "/wrangler.toml" "/.env" "/v1/inbox/$D_ID?ts=1&sig=%00"; do
  r=$(curl -s --path-as-is -w ' %{http_code}' -H 'cf-connecting-ip: 10.1.3.1' "$RELAY$p")
  code=${r##* }; body=${r% *}
  if [ "$code" -ge 500 ] || [ "$code" = 200 ] || grep -qi "panicked\|stack\|at .*\.rs" <<<"$body"; then bad="$bad $p=$code"; fi
done
check "A11 path tricks: 4xx only${bad:+ (got$bad)}" test -z "$bad"
S=$(curl -sS "$RELAY/stats.json")
check "A12 stats.json: counts only, no ids, addresses or IPs" python3 -c '
import json, re, sys
s = json.loads(sys.argv[1]); t = json.dumps(s)
assert not re.search(r"[A-Za-z0-9_-]{43}|\b\d+\.\d+\.\d+\.\d+\b", t), t
for bad in sys.argv[2:]: assert bad not in t, bad' "$S" "$A_ID" "$B_ID" "$D_ID" "la1." "127.0.0.1"
r=$(curl -s -o /dev/null -w '%{http_code} %{redirect_url}' -H 'x-forwarded-proto: http' "$RELAY/install.sh")
check "A13 plain HTTP page: 301 to https ($r)" grep -q "^301 " <<<"$r"
r=$(curl -s -w ' %{http_code}' -X POST -H 'x-forwarded-proto: http' -H "x-la-drop: $DROP" --data-binary @"$T/ok.env" "$RELAY/v1/send/$D_ID")
check "A13 plain HTTP API: refused before the mailbox ($r)" grep -q '"use https"} 403$' <<<"$r"

# ---------- E. contact admission and the pi-intercom adapter ----------
la alice rule add bobmach --to vault --allow list >/dev/null
la alice rule add bobmach --allow send,ask,reply,list >/dev/null
X=$(mark rule)
out=$(la bob send alicemach/vault "$X" --as worker 2>&1)
check "E1 rule denies bob -> vault: refused ($out)" grep -q "refused rule: bobmach/worker may not send vault" <<<"$out"
check "E1 the vault session got nothing" bash -c "sleep 1; ! grep -q '$X' '$T/pi-vault.out'"
# Confused deputy: bob names one of alice's own intercom sessions (her contact carol, bob himself, the daemon,
# pi-intercom routing). Before the fix, alice's machine forwarded the text to carol, signed as alice.
X=$(mark deputy)
for t in carolmach bobmach letagents-daemon "x@elsewhere"; do
  out=$(la bob send "alicemach/$t" "$X" --as worker 2>&1)
  check "E2 bob -> alice/$t: refused ($out)" grep -q "refused not an agent on this machine" <<<"$out"
done
sleep 2
la carol inbox --wait 2 >"$T/carol-inbox.out" 2>&1
check "E2 carol got nothing through alice" bash -c "! grep -q '$X' '$T/carol-inbox.out' && ! grep -q 'in send .*-> carol' '$T/carol.log'"
check "E2 alice forwarded nothing" bash -c "! grep -q 'out send -> carolmach (from intercom)\|out send -> bobmach (from intercom)' '$T/alice.log'"
# A roster with 300 agents must not become 300 sessions on alice's broker.
python3 -c 'import json; print(json.dumps([{"name": f"a{i}", "harness": "x\u001b[2J", "status": "idle"} for i in range(300)]))' >"$T/roster.json"
node "$AT" seal bob "$A" kind=roster body="$(cat "$T/roster.json")" >"$T/r.env"
node "$AT" post "$A" "$T/r.env" ip=10.1.4.1 >/dev/null
wait_for 15 grep -q "roster from bobmach: a0" "$T/alice.log"
sleep 3
n=$(node "$AT" pi "$BROKER" lister 1 list | sed -n 's/^sessions //p' | python3 -c 'import json,sys; print(sum(n.startswith("bobmach/") for n in json.load(sys.stdin)))')
check "E3 a 300-agent roster makes at most 64 sessions ($n)" test "${n:-999}" -le 64

# ---------- B. the relay operator ----------
sleep 1
check "B1 wire: no message text" bash -c "! grep -aq '$SECRET' '$T/wire.bin'"
check "B1 wire: no agent, contact or rule names" bash -c "! grep -aqE 'planner|worker|alicemach|bobmach|carolmach|vault' '$T/wire.bin'"
check "B1 wire: a mailbox id only in the URL path (its own login, or as recipient), never in a header or body" python3 -c '
import re, sys
w = open(sys.argv[1], "rb").read()
for i in sys.argv[2:]:
    pos = [m.start() for m in re.finditer(re.escape(i.encode()), w)]
    assert pos and all(w[p-10:p] in (b"/v1/inbox/", b"/v1/drops/") or w[p-8:p] in (b"/v1/ack/", b"/v1/send/") or w[p-9:p] == b"/v1/send/" for p in pos), i' "$T/wire.bin" "$B_ID" "$A_ID"
check "B2 relay log: no message text" bash -c "! grep -q '$SECRET' '$T/relay.log'"
check "B3 relay storage: no message text anywhere" bash -c "! grep -raq '$SECRET' '$T/state'"
check "B3 relay storage: per mailbox only drop-code hashes, times and counters; no mailbox id, no names" python3 -c '
import glob, re, sqlite3, sys
dbs = [f for f in glob.glob(sys.argv[1] + "/v3/do/letagents-Mailbox/*.sqlite") if not f.endswith("metadata.sqlite")]
assert dbs
for f in dbs:
    db = sqlite3.connect(f)
    keys = {k for (k,) in db.execute("SELECT k FROM meta")}
    assert keys <= {"drop_hash", "last_auth_ts", "last_seen", "live_h", "counted", "drops_ts"}, keys
    drops = db.execute("SELECT h, exp FROM drops").fetchall() if db.execute("SELECT 1 FROM sqlite_master WHERE name = \"drops\"").fetchone() else []
    assert len(drops) <= 64 and all(re.fullmatch(r"[A-Za-z0-9_-]{43}", h) and isinstance(e, int) for h, e in drops), drops
    rows = repr(db.execute("SELECT * FROM meta").fetchall()).encode() + repr(drops).encode() + b"".join(ct for (ct,) in db.execute("SELECT ct FROM msgs"))
    for i in sys.argv[2:]: assert i.encode() not in rows, (f, i)' "$T/state" "$A_ID" "$B_ID" "$D_ID" "$(cut -d. -f2 <<<"$C")"
# A malicious relay replays everything it carried to alice, after her daemon restarted.
python3 - "$T/wire.bin" "$A_ID" "$T/replay" <<'PY'
import os, re, sys
w = open(sys.argv[1], "rb").read(); os.makedirs(sys.argv[3], exist_ok=True); n = 0
for m in re.finditer(rb"POST /v1/send/" + re.escape(sys.argv[2].encode()) + rb" HTTP/1.1\r\n(.*?)\r\n\r\n", w, re.S):
    h = dict(l.split(b": ", 1) for l in m.group(1).split(b"\r\n") if b": " in l)
    h = {k.lower(): v for k, v in h.items()}
    body = w[m.end():m.end() + int(h[b"content-length"])]
    if body[:3] == b"LA1":
        open(f"{sys.argv[3]}/{n}.bin", "wb").write(body); n += 1
print(n)
PY
kill "$ALICE" 2>/dev/null; wait "$ALICE" 2>/dev/null
start_daemon alice; ALICE=$LAST
wait_for 30 logged "$T/alice.log" "inbox: connected" -ge 2 || die "alice did not reconnect"
sleep 2
nrep=$(ls "$T/replay" | wc -l | tr -d ' ')
[ "$nrep" -gt 0 ] || die "no envelope to alice captured on the wire"
before=$(count "$T/alice.log" "dropped replay")
for f in "$T"/replay/*.bin; do curl -s -o /dev/null -X POST -H "cf-connecting-ip: 10.1.5.1" -H "x-la-drop: $(cut -d. -f4 <<<"$A")" --data-binary @"$f" "$RELAY/v1/send/$A_ID"; done
check "B4 $nrep replayed envelopes after a restart: all dropped as replays" wait_for 20 logged "$T/alice.log" "dropped replay" -ge $((before + nrep))
check "B4 the planner session got the secret once only" test "$(grep -c "$SECRET" "$T/pi-planner.out")" = 1
before=$(count "$T/carol.log" "cannot decrypt")
curl -s -o /dev/null -X POST -H "x-la-drop: $(cut -d. -f4 <<<"$C")" --data-binary @"$T/replay/0.bin" "$RELAY/v1/send/$(cut -d. -f2 <<<"$C")"
check "B5 envelope for alice moved into carol's mailbox: cannot decrypt" wait_for 10 logged "$T/carol.log" "cannot decrypt" -gt "$before"
before=$(count "$T/alice.log" "stale message")
node "$AT" seal bob "$A" kind=send to_agent=planner body=late ts_offset_ms=-176400000 >"$T/old.env"
node "$AT" post "$A" "$T/old.env" ip=10.1.5.2 >/dev/null
check "B6 envelope held back 49 h by the relay: dropped (stale)" wait_for 10 logged "$T/alice.log" "stale message" -gt "$before"
python3 -c 'import sys; b = bytearray(open(sys.argv[1], "rb").read()); b[60] ^= 1; open(sys.argv[2], "wb").write(b)' "$T/replay/0.bin" "$T/tampered.bin"
before=$(count "$T/alice.log" "cannot decrypt")
curl -s -o /dev/null -X POST -H "x-la-drop: $(cut -d. -f4 <<<"$A")" --data-binary @"$T/tampered.bin" "$RELAY/v1/send/$A_ID"
check "B7 one flipped byte: cannot decrypt" wait_for 10 logged "$T/alice.log" "cannot decrypt" -gt "$before"

# ---------- C. invites ----------
inv() { la ivy invite --no-service 2>/dev/null | sed -n 's/^address: *//p'; }
contacts() { la ivy contact ls | wc -l | tr -d ' '; }
I=$(inv); I4=$(cut -d. -f1-4 <<<"$I"); CODE=$(cut -d. -f5 <<<"$I")
files=()
for i in $(seq 1 12); do
  la "r$i" init "r$i" >/dev/null
  node "$AT" seal "$T/r$i" "$I4" kind=hello full=1 body="r$i" invite="$CODE" >"$T/r$i.env"; files+=("$T/r$i.env")
done
node "$AT" race "$I4" "${files[@]}" >/dev/null
wait_for 20 logged "$T/ivy.log" "contact request" -ge 11
check "C1 12 joins race one invite: exactly one is added ($(contacts) contacts)" test "$(contacts)" = 1
la dave join ivy "$I" --me dave --no-service >/dev/null 2>&1
check "C2 a spent invite: only a contact request" bash -c "sleep 2; grep -q 'contact request .* from \"dave\"' '$T/ivy.log' && [ \$(LETAGENTS_HOME='$T/ivy' '$BIN' contact ls | wc -l) = 1 ]"
I=$(inv)
python3 -c 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); json.dump({k: 1 for k in d}, open(p, "w"))' "$T/ivy/invites.json"
la erin join ivy "$I" --me erin --no-service >/dev/null 2>&1
check "C3 an expired invite: only a contact request" bash -c "sleep 2; grep -q 'contact request .* from \"erin\"' '$T/ivy.log' && [ \$(LETAGENTS_HOME='$T/ivy' '$BIN' contact ls | wc -l) = 1 ]"
I=$(inv); I4=$(cut -d. -f1-4 <<<"$I")
for i in $(seq 1 10); do
  node "$AT" seal mallory "$I4" kind=hello full=1 body=guess invite="$(head -c 16 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=')" | node "$AT" post "$I4" - ip=10.1.6.1 >/dev/null
done
check "C4 10 guessed codes: none added" bash -c "sleep 3; [ \$(LETAGENTS_HOME='$T/ivy' '$BIN' contact ls | wc -l) = 1 ]"
# Tampered invites: each field changed in transit.
IFS=. read -r _ isig ibox idrop icode <<<"$I"
MSIG=$(cut -d. -f2 <<<"$M"); MBOX=$(cut -d. -f3 <<<"$M")
out=$(la t1 join ivy "la1.$isig.$ibox.AAAAAAAAAAAAAAAAAAAAAA.$icode" --me t1 --no-service 2>&1)
check "C5 drop changed: the relay refuses (404)" grep -q "404" <<<"$out"
out=$(la t2 join ivy "la1.$MSIG.$ibox.$idrop.$icode" --me t2 --no-service 2>&1)
check "C5 sig changed: the relay refuses (404)" grep -q "404" <<<"$out"
before=$(count "$T/ivy.log" "cannot decrypt")
la t3 join ivy "la1.$isig.$MBOX.$idrop.$icode" --me t3 --no-service >/dev/null 2>&1
check "C5 box changed: ivy cannot decrypt it (and mallory's box gets nothing)" wait_for 10 logged "$T/ivy.log" "cannot decrypt" -gt "$before"
last=${icode: -1}; [ "$last" = A ] && new=B || new=A
la t4 join ivy "la1.$isig.$ibox.$idrop.${icode%?}$new" --me t4 --no-service >/dev/null 2>&1
check "C5 code changed: only a contact request" wait_for 10 grep -q 'contact request .* from "t4"' "$T/ivy.log"
# An intercepted invite: the thief joins first. One contact is added, ivy's human is told, and the real
# joiner's hello becomes a request (so the theft shows).
I=$(inv)
la thief join ivy "$I" --me thief --no-service >/dev/null 2>&1
wait_for 10 grep -q "added contact thief" "$T/ivy.log"
la real join ivy "$I" --me real --no-service >/dev/null 2>&1
check "C6 intercepted invite: exactly one more contact (the thief)" bash -c "sleep 2; [ \$(LETAGENTS_HOME='$T/ivy' '$BIN' contact ls | wc -l) = 2 ]"
la ivy inbox --wait 1 >"$T/ivy-inbox.out" 2>&1
check "C6 ivy's human is told: notice names the thief" grep -q "added contact thief" "$T/ivy-inbox.out"
check "C6 the real joiner is a visible contact request" grep -q 'contact request .* from "real"' "$T/ivy.log"
# Request hijack: mallory picks a message id that starts with the real joiner's request id.
RID=$(la ivy requests | awk '/"real"/ {print $1}')
REAL_SAFETY=$(la ivy requests | awk '/"real"/ {$1=""; $2=""; print}' | xargs)
node "$AT" seal mallory "$(cut -d. -f1-4 <<<"$I")" kind=hello full=1 body=real id="${RID}-0000-4000-8000-000000000000" | node "$AT" post "$(cut -d. -f1-4 <<<"$I")" - ip=10.1.6.2 >/dev/null
wait_for 10 logged "$T/ivy.log" 'contact request .* from "real"' -ge 2
check "C7 mallory cannot replace request $RID (still the real joiner's safety number)" bash -c "LETAGENTS_HOME='$T/ivy' '$BIN' requests | grep '^$RID ' | grep -q '$REAL_SAFETY'"

# ---------- D. the local machine ----------
mkdir -m 755 "$T/eve" && touch "$T/eve/daemon.log" && chmod 644 "$T/eve/daemon.log"
la eve init eve >/dev/null
start_daemon eve
wait_for 20 test -S "$T/eve/daemon.sock"
mode() { stat -c '%a' "$1" 2>/dev/null || stat -f '%Lp' "$1"; } # GNU, then BSD (on Linux, -f is file-system stat)
check "D1 a home made 0755 beforehand is closed to 0700 ($(mode "$T/eve"))" test "$(mode "$T/eve")" = 700
check "D1 daemon.log made 0644 beforehand is 0600 ($(mode "$T/eve/daemon.log"))" test "$(mode "$T/eve/daemon.log")" = 600
bad=""
for f in "$T"/alice/* "$T"/ivy/*; do m=$(mode "$f"); [ "$m" = 600 ] || bad="$bad $(basename "$f")=$m"; done
check "D1 every file in ~/.letagents is 0600, daemon.sock too${bad:+ (got$bad)}" test -z "$bad"
keys=$(python3 -c 'import json,sys; [print(json.load(open(f))[k]) for f in sys.argv[1:] for k in ("sig","rcv")]' "$T"/{alice,bob,carol,ivy}/identity.json)
leak=""
for k in $keys "$SECRET"; do grep -rqa -- "$k" "$T"/*.log && leak="$leak ${k:0:6}…"; done
check "D2 logs hold no message text and no private key${leak:+ (found$leak)}" test -z "$leak"
cat >"$T/mcp.in" <<'EOF'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"attacker","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"events_conformance_allow_origin","arguments":{"url":"http://127.0.0.1:9"}}}
{"jsonrpc":"2.0","id":4,"method":"events/list"}
{"jsonrpc":"2.0","id":5,"method":"events/subscribe","params":{"name":"message.received","delivery":{"mode":"webhook","url":"http://127.0.0.1:9/x","secret":"whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw"}}}
{"jsonrpc":"2.0","id":6,"method":"events/subscribe","params":{"name":"message.received","delivery":{"mode":"webhook","url":"https://127.0.0.1/x","secret":"whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw"}}}
{"jsonrpc":"2.0","id":7,"method":"events/subscribe","params":{"name":"message.received","delivery":{"mode":"webhook","url":"https://169.254.169.254/latest/meta-data","secret":"whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw"}}}
{"jsonrpc":"2.0","id":8,"method":"events/subscribe","params":{"name":"message.received","delivery":{"mode":"webhook","url":"https://localhost./x","secret":"whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw"}}}
EOF
env -u LETAGENTS_EVENTS_CONFORMANCE -u LETAGENTS_EVENTS_ALLOW_PRIVATE LETAGENTS_HOME="$T/alice" PI_CODING_AGENT_DIR="$T/pi" node "$AT" mcp "$BIN" "$T/mcp.in" >"$T/mcp.out" 2>&1
check "D3 MCP: conformance test hooks are off in a normal run" python3 -c '
import json, sys
r = {m.get("id"): m for m in map(json.loads, filter(str.strip, open(sys.argv[1])))}
assert not any("conformance" in t["name"] for t in r[2]["result"]["tools"])
assert r[3]["result"]["isError"]
assert [e["name"] for e in r[4]["result"]["events"]] == ["message.received"]' "$T/mcp.out"
check "D3 MCP webhooks: no plain http, loopback, cloud metadata or localhost" python3 -c '
import json, sys
r = {m.get("id"): m for m in map(json.loads, filter(str.strip, open(sys.argv[1])))}
assert all("error" in r[i] for i in (5, 6, 7, 8)), [r[i] for i in (5, 6, 7, 8)]' "$T/mcp.out"

# ---------- G. per-contact drop codes ----------
# Each of alice's contacts writes to her with its own drop code. Removing carol deletes her code at the relay.
field() { python3 -c 'import json,sys; c=json.load(open(sys.argv[1])).get(sys.argv[2], {}); print(c.get(sys.argv[3], "") if sys.argv[3] != "code" else c["address"].split(".")[3])' "$T/$1/contacts.json" "$2" "$3"; }
moved() { [ "$(field "$1" "$2" our_drop_ok)" = True ]; }
check "G0 bob and carol confirmed their own drop codes with alice (both ways)" wait_for 30 bash -c "$(declare -f field moved); T='$T'; moved alice bobmach && moved alice carolmach && moved bob alicemach && moved carol alicemach"
SHARED=$(cut -d. -f4 <<<"$A")
CCODE=$(field carol alicemach code); BCODE=$(field bob alicemach code)
check "G1 carol and bob write with codes of their own, not the shared one" test -n "$CCODE" -a "$CCODE" != "$SHARED" -a "$CCODE" != "$BCODE" -a "$BCODE" != "$SHARED"
# Impersonation with another contact's code: the code lets bob drop mail, it does not make him carol.
X=$(mark impersonate)
node "$AT" seal bob "$A" kind=send to_agent=planner from_agent=worker body="$X" >"$T/g.env"
out=$(node "$AT" post "$A" "$T/g.env" drop="$CCODE" ip=10.1.7.1)
check "G2 bob drops mail with carol's code (relay: $out); alice's planner gets it as bob's" wait_for 10 bash -c "grep '$X' '$T/pi-planner.out' | grep -q 'verified bobmach/worker' && ! grep '$X' '$T/pi-planner.out' | grep -q carolmach"
before=$(count "$T/alice.log" "bad signature")
node "$AT" seal bob "$A" from="$(cut -d. -f1-3 <<<"$C")" kind=send to_agent=planner body="$X-2" >"$T/g.env"
node "$AT" post "$A" "$T/g.env" drop="$CCODE" ip=10.1.7.2 >/dev/null
check "G2 bob signs as carol and uses carol's code: dropped (bad signature)" wait_for 10 logged "$T/alice.log" "bad signature" -gt "$before"
A_CAROL=$(field alice carolmach code); A_BOB=$(field alice bobmach code)
node "$AT" seal bob "$A" kind=receipt reply_to=x drop="$(head -c 16 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=')" body='{"status":"queued"}' >"$T/g.env"
node "$AT" post "$A" "$T/g.env" ip=10.1.7.3 >/dev/null
check "G2 bob's drop field changes only how alice reaches bob, never carol" wait_for 10 bash -c "$(declare -f field); T='$T'; [ \"\$(field alice bobmach code)\" != '$A_BOB' ] && [ \"\$(field alice carolmach code)\" = '$A_CAROL' ]"
node "$AT" seal bob "$A" kind=receipt reply_to=x drop="$(field bob alicemach our_drop)" body='{"status":"queued"}' >"$T/g.env"
node "$AT" post "$A" "$T/g.env" ip=10.1.7.4 >/dev/null
wait_for 10 bash -c "$(declare -f field); T='$T'; [ \"\$(field alice bobmach code)\" = '$(field bob alicemach our_drop)' ]" || die "bob's code at alice not restored"
# alice removes carol.
out=$(la alice remove carolmach 2>&1)
check "G3 alice removes carol: the relay table is updated ($out)" grep -q "refuses its drop code" <<<"$out"
check "G3 every remaining contact has its own code, so the shared code was replaced" test "$(addr alice | cut -d. -f4)" != "$SHARED"
node "$AT" seal carol "$A" kind=send to_agent=planner body="$X-3" >"$T/g.env"
out=$(node "$AT" post "$A" "$T/g.env" drop="$CCODE" ip=10.1.7.5)
check "G3 removed carol's send with her own code: refused at the relay, nothing stored ($out)" grep -q '^404 ' <<<"$out"
out=$(node "$AT" post "$A" "$T/g.env" drop="$SHARED" ip=10.1.7.6)
check "G3 removed carol's send with the old shared code: refused at the relay ($out)" grep -q '^404 ' <<<"$out"
out=$(la carol send alicemach/planner "$X-4" --as worker 2>&1)
check "G3 carol's own client: the relay refuses it ($out)" grep -q "404" <<<"$out"
# (The planner session is the proof; the receipt may take over 5 s on a slow runner and then says "sent".)
X=$(mark after)
out=$(la bob send alicemach/planner "$X" --as worker 2>&1)
check "G3 control: bob still reaches alice's planner ($out)" wait_for 15 grep -q "$X" "$T/pi-planner.out"
# A relay operator (or anyone with the wire) replays alice's first table, which still held carol's code.
python3 - "$T/wire.bin" "$A_ID" "$T/oldtable" <<'PY'
import re, sys
w = open(sys.argv[1], "rb").read()
m = re.search(rb"POST (/v1/drops/" + re.escape(sys.argv[2].encode()) + rb"\?[^ ]*) HTTP/1.1\r\n(.*?)\r\n\r\n", w, re.S)
h = {k.lower(): v for k, v in (l.split(b": ", 1) for l in m.group(2).split(b"\r\n") if b": " in l)}
open(sys.argv[3] + ".path", "wb").write(m.group(1)); open(sys.argv[3] + ".body", "wb").write(w[m.end():m.end() + int(h[b"content-length"])])
PY
out=$(curl -s -w ' %{http_code}' -X POST -H 'cf-connecting-ip: 10.1.7.7' -H 'content-type: application/json' --data-binary @"$T/oldtable.body" "$RELAY$(cat "$T/oldtable.path")")
check "G4 alice's old table (with carol's code) replayed from the wire: refused ($out)" grep -q '401$' <<<"$out"
out=$(node "$AT" post "$A" "$T/g.env" drop="$CCODE" ip=10.1.7.8)
check "G4 carol's code is still refused after the replay ($out)" grep -q '^404 ' <<<"$out"
# The table is bounded and only the mailbox key can change it.
out=$(node "$AT" drops dana n=65 ip=10.1.7.9)
check "G5 65 drop codes for one mailbox: refused ($out)" grep -q '^400 .*too many' <<<"$out"
out=$(node "$AT" drops dana n=64 ip=10.1.7.10)
check "G5 64 drop codes: accepted ($out)" grep -q '^200 .*"drops":64' <<<"$out"
out=$(node "$AT" drops dana n=1 sign_with=mallory ip=10.1.7.11)
check "G5 mallory replaces dana's table: bad signature ($out)" grep -q '^401 .*bad signature' <<<"$out"
ts=$(($(date +%s) * 1000 - 60000))
out=$(node "$AT" drops dana n=1 ts=$ts ip=10.1.7.12)
check "G5 a table update older than the last one: replayed ($out)" grep -q '^401 .*replayed' <<<"$out"

# ---------- H. a guest bound to one local agent ----------
# xg is an external person's machine. alice joins xg's invite with --agent guide: xg may reach only alice's
# guide, and guide may write only to xg. Paths: daemon.sock (CLI), `letagents mcp`, the pi-intercom adapter.
la xg init xg >/dev/null || die "init xg"
start_daemon xg
wait_for 30 grep -q "inbox: connected" "$T/xg.log" || die "xg daemon did not connect"
I=$(la xg invite --no-service 2>/dev/null | sed -n 's/^address: *//p')
la alice join xg "$I" --agent guide --note "external engineer" --no-service >/dev/null 2>&1
check "H1 join --agent guide --note: bound in one step" python3 -c '
import json, sys
c = json.load(open(sys.argv[1]))["xg"]
assert c["bind"] == {"agent": "guide"}, c
assert c["note"] == "external engineer", c
assert c["rules"] == [{"from": "*", "to": "guide", "allow": ["send", "ask", "reply"]}, {"from": "*", "to": "*", "allow": []}], c["rules"]' "$T/alice/contacts.json"
wait_for 20 bash -c "LETAGENTS_HOME='$T/xg' '$BIN' contact ls | grep -q '^alice '" || die "xg did not add alice"
node "$AT" watch "$T/alice/daemon.sock" guide 600 >"$T/guide.out" 2>&1 &
PIDS+=($!)
sleep 1
X=$(mark guest)
out=$(la xg send alice/guide "$X" --as eng 2>&1)
check "H2 xg -> alice/guide: pushed ($out)" grep -q "pushed" <<<"$out"
check "H2 guide got it, labelled with the note" wait_for 10 grep -qF "verified xg/eng · note: external engineer]" "$T/guide.out"
for t in planner vault; do
  out=$(la xg send "alice/$t" "$X-$t" --as eng 2>&1)
  check "H3 xg -> alice/$t: refused ($out)" grep -q "refused rule: xg/eng may not send $t" <<<"$out"
done
check "H3 planner and vault got nothing from xg" bash -c "sleep 1; ! grep -q '$X' '$T/pi-planner.out' '$T/pi-vault.out'"
out=$(la xg list alice 2>&1)
check "H4 xg lists alice: refused, no agent names" bash -c "! grep -q 'alice/' <<<\"\$0\" && grep -q 'REFUSED list .*rule: xg/agent may not list' '$T/alice.log'" "$out"
check "H4 xg never got alice's roster (no presence either)" bash -c "! grep -q 'roster from alice' '$T/xg.log'"
# guide answers xg's ask: the reply path is open for the bound pair.
Q=$(mark q)
la xg ask alice/guide "$Q" --as eng --timeout 30 >"$T/h-ask.out" 2>&1 &
ASKP=$!
wait_for 15 grep -q "$Q" "$T/guide.out"
QID=$(python3 -c 'import json,sys; print(next(json.loads(l)["push"]["id"] for l in open(sys.argv[1]) if sys.argv[2] in l))' "$T/guide.out" "$Q" 2>/dev/null)
la alice reply "$QID" "ANS-$Q" --as guide >/dev/null 2>&1
wait "$ASKP"
check "H5 guide replies to xg's ask: xg gets the answer" grep -q "ANS-$Q" "$T/h-ask.out"
out=$(la alice send xg/eng "$(mark toguest)" --as guide 2>&1)
check "H6 guide -> xg/eng: sent ($out)" bash -c '! grep -qi "refused\|error" <<<"$0" && grep -q "queued\|pushed" <<<"$0"' "$out"
X=$(mark bound)
out=$(la alice send bobmach/worker "$X" --as guide 2>&1)
check "H7 guide -> bobmach/worker (another contact): refused ($out)" grep -q "guide is bound to contact xg: it may write only to xg/<agent>, not to contact bobmach" <<<"$out"
for t in planner alice/planner; do
  out=$(la alice send "$t" "$X" --as guide 2>&1)
  check "H8 guide -> $t (local): refused ($out)" grep -q "guide is bound to contact xg: .*not to agents on this machine" <<<"$out"
done
cat >"$T/mcp-guide.in" <<EOF
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"guest-guide","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"letagents_list","arguments":{}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"letagents_send","arguments":{"to":"bobmach/worker","text":"$X"}}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"letagents_send","arguments":{"to":"planner","text":"$X"}}}
EOF
LETAGENTS_HOME="$T/alice" PI_CODING_AGENT_DIR="$T/pi" node "$AT" mcp "$BIN" "$T/mcp-guide.in" guide >"$T/mcp-guide.out" 2>&1
check "H9 MCP as guide: sends to bobmach and to a local agent are refused" python3 -c '
import json, sys
r = {m.get("id"): m for m in map(json.loads, filter(lambda l: l.strip().startswith("{"), open(sys.argv[1])))}
for i in (2, 3):
    assert r[i]["result"]["isError"] and "guide is bound to contact xg" in json.dumps(r[i]), r[i]' "$T/mcp-guide.out"
check "H9 MCP as guide: list shows no local agent and no other contact" python3 -c '
import json, sys
r = {m.get("id"): m for m in map(json.loads, filter(lambda l: l.strip().startswith("{"), open(sys.argv[1])))}
t = json.loads(r[4]["result"]["content"][0]["text"])
assert t["local"] == [] and t["groups"] == [] and all(a["address"].startswith("xg/") for a in t["remote"]), t' "$T/mcp-guide.out"
node "$AT" pi "$BROKER" guide 4 "send:bobmach:$X" >"$T/pi-guide.out" 2>&1
check "H10 pi session guide -> bobmach through the intercom adapter: refused" grep -q "refused: guide is bound to contact xg" "$T/pi-guide.out"
P=$(mark pi)
node "$AT" pi "$BROKER" guide 4 "send:xg:to: eng
$P" >/dev/null 2>&1
check "H10 pi session guide -> xg through the adapter: sealed and sent" wait_for 10 grep -q "in send from alice/guide -> eng" "$T/xg.log"
sleep 1
check "H7-10 bob got nothing from guide" bash -c "! grep -q 'from alicemach/guide' '$T/bob.log'"
check "H7-10 planner got nothing from guide" bash -c "! grep -q '$X' '$T/pi-planner.out'"
X=$(mark other)
out=$(la bob send alicemach/guide "$X" --as worker 2>&1)
check "H11 another contact (bob) -> alice/guide: refused ($out)" grep -q "refused guide takes messages from one contact only" <<<"$out"
check "H11 guide got nothing from bob" bash -c "sleep 1; ! grep -q '$X' '$T/guide.out'"
node "$AT" seal "$T/xg" "$(addr alice)" kind=reply reply_to=00000000-0000-4000-8000-000000000000 from_agent=eng body="$X" >"$T/h.env"
node "$AT" post "$(addr alice)" "$T/h.env" ip=10.1.8.1 >/dev/null
check "H12 xg sends a reply to no ask of ours: refused by its rules" wait_for 10 grep -q "REFUSED reply .* rule: xg/eng may not reply" "$T/alice.log"
# Default (not exclusive): another local agent may still write to xg and get its answer.
out=$(la alice send xg/eng "$(mark planner)" --as planner 2>&1)
check "H13 default: planner -> xg/eng is not refused ($out)" bash -c '! grep -qi "refused\|error" <<<"$0"' "$out"
Q=$(mark q2)
la alice ask xg/eng "$Q" --as planner --timeout 40 >"$T/h-ask2.out" 2>&1 &
ASKP=$!
QID=""
for _ in $(seq 30); do
  QID=$(la xg inbox --as eng 2>/dev/null | python3 -c 'import json,sys; print(next((m["id"] for m in map(json.loads, sys.stdin) if sys.argv[1] in m["text"]), ""))' "$Q")
  [ -n "$QID" ] && break
  sleep 0.5
done
la xg reply "$QID" "ANS-$Q" --as eng >/dev/null 2>&1
wait "$ASKP"
check "H13 default: planner asks xg and gets the answer (a reply to our own ask)" grep -q "ANS-$Q" "$T/h-ask2.out"
# --exclusive on an existing contact: only guide may write to xg.
out=$(la alice contact bind xg guide --exclusive --note "external engineer" 2>&1)
check "H14 contact bind xg guide --exclusive" grep -q "only guide may write to xg (exclusive)" <<<"$out"
out=$(la alice send xg/eng "$(mark ex)" --as planner 2>&1)
check "H14 exclusive: planner -> xg/eng refused ($out)" grep -q "contact xg is bound to guide only (exclusive)" <<<"$out"
out=$(la alice send xg/eng "$(mark ex)" 2>&1)
check "H14 exclusive: the plain CLI (machine inbox) -> xg refused" grep -q "exclusive" <<<"$out"
out=$(la alice invite --no-service --agent guide 2>&1)
check "H15 invite --agent guide while guide serves xg: refused" grep -q "already bound to contact xg" <<<"$out"
# The binding is in contacts.json: a restarted daemon enforces it at once.
kill "$ALICE" 2>/dev/null; wait "$ALICE" 2>/dev/null
start_daemon alice; ALICE=$LAST
wait_for 30 logged "$T/alice.log" "inbox: connected" -ge 3 || die "alice did not reconnect"
node "$AT" watch "$T/alice/daemon.sock" guide 300 >"$T/guide2.out" 2>&1 &
PIDS+=($!)
sleep 2
X=$(mark restart)
out=$(la xg send alice/planner "$X" --as eng 2>&1)
check "H16 after a restart: xg -> planner refused ($out)" grep -q "refused rule: xg/eng may not send planner" <<<"$out"
out=$(la xg send alice/guide "$X" --as eng 2>&1)
check "H16 after a restart: xg -> guide pushed ($out)" grep -q "pushed" <<<"$out"
out=$(la alice send bobmach/worker "$X" --as guide 2>&1)
check "H16 after a restart: guide -> bobmach refused" grep -q "guide is bound to contact xg" <<<"$out"
out=$(la alice send xg/eng "$X" --as planner 2>&1)
check "H16 after a restart: planner -> xg refused (exclusive)" grep -q "exclusive" <<<"$out"
# invite --agent: the binding waits with the invite and is applied in the same write that adds the guest.
I=$(la alice invite --no-service --agent helper --note "visitor" 2>/dev/null | sed -n 's/^address: *//p')
la yg join alice "$I" --me yg --no-service >/dev/null 2>&1
check "H17 invite --agent helper: the guest that uses it is bound on arrival" wait_for 15 python3 -c '
import json, sys
c = json.load(open(sys.argv[1]))["yg"]
assert c["bind"] == {"agent": "helper"} and c["note"] == "visitor" and c["rules"][0]["to"] == "helper" and c["rules"][1]["allow"] == [], c' "$T/alice/contacts.json"
check "H18 wire: no binding, agent name or note left alice's machine" bash -c "! grep -aqE 'external engineer|visitor|guide|helper' '$T/wire.bin'"

# ---------- A. last: a flood of one mailbox is cut at 60 a minute ----------
codes=$(for i in $(seq 1 64); do curl -s -o /dev/null -w '%{http_code}\n' -X POST -H "cf-connecting-ip: 10.3.$i.1" -H "x-la-drop: $DROP" --data-binary @"$T/ok.env" "$RELAY/v1/send/$D_ID"; done | sort | uniq -c | tr -s ' ' | tr '\n' ',')
check "A14 64 sends to one mailbox from 64 IPs in a minute: the rest get 429 ($codes)" grep -q "429" <<<"$codes"

finish
