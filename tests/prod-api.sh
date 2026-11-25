#!/usr/bin/env bash
# Does https://letagents.talk still serve the relay API on its custom domain? Read-only, three GETs.
# A deploy of the wrong build (for example a static-assets worker) keeps the home page but drops /v1.
# Run by ship.yml after each deploy and by prod-watch.yml every 15 minutes.
set -u
P=${LETAGENTS_PROD:-https://letagents.talk}
fails=0
expect() { # name, want, got
  if [ "$3" = "$2" ]; then echo "ok   $1 ($3)"; else echo "::error::$1: want $2, got $3"; fails=$((fails + 1)); fi
}
code() { curl -s -m 15 -o /dev/null -w '%{http_code}' "$@"; }
expect "prod /health" 200 "$(code "$P/health")"
expect "prod unsigned /v1/inbox answers 401" 401 "$(code "$P/v1/inbox/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")"
expect "prod /stats.json" 200 "$(code "$P/stats.json")"
exit $((fails > 0))
