# Decisions

Each entry: the decision, the reason, and what would change it. Newest first.

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
