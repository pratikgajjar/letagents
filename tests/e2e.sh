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
BOB=$!
PIDS+=($BOB)
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

# ---------- presence: a new agent shows up at the contact ----------
# Event-driven rosters (docs/DECISIONS.md #14): the push on change, at most one a minute per contact.
start=$(date +%s)
la alice inbox --as newcomer --wait 1 >/dev/null 2>&1
if wait_for 60 grep -q "roster from alice: .*newcomer" "$T/bob.log"; then
  ok "presence: bob sees alice/newcomer in $(($(date +%s) - start)) s (at most 60)"
else
  fail "presence: alice/newcomer did not reach bob in 60 s"
fi

# ---------- per-contact drop codes ----------
# Added by bare addresses (the shared code), alice and bob move to codes of their own on the first exchange.
own_code() { python3 -c 'import json,sys; c=json.load(open(sys.argv[1]))[sys.argv[2]]; sys.exit(0 if c.get("our_drop") and c.get("our_drop_ok") else 1)' "$T/$1/contacts.json" "$2"; }
check "drop codes: alice and bob each confirmed a code of their own" wait_for 30 bash -c "$(declare -f own_code); T='$T'; own_code alice bob && own_code bob alice"
check "drop codes: bob writes to alice with her code for him, not the shared one" python3 -c '
import json, sys
a, b = (json.load(open(f"{sys.argv[1]}/{m}/contacts.json")) for m in ("alice", "bob"))
assert b["alice"]["address"].split(".")[3] == a["bob"]["our_drop"] != sys.argv[2].split(".")[3]' "$T" "$A_ADDR"

# ---------- routing: never guess a session ----------
# A send or ask that names no agent is refused with a hint; nothing lands in a session or the machine inbox.
out=$(la bob send alice "plain note" --as worker 2>&1)
check "plain send refused: name an agent ($out)" grep -q "alice -> refused name an agent: send to alice/<agent>" <<<"$out"
start=$(date +%s)
out=$(la bob ask alice "plain question" --as worker --timeout 30 2>&1)
took=$(($(date +%s) - start))
if grep -q "refused: name an agent" <<<"$out" && [ "$took" -lt 10 ]; then ok "plain ask refused in ${took}s"; else fail "plain ask (took ${took}s, output: $out)"; fi
la alice inbox --wait 1 >"$T/machine.out" 2>&1
check "plain mail not in alice's machine inbox" bash -c "! grep -q plain '$T/machine.out'"
# A plain send to the machine of an agent that wrote to us goes back to that agent (bob/worker wrote to
# alice/planner above).
la alice send bob "plain answer to worker" --as planner >/dev/null 2>&1
check "plain send continues the conversation: bob/worker got it" inbox_has bob worker "plain answer to worker"
# A reply after the ask timed out and the asker's daemon restarted still reaches the asker, by ask id.
la bob ask alice/planner "late question" --as worker --timeout 2 >/dev/null 2>&1
LATE=$(ask_id)
kill -- "-$BOB" 2>/dev/null || kill "$BOB"
wait "$BOB" 2>/dev/null
la bob daemon >>"$T/bob.log" 2>&1 &
BOB=$!
PIDS+=($BOB)
wait_for 30 logged "$T/bob.log" "inbox: connected" -ge 2 || die "bob daemon did not reconnect after restart"
if [ -n "$LATE" ] && la alice reply "$LATE" "late answer" --as planner >/dev/null 2>&1; then
  check "late reply after restart: bob/worker got it" inbox_has bob worker "late answer"
else
  fail "late reply (ask id: $LATE)"
fi

# ---------- one-time invite codes ----------
# carol uses alice's invite: alice's daemon adds her at once. dave reuses the same invite: only a request.
INV=$(la alice invite | sed -n 's/^address: *//p')
check "invite has a 5th part (the code)" test "$(tr -cd . <<<"$INV" | wc -c | tr -d ' ')" = 4
# join: creates carol's keys and mailbox, adds alice, sends hello with the code (one command)
la carol join alice "$INV" --me carol >"$T/join.out" 2>&1 || { fail "join: $(cat "$T/join.out")"; }
if wait_for 20 grep -q "added contact carol .* by invite code" "$T/alice.log"; then ok "invite: alice added carol at once"; else fail "invite: carol not added"; fi
la alice send carol/desk "hi carol" --as planner >/dev/null 2>&1
check "invite: alice can write to carol" bash -c "LETAGENTS_HOME='$T/carol' LETAGENTS_RELAY='$RELAY' LETAGENTS_AUTH_HOST=letagents.talk '$BIN' check 2>/dev/null | grep -q 'hi carol'"
la dave init dave >/dev/null && la dave check >/dev/null 2>&1
la dave contact add alice "$INV" >/dev/null && la dave hello alice >/dev/null 2>&1
if wait_for 20 grep -q 'contact request .* from "dave"' "$T/alice.log" && ! la alice contact ls | grep -q '^dave '; then
  ok "invite: a used code only makes a contact request"
else
  fail "invite reuse (alice contacts: $(la alice contact ls | cut -c1-20 | tr '\n' ' '))"
fi

# ---------- MCP Events: an independent client (Vercel AI SDK, its compatibility mode) ----------
cp -R "$ROOT/tests/aisdk" "$T/aisdk" && (cd "$T/aisdk" && npm install --silent --no-audit --no-fund >/dev/null 2>&1) || die "npm install @ai-sdk/mcp"
SEND="LETAGENTS_HOME='$T/bob' LETAGENTS_RELAY='$RELAY' LETAGENTS_AUTH_HOST=letagents.talk '$BIN' send alice/hooky 'hello events' --as worker"
if LETAGENTS_HOME="$T/alice" PI_CODING_AGENT_DIR="$T/pi" LETAGENTS_RELAY="$RELAY" LETAGENTS_AUTH_HOST=letagents.talk \
  node "$T/aisdk/verify.mjs" "$BIN" hooky "$SEND" >"$T/events.out" 2>&1 && grep -q '"text":"hello events"' "$T/events.out"; then
  ok "MCP events: AI SDK client subscribed, got the signed webhook event, refreshed, unsubscribed"
else
  fail "MCP events (AI SDK)"; cat "$T/events.out"
fi

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

# ---------- public usage counters ----------
STATS=$(curl -fsS "$RELAY/stats.json")
if python3 -c 'import json,sys; s=json.loads(sys.argv[1]); m=s["messages"]; assert s["machines_online"] >= 2 and sum(m[-24:]) >= 5 and s["messages_total"] >= sum(m) and max(s["peak_online"][-24:]) >= 2 and len(m) == len(s["peak_online"]) <= 720, s' "$STATS"; then
  ok "stats.json counts machines online and messages per hour ($(python3 -c 'import json,sys; s=json.loads(sys.argv[1]); print(s["machines_online"], "online,", sum(s["messages"][-24:]), "messages")' "$STATS"))"
else
  fail "stats.json ($STATS)"
fi
BROWSER_UA='Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36'
BROWSER_ACCEPT='text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8'
PAGE=$(curl -fsS -A "$BROWSER_UA" -H "Accept: $BROWSER_ACCEPT" "$RELAY/")
if grep -q "machines online" <<<"$PAGE" && grep -q '"peak_online"' <<<"$PAGE" && ! grep -q "__ONLINE__\|__MSGS_\|__STATS_" <<<"$PAGE"; then ok "home page shows live counters"; else fail "home page counters"; fi

# ---------- docs: a page for browsers, the markdown file for everyone else ----------
# get <path> <out> [curl args...]: body to <out>, headers to <out>.h (lower case)
get() { local p=$1 o=$2; shift 2; curl -sS -o "$o" -D - "$@" "$RELAY$p" | tr -d '\r' | tr 'A-Z' 'a-z' >"$o.h"; }
is_md() { grep -q '^content-type: text/markdown' "$1.h" && cmp -s "$1" "$2"; }
is_html() { grep -q '^content-type: text/html' "$1.h" && head -c 15 "$1" | grep -q '<!doctype html>' && grep -q "^content-security-policy: .*script-src 'sha256-" "$1.h"; }
varied() { grep -q '^vary: accept, user-agent$' "$1.h"; }
B=(-A "$BROWSER_UA" -H "Accept: $BROWSER_ACCEPT")
for d in "/:web/agents.md" "/agents.md:web/agents.md" "/protocol.md:PROTOCOL.md" "/PROTOCOL.md:PROTOCOL.md" "/skill.md:skills/letagents/SKILL.md" "/SKILL.md:skills/letagents/SKILL.md"; do
  p=${d%%:*} src=$ROOT/${d#*:} o=$T/doc
  get "$p" "$o"; check "$p: curl gets the markdown, byte for byte" is_md "$o" "$src"
  check "$p: Vary: Accept, User-Agent" varied "$o"
  get "$p" "$o" -H 'Accept: text/html'; check "$p: curl with Accept text/html still gets markdown" is_md "$o" "$src"
  get "$p" "$o" "${B[@]}"; check "$p: a browser gets the HTML page" is_html "$o"
  check "$p: Vary on the HTML page too" varied "$o"
  get "$p?raw" "$o" "${B[@]}"; check "$p?raw: a browser gets markdown" is_md "$o" "$src"
  get "$p" "$o" -A "$BROWSER_UA" -H 'Accept: text/markdown, text/html;q=0.9'; check "$p: Accept text/markdown wins" is_md "$o" "$src"
  get "$p?html" "$o"; check "$p?html: curl gets the HTML page" is_html "$o"
done
for bot in 'Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; ClaudeBot/1.0; +claudebot@anthropic.com)' \
  'Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko); compatible; GPTBot/1.2; +https://openai.com/gptbot' \
  'Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko); compatible; ChatGPT-User/1.0; +https://openai.com/bot' \
  'Mozilla/5.0 (compatible; PerplexityBot/1.0; +https://perplexity.ai/perplexitybot)' 'python-requests/2.32.3' 'node' 'axios/1.7.7'; do
  get /agents.md "$T/doc" -A "$bot" -H "Accept: $BROWSER_ACCEPT"
  check "AI fetcher or library gets markdown, also with Accept text/html (${bot:0:60})" is_md "$T/doc" "$ROOT/web/agents.md"
done
get /agents.md "$T/doc" -A 'Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)'
check "a link-preview bot gets the HTML page (og tags)" is_html "$T/doc"
for p in /llms.txt /llms-full.txt /security.txt /install.sh; do
  get "$p" "$T/doc" "${B[@]}"; check "$p stays plain text for a browser" grep -q '^content-type: text/plain' "$T/doc.h"
done
get /protocol.md "$T/doc" "${B[@]}"
check "protocol page: canonical, markdown alternate, og tags, copy script" bash -c 'grep -q "<link rel=\"canonical\" href=\"https://letagents.talk/protocol.md\">" "$0" && grep -q "<link rel=\"alternate\" type=\"text/markdown\" href=\"?raw\"" "$0" && grep -q "og:image\" content=\"https://letagents.talk/og.png" "$0" && grep -q "<h2 id=\"sealing\">" "$0"' "$T/doc"
check "protocol page: the CSP hash matches its script" python3 -c '
import base64, hashlib, re, sys
html, hdr = open(sys.argv[1]).read(), open(sys.argv[1] + ".h").read()
js = html.split("<script>", 1)[1].split("</script>", 1)[0]
h = base64.b64encode(hashlib.sha256(js.encode()).digest()).decode().lower()
assert "sha256-" + h in hdr, (h, hdr)' "$T/doc"
for img in /strip-night.webp /strip-dawn.webp; do
  check "$img is served (under 30 KB)" bash -c '[ "$(curl -fsS -o /dev/null -w "%{http_code} %{content_type}" "$0")" = "200 image/webp" ] && [ "$(curl -fsS "$0" | wc -c)" -lt 30000 ]' "$RELAY$img"
done

# ---------- MCP Events: the official conformance suite ----------
# modelcontextprotocol/conformance PR 521 (SEP-3415 server scenarios), pinned. The runner speaks Streamable
# HTTP; tests/conformance/bridge.mjs maps it to `letagents mcp` on stdio. bob/ticker writes to alice/conf
# every 2 s, so streams, polls and webhooks have real events. That is half the relay's 60 sends a minute per
# mailbox, and this section runs last, so no other check meets the rate limit. The Mcp-Name rows are
# Streamable HTTP rules and letagents speaks stdio only: they are counted as not applicable.
CONF_COMMIT=76fef32dad810143a56819065b286fa0e93a7256
CONF="$T/conformance"
if git init -q "$CONF" && git -C "$CONF" fetch -q --depth 1 https://github.com/modelcontextprotocol/conformance.git "$CONF_COMMIT" \
  && git -C "$CONF" checkout -q FETCH_HEAD && (cd "$CONF" && npm ci --silent --no-audit --no-fund >/dev/null 2>&1); then
  LETAGENTS_HOME="$T/alice" PI_CODING_AGENT_DIR="$T/pi" LETAGENTS_RELAY="$RELAY" LETAGENTS_AUTH_HOST=letagents.talk LETAGENTS_EVENTS_CONFORMANCE=1 \
    node "$ROOT/tests/conformance/bridge.mjs" "$T/bridge.port" "$BIN" mcp --as conf >"$T/bridge.log" 2>&1 &
  PIDS+=($!)
  (while sleep 2; do la bob send alice/conf "tick $(date +%s)" --as ticker >/dev/null 2>&1; done) &
  TICKER=$!
  PIDS+=($TICKER)
  wait_for 10 test -s "$T/bridge.port" || die "conformance bridge did not start"
  for s in events-discovery events-poll events-push events-webhook events-webhook-delivery; do
    (cd "$CONF" && npx tsx src/index.ts server --url "http://127.0.0.1:$(cat "$T/bridge.port")/mcp" --scenario "$s" --timeout 120000 -o "$T/conf-results") >"$T/conf-$s.log" 2>&1
    res=$(python3 - "$T/conf-results" "$s" <<'PY'
import glob, json, sys
NA = {"sep-3415-mcp-name-missing-rejected", "sep-3415-mcp-name-mismatch-rejected"}  # Streamable HTTP only
d = sorted(glob.glob(f"{sys.argv[1]}/server-{sys.argv[2]}-*/checks.json"))
checks = json.load(open(d[-1])) if d else []
n = lambda st: sum(c["status"] == st and c["id"] not in NA for c in checks)
bad = [c["id"] for c in checks if c["status"] == "FAILURE" and c["id"] not in NA]
print(f"{'ok' if checks and not bad else 'FAIL'} {n('SUCCESS')} passed, {len(bad)} failed, {n('WARNING')} warnings, {n('SKIPPED')} skipped, {sum(c['id'] in NA for c in checks)} n/a (stdio){' ' + ' '.join(bad) if bad else ''}")
PY
)
    if [ "${res%% *}" = ok ]; then ok "conformance $s: ${res#ok }"; else fail "conformance $s: ${res#FAIL }"; tail -40 "$T/conf-$s.log"; fi
  done
  kill "$TICKER" 2>/dev/null
else
  fail "conformance suite checkout ($CONF_COMMIT)"
fi

if [ "$FAILS" -eq 0 ]; then echo "e2e: all checks passed"; else echo "e2e: $FAILS check(s) failed"; fi
[ "$FAILS" -eq 0 ]
