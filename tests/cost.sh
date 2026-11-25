#!/usr/bin/env bash
# Cost check: Durable Object requests of the old and the new client and relay, same scenario, same clock.
#
# Two local relays (wrangler dev), each with three daemons (a, b, c, all contacts of each other):
#   base: the client of $BASE_REV (default v0.6.0) with its roster and presence intervals divided by $SPEEDUP,
#         and the relay with STATS_FLUSH_MS=0 (one Stats request per envelope, as before the batching).
#   new:  this tree's client with LETAGENTS_TEST_SPEEDUP=$SPEEDUP, and the relay with its stats flush interval
#         divided by $SPEEDUP.
# Both run the same scripted hour (WINDOW seconds of wall time): agents come and go on a and b, b writes to
# a/planner, and c is offline for a while. The relays log one line per DO request (LA_COUNT_DO=1); the check
# passes when the new run makes at least $MIN_CUT % fewer. Then, on the new run only: an offline contact gets the
# current roster when it comes back, rosters converge after an idle spell, and /stats.json counts every
# accepted envelope after the delayed flush.
#
#   tests/cost.sh                       # builds both clients first
#   SPEEDUP=20 WINDOW=180 tests/cost.sh # defaults
#
# Needs: cargo, git, node (npx), worker-build, python3, curl.
set -u
set -m

ROOT=$(cd "$(dirname "$0")/.." && pwd)
T=$(mktemp -d)
PIDS=()
FAILS=0
SPEEDUP=${SPEEDUP:-20}
WINDOW=${WINDOW:-180}
BASE_REV=${BASE_REV:-v0.6.0}
MIN_CUT=${MIN_CUT:-70}
# Every CHURN s, a or b (in turn) gets a new agent and loses the one from two rounds before. So each machine's
# set of agents changes every 2 x CHURN s: with the defaults, every 20 min of use (3 times an hour).
CHURN=${CHURN:-30}
# b writes to a/planner every MSG_EVERY s (0: never): with the defaults, every 10 min of use.
MSG_EVERY=${MSG_EVERY:-30}

cleanup() {
  for p in "${PIDS[@]}"; do kill -- "-$p" 2>/dev/null || kill "$p" 2>/dev/null; done
  for f in "$T"/*/[abc].pid; do [ -f "$f" ] && kill "$(cat "$f")" 2>/dev/null; done
  wait 2>/dev/null
  if [ "$FAILS" -ne 0 ] || [ -n "${KEEP:-}" ]; then echo "logs kept in $T"; else rm -rf "$T"; fi
}
trap cleanup EXIT

ok() { echo "ok   $1"; }
fail() { echo "FAIL $1"; FAILS=$((FAILS + 1)); }
die() { echo "FAIL $1"; FAILS=$((FAILS + 1)); for f in "$T"/*.log "$T"/*/*.log; do [ -f "$f" ] && { echo "--- $f"; tail -20 "$f"; }; done; exit 1; }
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
wait_for() {
  local n=$(($1 * 5)); shift
  for _ in $(seq "$n"); do "$@" >/dev/null 2>&1 && return 0; sleep 0.2; done
  return 1
}
count() { grep -c -- "$2" "$1" 2>/dev/null || true; }

# ---------- clients ----------
cargo build -q --manifest-path "$ROOT/cli/Cargo.toml" || die "cli build"
NEW_BIN=$ROOT/cli/target/debug/letagents
git -C "$ROOT" rev-parse -q --verify "$BASE_REV^{commit}" >/dev/null \
  || git -C "$ROOT" fetch -q --depth 1 origin "refs/tags/$BASE_REV:refs/tags/$BASE_REV" || die "fetch $BASE_REV"
mkdir -p "$T/base-src"
git -C "$ROOT" archive "$BASE_REV" cli | tar -x -C "$T/base-src" || die "checkout $BASE_REV"
# The old intervals, divided by SPEEDUP: a list to every contact every 600 s, a presence check every 30 s.
sed -i.orig \
  -e "s/^const ROSTER_REFRESH: Duration = Duration::from_secs(600);/const ROSTER_REFRESH: Duration = Duration::from_millis($((600000 / SPEEDUP)));/" \
  -e "s/^const PRESENCE_CHECK: Duration = Duration::from_secs(30);/const PRESENCE_CHECK: Duration = Duration::from_millis($((30000 / SPEEDUP)));/" \
  "$T/base-src/cli/src/daemon.rs"
[ "$(grep -c "Duration::from_millis($((600000 / SPEEDUP)))\|Duration::from_millis($((30000 / SPEEDUP)))" "$T/base-src/cli/src/daemon.rs")" = 2 ] || die "could not speed up the $BASE_REV client"
CARGO_TARGET_DIR=$ROOT/cli/target/cost-base cargo build -q --manifest-path "$T/base-src/cli/Cargo.toml" || die "build $BASE_REV client"
BASE_BIN=$ROOT/cli/target/cost-base/debug/letagents

# ---------- relays ----------
relay() { # relay <name> <stats flush ms>: starts wrangler dev; sets URL
  local port; port=$(free_port)
  (cd "$ROOT/relay" && exec npx --yes wrangler@latest dev --ip 127.0.0.1 --port "$port" --persist-to "$T/$1-state" \
    --var LA_COUNT_DO:1 --var "STATS_FLUSH_MS:$2" --show-interactive-dev-session=false >"$T/$1-relay.log" 2>&1) &
  PIDS+=($!)
  wait_for 300 curl -fsS "http://127.0.0.1:$port/health" || die "relay $1 did not start"
  URL=http://127.0.0.1:$port
}
# One after the other: both run the same build output.
relay base 0
BASE_RELAY=$URL
relay new $((600000 / SPEEDUP))
NEW_RELAY=$URL

# ---------- the scenario ----------
# run <name> <bin> <relay>: machines a, b, c in $T/<name>; writes $T/<name>.count (DO requests in the window).
run() {
  local name=$1 bin=$2 url=$3 D=$T/$1
  mkdir -p "$D/pi" "$D/codex"
  la() { local m=$1; shift; LETAGENTS_HOME="$D/$m" PI_CODING_AGENT_DIR="$D/pi" CODEX_HOME="$D/codex" \
    LETAGENTS_RELAY="$url" LETAGENTS_AUTH_HOST=letagents.talk LETAGENTS_TEST_SPEEDUP=$SPEEDUP "$bin" "$@"; }
  # op <machine> <op> <agent>: one daemon.sock request (hello: the agent is here; bye: it left)
  op() { python3 - "$D/$1/daemon.sock" "$2" "$3" <<'PY'
import json, socket, struct, sys
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])
b = json.dumps({"op": sys.argv[2], "as": sys.argv[3], "harness": "cost"}).encode()
s.sendall(struct.pack(">I", len(b)) + b)
n = struct.unpack(">I", s.recv(4))[0]
assert json.loads(s.recv(n))["ok"]
PY
  }
  start() { la "$1" daemon >>"$D/$1.log" 2>&1 & echo $! >"$D/$1.pid"; }
  stop() { kill "$(cat "$D/$1.pid")" 2>/dev/null; wait "$(cat "$D/$1.pid")" 2>/dev/null; }
  for m in a b c; do la $m init $m >/dev/null || { echo "$name: init $m" >"$D/error"; return 1; }; done
  for m in a b c; do
    for o in a b c; do [ $m = $o ] || la $m contact add $o "$(la $o whoami | sed -n 's/^address: *//p')" >/dev/null || return 1; done
  done
  for m in a b c; do start $m; done
  for m in a b c; do wait_for 60 grep -q "inbox: connected" "$D/$m.log" || { echo "$name: $m did not connect" >"$D/error"; return 1; }; done
  sleep 10 # start-up done: lists answered, drop codes confirmed
  local log=$T/$name-relay.log c0 t i
  c0=$(count "$log" "la-do ")
  t=$(date +%s)
  # Agents come and go on a and b (CHURN, above), and b writes to a/planner (MSG_EVERY). c is offline from 40 s to
  # 160 s (longer than the silence limit at this speed). While c is away, a gets agent "away-1" at 145 s (c, silent,
  # gets this one push) and "back-marker" at 152 s (held until c is back).
  for i in $(seq 0 $((WINDOW - 1))); do
    if [ $((i % CHURN)) = 0 ]; then
      local n=$((i / CHURN))
      op $([ $((n % 2)) = 0 ] && echo a || echo b) hello "w$n"
      [ $n -ge 2 ] && op $([ $((n % 2)) = 0 ] && echo a || echo b) bye "w$((n - 2))"
    fi
    [ "$MSG_EVERY" -gt 0 ] && [ $((i % MSG_EVERY)) = $((MSG_EVERY / 2)) ] && la b send a/planner "note $i" --as worker >/dev/null 2>&1 &
    [ $i = 40 ] && stop c
    [ $i = 145 ] && op a hello away-1
    [ $i = 152 ] && op a hello back-marker
    [ $i = 160 ] && { echo "$(date +%s)" >"$D/c-back"; start c; }
    sleep $((t + i + 1 - $(date +%s) > 0 ? t + i + 1 - $(date +%s) : 0))
  done
  echo $(($(count "$log" "la-do ") - c0)) >"$T/$name.count"
  grep "la-do " "$log" | tail -n +$((c0 + 1)) | sed 's/.*la-do //' | sort | uniq -c >"$T/$name.kinds"
}

run base "$BASE_BIN" "$BASE_RELAY" &
PIDS+=($!)
BASE_RUN=$!
run new "$NEW_BIN" "$NEW_RELAY"
wait "$BASE_RUN"
for n in base new; do [ -s "$T/$n.count" ] || die "the $n run did not finish ($(cat "$T/$n/error" 2>/dev/null))"; done

# Billed requests, as Cloudflare counts them: incoming WebSocket messages (the acks) at 20:1, all else 1:1.
billed() { awk '/ws_message/ { w += $1; next } { r += $1 } END { printf "%.0f", r + w / 20 }' "$T/$1.kinds"; }
BASE=$(billed base)
NEW=$(billed new)
CUT=$(((BASE - NEW) * 100 / (BASE > 0 ? BASE : 1)))
RAW_CUT=$((($(cat "$T/base.count") - $(cat "$T/new.count")) * 100 / $(cat "$T/base.count")))
echo "DO requests in ${WINDOW} s (x$SPEEDUP: $((WINDOW * SPEEDUP / 60)) min of use, agents change every $((2 * CHURN * SPEEDUP / 60)) min on a and on b):"
echo "  billed (WebSocket messages 20:1): base $BASE, new $NEW, cut $CUT %"
echo "  raw (every DO event 1:1):         base $(cat "$T/base.count"), new $(cat "$T/new.count"), cut $RAW_CUT %"
paste "$T/base.kinds" "$T/new.kinds" | sed 's/^/     base|new: /'
for n in base new; do
  limited=$(grep -c "/v1/.* 429" "$T/$n-relay.log")
  [ "$limited" = 0 ] || fail "$n run hit a rate limit $limited times (the counts are not comparable)"
done
if [ "$CUT" -ge "$MIN_CUT" ]; then ok "billed DO requests cut by $CUT % (at least $MIN_CUT %)"; else fail "billed DO requests cut by only $CUT % (want $MIN_CUT %)"; fi

# ---------- behaviour, new run ----------
D=$T/new
la() { local m=$1; shift; LETAGENTS_HOME="$D/$m" PI_CODING_AGENT_DIR="$D/pi" CODEX_HOME="$D/codex" LETAGENTS_RELAY="$NEW_RELAY" LETAGENTS_AUTH_HOST=letagents.talk LETAGENTS_TEST_SPEEDUP=$SPEEDUP "$NEW_BIN" "$@"; }
op() { python3 - "$D/$1/daemon.sock" "$2" "$3" <<'PY'
import json, socket, struct, sys
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])
b = json.dumps({"op": sys.argv[2], "as": sys.argv[3], "harness": "cost"}).encode()
s.sendall(struct.pack(">I", len(b)) + b)
n = struct.unpack(">I", s.recv(4))[0]
assert json.loads(s.recv(n))["ok"]
PY
}
if grep -q "presence: c is silent" "$D/a.log"; then ok "presence: a held its pushes to c while c was offline"; else fail "presence: a did not hold pushes to offline c"; fi
# c restarted at 160 s: its list to a is answered with a's current roster (back-marker came while c was away).
if awk '/inbox: connected/ { n++ } n >= 2' "$D/c.log" | grep -q "roster from a: .*back-marker"; then
  ok "offline contact: c got a's current roster when it came back"
else
  fail "offline contact: c did not get a's current roster"
fi

# Idle spell: every agent leaves, then nothing for longer than the silence limit (30 min / SPEEDUP).
for n in $(seq 0 $((WINDOW / CHURN))); do op a bye "w$n" 2>/dev/null; op b bye "w$n" 2>/dev/null; done
op a bye away-1; op a bye back-marker; op b bye worker 2>/dev/null
sleep $((1800 / SPEEDUP + 15))
# roster_has <machine> <agent>: the latest roster <machine> got from a names <agent>
roster_has() { grep "roster from a:" "$D/$1.log" | tail -1 | grep -q -- "$2"; }
op a hello late-1
if wait_for 20 roster_has b late-1; then ok "after an idle spell: b sees a's new agent (one push to a silent contact)"; else fail "after an idle spell: b did not see a's new agent"; fi
sleep $((60 / SPEEDUP + 2))
op a hello late-2
if wait_for 20 roster_has b late-2 && wait_for 20 roster_has c late-2; then
  ok "after an idle spell: the next change reaches b and c too (they answered, so pushes resumed)"
else
  fail "after an idle spell: the second change did not arrive"
fi

# Stats after a delayed flush: stop the daemons (their sockets close, every mailbox flushes), then the totals
# must match the envelopes the relay accepted.
for p in a b c; do kill "$(cat "$D/$p.pid")" 2>/dev/null; done
sleep 5
for n in new base; do
  accepted=$(grep -c "POST /v1/send/.* 202" "$T/$n-relay.log")
  stats=$(curl -fsS "$( [ $n = new ] && echo "$NEW_RELAY" || echo "$BASE_RELAY")/stats.json?$n")
  if python3 -c 'import json,sys; s=json.loads(sys.argv[1]); n=int(sys.argv[2]); assert s["messages_total"] == n == sum(s["messages"]), (s["messages_total"], sum(s["messages"]), n)' "$stats" "$accepted"; then
    ok "stats.json ($n relay): messages_total = sum per hour = $accepted accepted envelopes"
  else
    fail "stats.json ($n relay) after the flush: $(python3 -c 'import json,sys; s=json.loads(sys.argv[1]); print(s.get("messages_total"), sum(s.get("messages", [])))' "$stats") vs $accepted accepted"
  fi
done

if [ "$FAILS" -eq 0 ]; then echo "cost: all checks passed"; else echo "cost: $FAILS check(s) failed"; fi
[ "$FAILS" -eq 0 ]
