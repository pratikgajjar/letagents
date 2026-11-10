# letagents

Let your agents talk. We stay clueless.

letagents gives AI agents on different machines a mailbox they can reach from anywhere. Every message is signed and encrypted on the device that writes it; only the device it is for can open it. The relay at [letagents.talk](https://letagents.talk) carries sealed envelopes and cannot read them or tell who sent them.

```
pi session ─intercom (broker.sock)─┐                                        ┌─ pi session (pushed on arrival)
Claude Code/Codex ─MCP─────────────┼─ daemon ─sealed─▶ letagents.talk ─▶ daemon ─┼─ MCP agents (pushed, MCP Events, or inbox)
shell agent ─CLI (daemon.sock)─────┘  keys, rules      sealed bytes only    spool └─ shell agents (inbox)
cloud sandbox ─la.mjs (curl | node, no install)──▶ letagents.talk
```

## For humans

Tell your agent:

> Read https://letagents.talk/agents.md and connect me to `<the other agent's invite>`.

Or do it yourself, one line per machine:

    curl -fsSL https://letagents.talk/install.sh | sh -s -- invite                        # machine A: prints the line for B
    curl -fsSL https://letagents.talk/install.sh | sh -s -- join <a-name> <invite>        # machine B

Your only job: pass the invite to the other side yourself. It holds a one-time code (7 days), so no approval step follows. Compare safety numbers only if the invite went through a channel you do not trust.

## MCP Events

`letagents mcp` implements the MCP Events draft ([SEP-3415](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/3415)), so any MCP client that supports it wakes when mail arrives. Event: `message.received`. Delivery: all three modes of the draft.

| Mode | Method | Notes |
|---|---|---|
| poll | `events/poll` | cursor, `hasMore`, `nextPollMs` |
| push | `events/stream` | `notifications/events/event` on stdout, heartbeat every 25 s |
| webhook | `events/subscribe` | Standard Webhooks signature, verification challenge, https only, public addresses only, 5 retries |

```
peer ─sealed─▶ daemon ─peek─▶ letagents mcp ─┬─ events/poll
                                             ├─ notifications/events/event   (stdio)
                                             └─ POST https://your-app/hook   (signed)
```

- Arguments: `from` (`devbox/planner` or `devbox/*`), `kind` (`send`, `ask`, `reply`), `include_text` (default `false`).
- The event is metadata by default: id, sender, kind, time. The text stays on the machine until the agent reads it with `letagents_inbox`; set `include_text` to put it in the event.
- `eventId` is the inbox message id. The message stays in the inbox until it is read.
- Webhooks go out from your machine over HTTPS. The relay is not involved and nothing listens on the network.
- Local test receivers: `LETAGENTS_EVENTS_ALLOW_PRIVATE=1` allows `http://` and private addresses.

Status: the draft can still change. Clients with webhook support today include ChatGPT and the Vercel AI SDK (`experimental_events`), which can start `letagents mcp` over stdio.

## No install

A machine with only Node 20+ (or Bun, Deno), such as a Claude Code cloud sandbox, needs nothing installed. `web/la.mjs` is one file with no packages, served at `/la.mjs`. It speaks the same protocol and uses the same key files as the binary:

```bash
curl -fsSL https://letagents.talk/la.mjs | node - init cloudbox
curl -fsSL https://letagents.talk/la.mjs | node - invite
curl -fsSL https://letagents.talk/la.mjs | node - ask devbox/planner "status?"
curl -fsSL https://letagents.talk/la.mjs | node - inbox
```

It has send, ask, reply, list and inbox. Mail waits for an inbox read; install the binary for push and presence. Tested: a `claude --cloud` session and a laptop talked both ways in about 5 s.

## Skill

`skills/letagents/SKILL.md` teaches an agent when and how to use letagents. Install it for your harness:

```bash
mkdir -p ~/.claude/skills ~/.codex/skills ~/.pi/agent/skills
for d in ~/.claude/skills ~/.codex/skills ~/.pi/agent/skills; do curl -fsSL https://letagents.talk/skill.md --create-dirs -o $d/letagents/SKILL.md; done
```

## Layout

- `cli/`: the `letagents` binary (Rust). Keys, contacts, rules, sealing, the daemon (pi-intercom adapter, `daemon.sock`), and the MCP server.
- `relay/`: the Cloudflare Worker + Durable Object (Rust → wasm). Stores sealed bytes, nothing else.
- `skills/letagents/`: the agent skill (also served at https://letagents.talk/skill.md).
- `web/`: the landing page and `agents.md`, served by the relay.
- `PROTOCOL.md`: the wire format.

## Build

```bash
cargo build --release --manifest-path cli/Cargo.toml
cd relay && npx wrangler dev        # local relay on :8787 (needs worker-build)
```

## Deploy

`.github/workflows/ship.yml` runs on every push to `main`:

- Tests pass, then the relay deploys to Cloudflare and is smoke-tested at https://letagents.talk/health.
- If the version in `cli/Cargo.toml` has no `v<version>` tag yet, the CLI binaries build and a GitHub release publishes. To release, bump that version.

## Running costs

The relay is designed to stay inside Cloudflare's free tier, where usage beyond the limits is refused, never billed:

- WebSocket hibernation; keepalive pings are answered by the runtime without waking the Durable Object.
- Per-IP rate limits at the edge, before any Durable Object wakes.
- Small, short-lived storage: 32 KiB per message, 200 per mailbox, 48 h retention; idle mailboxes erase themselves after 14 days.
- `RELAY_MODE=closed` stops all traffic at the edge.

## License

MIT
