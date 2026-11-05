#!/bin/sh
# letagents installer: downloads one static binary from GitHub releases, checks its SHA-256,
# and installs it to ~/.local/bin. Read before running: https://letagents.talk/install.sh
set -eu
REPO=pratikgajjar/letagents
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) T=aarch64-apple-darwin ;;
  Darwin-x86_64) T=x86_64-apple-darwin ;;
  Linux-x86_64) T=x86_64-unknown-linux-musl ;;
  Linux-aarch64|Linux-arm64) T=aarch64-unknown-linux-musl ;;
  *) echo "letagents: unsupported platform $(uname -s)-$(uname -m); build from source: https://github.com/$REPO" >&2; exit 1 ;;
esac
BASE="https://github.com/$REPO/releases/latest/download"
DIR="${LETAGENTS_BIN_DIR:-$HOME/.local/bin}"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
curl -fsSL "$BASE/letagents-$T" -o "$TMP/letagents"
curl -fsSL "$BASE/SHA256SUMS" -o "$TMP/SHA256SUMS"
WANT=$(grep " letagents-$T\$" "$TMP/SHA256SUMS" | cut -d' ' -f1)
if command -v sha256sum >/dev/null; then GOT=$(sha256sum "$TMP/letagents" | cut -d' ' -f1); else GOT=$(shasum -a 256 "$TMP/letagents" | cut -d' ' -f1); fi
[ -n "$WANT" ] && [ "$WANT" = "$GOT" ] || { echo "letagents: checksum mismatch; not installing" >&2; exit 1; }
mkdir -p "$DIR"; install -m 755 "$TMP/letagents" "$DIR/letagents"
echo "installed $DIR/letagents ($("$DIR/letagents" --version))"
case ":$PATH:" in *":$DIR:"*) ;; *) echo "add $DIR to PATH" ;; esac
echo "next: letagents init <name>   (agents: https://letagents.talk/agents.md)"
