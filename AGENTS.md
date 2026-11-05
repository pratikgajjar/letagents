# letagents: notes for coding agents

End-to-end encrypted messaging between AI agents. Users' setup guide: web/agents.md (served at https://letagents.talk/agents.md). Wire format: PROTOCOL.md.

## Layout

- `cli/`: the `letagents` binary. `crypto.rs` (keys, sealing), `store.rs` (state in ~/.letagents), `relay.rs` (HTTPS/WSS client), `daemon.rs` (daemon.sock, spool, pi-intercom adapter), `intercom.rs` (pi-intercom broker client), `mcp.rs` (MCP server), `service.rs` (launchd/systemd).
- `relay/`: Cloudflare Worker + Durable Object in Rust (workers-rs). Serves the API and the `web/` files.
- `web/`: landing page and agent docs. `index.html` is generated: edit `web/src/page.html` or `web/src/gen.py`, then run `python3 web/src/build.py`.

## Commands

    cargo test --manifest-path cli/Cargo.toml
    cargo build --release --target wasm32-unknown-unknown --manifest-path relay/Cargo.toml
    cd relay && npx wrangler dev --port 8787          # local relay
    LETAGENTS_RELAY=http://localhost:8787 LETAGENTS_AUTH_HOST=letagents.talk LETAGENTS_HOME=/tmp/a cli/target/debug/letagents init a

## Rules

- Never weaken the zero-knowledge property: the relay must never receive plaintext, sender identity, contacts or rules. Change PROTOCOL.md in the same commit as any wire change.
- Keep relay storage short-lived and bounded (see the constants at the top of `relay/src/lib.rs`).
- No new crypto: use RustCrypto/dalek primitives as they are.
- Rust only.
