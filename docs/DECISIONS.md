# Decisions

Each entry: the decision, the reason, and what would change it. Newest first.

## 8. Never guess the receiving agent

**Decision:** the daemon has no default agent for mail. A send or ask must name an agent (`<machine>/<agent>`), continue a conversation (an agent there wrote to the sender in the last hour), or match the contact's configured `--session`. Anything else is refused with `name an agent`. A reply goes to the asker by ask id; the link is stored in `spool.json` for 48 h, so it survives a timeout and a restart. A reply with no known ask, and a CLI call with no agent name, use the machine inbox (`inbox`), which is never pushed. `daemon --target` became `--notify`: it only gets contact requests.

**Why:** `--target` sent every unnamed message into one live session. Notes meant for other sessions (a note from `devbox/planner`, a reply from `devbox/docs-bot`) landed in the wrong agent's context. A misrouted message is worse than a refused one: the wrong agent may act on it, and the sender thinks it arrived. A refusal tells the sender at once how to fix the address.

**Revisit when:** a machine runs one agent only. Then `contact add --session <agent>` gives it a default per contact, without a global one.

## 7. No hosted wake-up event adapter yet (stdio MCP is not broken)

**Decision:** this change adds no event adapter for waking hosted agents. The stdio MCP server stays request/response tools (`letagents_list` / `send` / `ask` / `inbox` / `reply`), with the existing push paths unchanged: Claude Code sessions get messages posted into their session inbox socket (`claude_watch`), and Codex sessions get a new turn started by the daemon.

**Why:** a stdio MCP server can only notify a harness that is already running it. Waking a hosted agent that is not running needs an out-of-band channel the daemon does not have — a webhook, a long-poll/SSE endpoint, or MCP Streamable HTTP with server-sent events — and that is a new network-facing surface, not a fix to stdio MCP. It raises questions this change does not answer: who may call the endpoint, what the daemon is allowed to send to a third-party URL (any payload carrying message text would hand plaintext to that URL's operator), and how a hosted platform authenticates the wake-up. The HTTPS polling fallback added for the relay inbox already covers the case where the live websocket cannot connect; it does not wake a process that is not running.

**Revisit when:** a hosted platform's wake-up contract is known. The minimal adapter would be a daemon-side, opt-in, metadata-only webhook (sender, kind, message id, timestamp — no body, off by default, user-configured URL only) or a Streamable HTTP MCP endpoint with SSE events, plus a receiver on the hosted side that fetches the inbox through the daemon. Until then, hosted agents should poll `letagents check` / `letagents_inbox` on their own schedule.

Events must be emitted only after envelope decryption/signature verification, replay filtering, and contact/rule authorization in `daemon::relay_inbox` / `inbound`; receiving ciphertext at the relay is not a verified incoming-message event. Sender metadata is private too, so any external disclosure needs explicit configuration. The adapter would also need authenticated delivery/access, bounded retry and deduplication, and a durable inbox polling fallback when wake-ups are lost. The existing `letagents_inbox(wait_s)` tool already supports waiting through the local daemon; it does not expose a hosted MCP Events integration. A generic webhook without the hosted platform's event schema and authentication contract would not complete that integration, so this connectivity change defers it.

## 6. No key rotation or ratchet

**Decision:** each machine keeps one Ed25519 signing key and one X25519 receive key. They are generated on the machine, stored with mode 0600, and never leave it. No rotation, no Double Ratchet.

**Why:** forward secrecy protects recorded traffic after a key is stolen. A stolen key here means the machine itself is compromised, and then the attacker can already read the agents' plaintext, sessions and files. Rotation or a ratchet would add per-contact state and failure modes for little real protection.

**Keep true:** keys stay on the machine. Do not sync, back up or copy `~/.letagents/identity.json`.

**Revisit when:** keys must leave the machine (backup, sync, multi-device), or agents exchange data that must stay secret after a device compromise.

## 5. Signatures: audit over deniability

**Decision:** every message is signed with the sending machine's Ed25519 key, inside the encryption.

**Why:** agents act on messages. A receiver needs proof of which machine asked, and a human reviewing an incident needs to know who said what. Signal avoids signatures to keep messages deniable; for people that is a feature, for agents it removes the audit trail.

**Revisit when:** a use case needs deniable messages between agents. The alternative is DH-derived MACs (X3DH-style authentication) instead of signatures.

## 4. The relay is untrusted for content, trusted for availability

**Decision:** the relay sees only sealed envelopes, the recipient mailbox id, sizes in 1 KiB steps, timing and IP addresses. It can drop or delay mail; it cannot read, forge or replay it without detection.

**Why:** a mailbox has to exist somewhere when both agents are not online. Making the server unable to read anything is cheap; making it unable to drop mail is not.

**Known metadata gaps:** the mailbox id is the long-term public key, so the relay can link a mailbox's traffic over time; the slot code is one token shared with all contacts. Next: a drop token per contact (revoke one contact without the others), then rotating mailbox ids.

## 3. Rules on the receiving machine, not in the relay or the model

**Decision:** each machine checks every inbound message against its contacts and per-agent rules before any agent sees it. Inbound text is labelled as coming from another agent.

**Why:** encryption proves who sent a message, not that the request is safe. A correctly signed agent can be tricked by a prompt injection. The defence is least privilege on the receiver and a human approving new contacts.

## 2. One daemon per machine; agents are names under it

**Decision:** the machine holds the keys; agents are addressed as `<machine>/<agent>`. The machine vouches for its agents' names; agents have no keys of their own.

**Why:** harnesses (pi, Claude Code, Codex) come and go per session. Per-agent keys would need storage and trust in every harness. One daemon gives one place for keys, rules and the relay connection.

**Revisit when:** agents on one machine must not trust each other.

## 1. Plain building blocks, one static binary

**Decision:** Rust, RustCrypto and dalek crates, one binary for the client, daemon, MCP server and CLI; the relay is a Cloudflare Worker in Rust. No runtime for users to install.

**Why:** agents install it themselves with one HTTPS command. Fewer moving parts means fewer ways for setup to fail.
