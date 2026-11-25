# Decisions

Each entry: the decision, the reason, and what would change it. Newest first.

## 14. Event-driven rosters and batched counters (cost)

**Problem (prod, about 20 days):** 206k Durable Object requests, about 10k a day, for about 4.9k envelopes a day: one Mailbox request per envelope and one Stats request per envelope. Most envelopes were background: every daemon sent `list` to every contact every 10 minutes (each answered with a `roster`), and pushed its roster on every change to every contact, offline ones too. One offline contact's mailbox filled up (507 `mailbox full`).

**Decision, client (0.6.1):**
- `list` goes out at daemon start (once the drop codes are synced, so it carries new ones), each time the relay is reached again after that (WSS ready, or the first HTTPS fetch that works after a loss), to a new contact (reload, invite), for a new drop code (ours, once the relay holds it, or theirs from a message we do not answer), when a person runs `letagents list`, and every 6 h ± 10 % as a safety net. Start, reach and refresh lists are bounded: none while an earlier list to that contact is unanswered (it waits in the contact's mailbox, and the contact asks when it comes back), and a reach list at most every 10 minutes per contact.
- Presence is per contact: a contact gets a push when its view of our agents (name, harness; bound agents filtered) differs from what it last got, at most one a minute (the last state wins). What each contact last got is kept in `presence.json`, so a restart pushes only changes.
- A contact we have not heard from (any verified envelope) for 30 minutes gets one push, and the next ones wait until it writes. When it comes back it sends `list` and gets the current roster.
- Two machines both online but quiet for 30 minutes would each count the other as silent. So a daemon answers an unsolicited roster with its own (or a `list`, if the contact may not see ours) when it has sent that contact nothing for 15 minutes; the contact hears it, and its pushes go on. The answer waits 3 s and is never sent before the start round, so stored rosters that come just before a relay login are covered by that login's `list` instead.
- A roster older than the one we keep (it waited in the mailbox) is ignored.

**Decision, relay:** a mailbox counts accepted envelopes in its own meta (`sent_p`: `<hour>:<n>` pairs, at most 48 hours) and sends them as `sent_h=<hour>:<n>` with its next Stats call: when Stats is called anyway (hourly online marker, first login), on an event it handles (send, login, ack, drop table) at most every 10 minutes, and always on close, error and alarm. No timer or alarm wakes a mailbox for the counters. Stats adds each count to the hour it happened in, so the chart stays right after a late flush; still no ids.

**Measured (tests/cost.sh, in CI):** two local relays with three daemons each (a, b, c, all contacts), one hour of use in 3 minutes (`LETAGENTS_TEST_SPEEDUP=20`). Base: the 0.6.0 client with the same speed-up and the relay with `STATS_FLUSH_MS=0` (one Stats request per envelope, as before). Agents change every 20 minutes on a and on b, b writes to a every 10 minutes, c is offline for 40 minutes. Billed requests (incoming WebSocket messages at 20:1, as Cloudflare counts them):

| scenario (one hour) | base | new | cut |
|---|---|---|---|
| default | 185 | 48 | 74 % |
| no messages, only presence | 157 | 33 | 78 % |
| agents change every 6 minutes | 239 | 70 | 70 % |

Counting every DO event 1:1 (WebSocket acks too): 269 → 77 (71 %) in the default scenario. Stats requests fell from 90 to 12. Envelopes sent by users (and their receipts) are not cut: the more of them, the smaller the cut. Prod traffic was mostly background, so expect about 80 % there. The same run checks that an offline contact gets the current roster when it comes back, that rosters converge after a quiet spell, and that `/stats.json` totals match the accepted envelopes after the late flushes; `tests/e2e.sh` checks that a new agent reaches a contact within 60 s.

**Keep true:** nothing new on the wire and nothing new for the relay: it sees fewer envelopes, and counts without ids. Test-only knobs (`LETAGENTS_TEST_SPEEDUP`, `STATS_FLUSH_MS`, `LA_COUNT_DO`) are off unless set.

**Revisit when:** a contact must see a change within a minute after a quiet spell longer than 30 minutes on both sides (the echo handles it, but costs one envelope per spell), contacts run clients that never answer an unsolicited roster or a `list` (they stay silent: one push per spell), or Cloudflare bills WebSocket messages 1:1 (then batch acks, which needs a wire change).

## 13. Bind a contact to one local agent (local rules with an outbound part)

**Decision:** `--agent <agent>` on `join`, `invite`, `contact add` and `contact accept`, and `contact bind <contact> <agent>` for a contact that exists, bind a contact to one local agent. The binding writes the contact's inbound rules (`* -> <agent>: send,ask,reply`, then `* -> *: nothing`, so no `list` and no roster) and adds an outbound check in the daemon: the bound agent may send, ask and reply only to that contact, on every path (`daemon.sock`, `letagents mcp`, the pi-intercom adapter). Other contacts cannot reach a bound agent and do not see it in rosters. An invite keeps its binding (`invite_binds.json`, by code hash) and applies it in the same write that adds the guest. `--note` keeps a reminder per contact, shown in its labels (`[letagents · verified x/alice · note: external engineer]`); it is not a control. Nothing goes on the wire: the relay and the contact learn nothing.

**Default: not exclusive.** Other local agents may still write to the bound contact; `--exclusive` stops that. Why: the binding is there to box the agent that reads the guest's text, and the guest's text reaches only that agent. The owner's other agents never see it, and they often must brief or follow up with the guest. `--exclusive` could not stop them anyway (see the boundary), so a default that promised it would claim more than it gives.

**Replies:** a reply to an ask that this machine sent to the same contact skips per-agent rules (the allow list still applies). Without this, a not-exclusive binding would let `planner` ask the guest and then refuse the answer (`* -> *: nothing`). The reply goes only to the agent that asked, so it opens nothing new. A reply that names an ask sent to another contact is now refused.

**Boundary:** the daemon trusts every process of the user (decision #2), and `--as` is a name that the caller gives. An agent that can run commands can call `letagents send --as <another agent>`, and a pi agent with pi-intercom can write to other pi sessions without the daemon. So the binding is a hard boundary only when the bound agent's harness cannot run commands. The recipe in web/agents.md ("Give a guest one agent"): no shell, no pi-intercom, `letagents mcp --as guide` as the only channel, woken by MCP Events (pi-mcp-events).

**Keep true:** bindings and notes stay in `contacts.json`; no wire field. Every local send path checks the binding before it seals anything. One agent serves one contact.

**Revisit when:** agents must not trust each other on one machine (then: per-agent socket credentials, decision #2), one guide must serve several contacts, or a harness can prove which session calls `letagents mcp`.

## 12. A drop code per contact; `remove` revokes it at the relay

**Decision:** each machine gives each contact its own drop code. The relay keeps SHA-256 of each code (at most 64 per mailbox, sorted, no names) next to the shared code's hash; the owner replaces the table with a signed, newer-`ts` request. Codes travel only inside sealed messages (`drop`, until the contact confirms with `drop_ok`), in a hello's `from`, and in invites: every invite carries a code of its own, which becomes the joiner's code. `letagents remove <contact>` (`contact rm`) deletes the contact's code at the relay, so the relay refuses its sends with 404 and stores nothing.

**Migration rule:** an existing contact gets its code on the next exchange (the daemon's `list` at start, for a new code, and in the 6-hourly refresh carries it; see #14), and confirms with `drop_ok` in its answer. The shared code stays valid while any contact has not confirmed (older versions never do), and for new contacts added by a bare `whoami` address. Removing a contact replaces the shared code (the removed contact may know it): at once if every remaining contact has its own code, else as soon as the last one confirms or is removed; `remove` names the contacts that hold it up.

**Why not drop the shared code as soon as every contact has moved:** then a `whoami` address would stop working, a fresh machine (no contacts) would have no working bare address at all, and only invites could connect machines. Replacing it on removal gives the same protection against removed contacts and keeps `contact add <address>` working.

**Cost:** the relay can now tell a mailbox's senders apart by the code they use (not who they are). A removed contact on an old version keeps the shared code until it is replaced. At most 64 codes (contacts plus open invites); past that, new contacts use the shared code and `invite` fails until one is freed.

**Keep true:** the relay never gets a code in clear except as a send header, never a name, and never a mapping from code to contact. The table stays bounded. A code is handed out only after the relay holds it.

**Revisit when:** a user needs more than 64 contacts per machine, sender linkability inside a mailbox matters (then: anonymous tokens, which need new crypto), or bare addresses are no longer needed (then: drop the shared code once every contact has moved).

## 11. Public stats: machines online and messages per hour, nothing else

**Decision:** `/stats.json` and the home page show machines online now, sealed messages relayed per hour, and peak machines online per hour, for 30 days. The day and week tables of v0.5.0 are gone. A machine is a mailbox with an open WebSocket. Each online mailbox keeps the hour it last confirmed it (`live_h`); the Stats object keeps a count per such hour and sums the last 24 h. A mailbox confirms only on its own events (login, envelope, close, alarm) and in the same single Stats call it already makes. Stats answers are cached at the edge for 60 s.

**Why:** a live count needs closes, and closes get lost (a deploy drops every socket without close events). A plain +1/-1 gauge drifts up forever. A per-connection lease would need renewals, and renewals would wake hibernated mailboxes on every ping. With the hour marker, reconnects after a deploy do not count twice, and a lost close ages out in 24 h without any timer.

**Keep true:** no mailbox id, IP or connection id in the Stats object; at most 720 hourly rows and 25 online rows; no wire field about agents (the relay cannot and must not count them); at most one Stats call per connect, disconnect or envelope.

**Revisit when:** a machine can stay connected for over 24 h with no login, envelope or reconnect often enough to undercount (then: lengthen the window), or the counters need a breakdown that would need ids.

## 10. MCP Events in `letagents mcp` (supersedes the wait in #7)

**Decision:** `letagents mcp` implements MCP Events per accepted SEP-3415 (commit 4165cc2), for one event, `message.received`, in all three modes: `events/poll`, `events/stream` (stdio notifications) and `events/subscribe` (Standard Webhooks). The source is the daemon's `peek` op: messages waiting for the agent, not removed. Events live in an in-memory ring buffer; a cursor is `<process epoch>.<seq>`. Webhook grants are finite (1 h default and cap, `ttlMs: null` too), because nothing is persisted. Stdio only, so the Streamable HTTP `Mcp-Name` rules do not apply.

**Verified:** conformance#521 passes: discovery, poll, push, webhook, webhook-delivery: 122 checks passed (15, 31, 26, 25, 25), 0 failed (pinned `76fef32`, in `tests/e2e.sh`). The two `Mcp-Name` rows are counted as not applicable. The suite runs over a test-only HTTP-to-stdio bridge, with `LETAGENTS_EVENTS_CONFORMANCE=1` (test controls and fixture event types, off by default).

**Deviations, for reach:** more people can use letagents if clients written before the accepted SEP work too.
1. The top-level `capabilities.events` key is sent next to `capabilities.extensions["io.modelcontextprotocol/events"]`.
2. For a client whose `clientInfo.name` is `ai-sdk-mcp-client`, an event delivery's `webhook-id` is its `eventId` (the SEP says MUST NOT). Every other client, and every control envelope, gets a random `webhook-id`.

@ai-sdk/mcp ≤2.0.99 reads the old key and ties webhook-id to eventId; re-add its check when it follows the accepted SEP. Until then `tests/aisdk` checks this compatibility mode. **Remove both deviations** when an @ai-sdk/mcp release reads the `extensions` key and accepts a separate `webhook-id`.

**Why:** clients already ship the webhook mode (ChatGPT, Vercel AI SDK), and the stdio push mode fits local agents. Waiting for every harness would leave the event-capable ones polling.

**Keep true:** events are metadata unless the subscriber asks for `include_text`; the message stays in the inbox; webhooks go out from the user's machine only (https, public addresses, no redirects, verification challenge first). The relay never sees events.

**Revisit when:** the extension's error codes (`-32023` to `-32027`) or methods change, a client needs no-expiry or long subscriptions (then persist them), or @ai-sdk/mcp follows the accepted SEP (then remove the deviations).

## 9. One-time invite codes replace the approval step

**Decision:** `letagents invite` adds a one-time code to the address (`la1.<sig>.<box>.<drop>.<code>`). The code works once, for 7 days. A `hello` that carries it is accepted at once; no `contact accept`. The inviter stores only SHA-256 of the code. A hello without a valid code still needs a human to accept it. Decision 3 still holds for everything after the contact exists.

**Why:** the human who passes an invite to the other agent has already chosen that contact; asking again added a turn and no safety. A bare address cannot do this job: it is permanent and the same for every contact, so a hello proves only that the sender saw it once. A one-time, short-lived code proves the sender got this invite. If someone else uses it first, the real joiner's hello falls back to a request, so the theft is visible.

Setup is one line per machine: `install.sh | sh -s -- invite` on one side (keys, daemon, invite) and `... -- join <name> <invite>` on the other (keys, contact, hello with the code, daemon).

**Keep true:** the code travels only inside the sealed hello, never to the relay in clear. Each invite is single-use.

**Revisit when:** one invite must admit a group of agents (then: a code with a use count), or invites go through channels humans do not control (then: require the safety number check again).

## 8. Never guess the receiving agent

**Decision:** the daemon has no default agent for mail. A send or ask must name an agent (`<machine>/<agent>`), continue a conversation (an agent there wrote to the sender in the last hour), or match the contact's configured `--session`. Anything else is refused with `name an agent`. A reply goes to the asker by ask id; the link is stored in `spool.json` for 48 h, so it survives a timeout and a restart. A reply with no known ask, and a CLI call with no agent name, use the machine inbox (`inbox`), which is never pushed. `daemon --target` became `--notify`: it only gets contact requests.

**Why:** `--target` sent every unnamed message into one live session. Notes meant for other sessions (a note from `devbox/planner`, a reply from `devbox/docs-bot`) landed in the wrong agent's context. A misrouted message is worse than a refused one: the wrong agent may act on it, and the sender thinks it arrived. A refusal tells the sender at once how to fix the address.

**Revisit when:** a machine runs one agent only. Then `contact add --session <agent>` gives it a default per contact, without a global one.

## 7. No hosted wake-up event adapter yet (stdio MCP is not broken)

**Decision:** this change adds no event adapter for waking hosted agents. The stdio MCP server stays request/response tools (`letagents_list` / `send` / `ask` / `inbox` / `reply`), with the existing push paths unchanged: Claude Code sessions get messages posted into their session inbox socket (`claude_watch`), and Codex sessions get a new turn started by the daemon.

**Why:** a stdio MCP server can only notify a harness that is already running it. Waking a hosted agent that is not running needs an out-of-band channel the daemon does not have — a webhook, a long-poll/SSE endpoint, or MCP Streamable HTTP with server-sent events — and that is a new network-facing surface, not a fix to stdio MCP. It raises questions this change does not answer: who may call the endpoint, what the daemon is allowed to send to a third-party URL (any payload carrying message text would hand plaintext to that URL's operator), and how a hosted platform authenticates the wake-up. The HTTPS polling fallback added for the relay inbox already covers the case where the live websocket cannot connect; it does not wake a process that is not running.

**Revisit when:** a hosted platform's wake-up contract is known. The minimal adapter would be a daemon-side, opt-in, metadata-only webhook (sender, kind, message id, timestamp — no body, off by default, user-configured URL only) or a Streamable HTTP MCP endpoint with SSE events, plus a receiver on the hosted side that fetches the inbox through the daemon. Until then, hosted agents should poll `letagents check` / `letagents_inbox` on their own schedule.

**Status (Nov 2026):** MCP Events is a draft extension (SEP-3415): `events/list`, `events/poll`, `events/stream` (stdio: `notifications/events/event`) and webhooks. ChatGPT supports webhooks only, which needs a public server that can read messages: not for us. Claude Code has its own Channels (research preview, custom channels need `--dangerously-load-development-channels`); our socket push needs no flag. No harness we push into supports the draft. **Build when** a harness we support (Claude Code, Codex, Cursor, Gemini CLI, OpenCode) ships `events/stream` over stdio: add `events/list`, `events/poll` and `events/stream` to `letagents mcp`, fed by the daemon's watch connection.

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

**Known metadata gaps:** the mailbox id is the long-term public key, so the relay can link a mailbox's traffic over time. The drop code was one token shared with all contacts; since #12 each contact has its own (revoke one contact without the others), which also lets the relay tell senders apart. Next: rotating mailbox ids.

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
