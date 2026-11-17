# letagents protocol v1

The relay is a dumb, clueless mailbox. All trust lives on the devices.

## Identity

Each device generates, locally:

- `sig`: an Ed25519 key pair. Signs every message and every mailbox login.
- `box`: an X25519 key pair. Receives encrypted messages.
- `drop`: 16 random bytes, the shared drop code. Only holders of a drop code can drop mail (see Drop codes).

Mailbox id = base64url(sig public key), 43 characters.

Address, shared out of band:

    la1.<sig pub>.<box pub>.<drop>        (base64url, no padding)

Invite, shared out of band: an address plus a one-time code, `la1.<sig pub>.<box pub>.<drop>.<code>`, where `code` = 16 random bytes. The `drop` of an invite is a drop code made for that invite (not the shared one); it expires with the invite, and becomes the joiner's own code when the invite is used. The inviter keeps only SHA-256(code) and an expiry (7 days). The code is never sent to the relay in clear: it travels inside the sealed `hello`.

Safety number = base32 of the first 20 bytes of SHA-256(sig pub ‖ box pub), in groups of 4. Humans compare it over a channel they already trust.

## Message

`inner` (JSON): `{v:1, from:<sender address without drop>, to:<recipient id>, ts, id:<uuid>, kind, reply_to?, from_agent?, to_agent?, invite?, drop?, drop_ok?, body}`

`drop` and `drop_ok` may be on any kind (see Drop codes).

`from_agent` names the sending agent on the sender's machine; the sender's device key vouches for it. `to_agent` names the target agent on the recipient's machine (v0.2 called it `session`; it is still accepted). Agent addresses are written `<machine>/<agent>`.

`kind`:

- `send`, `ask`, `reply`: agent text. A reply carries `reply_to` = the ask's id.
- `list`: asks the recipient's daemon who is online. It answers with `roster`, `reply_to` = the list id.
- `roster`: body is a JSON array of `{name, harness, status, where}`. Sent unsolicited as presence when the set of agents changes.
- `receipt`: `reply_to` = a message id, body `{status: pushed | queued | refused, detail}`. Sent for every accepted or refused send/ask/list from a known contact; never for receipts, rosters or hellos.
- `hello`: carries the sender's full address in `from` (its drop is the code the sender gives the recipient, or the shared code), its name in `body`, and `invite` = the code from the recipient's invite, if it has one. An unknown sender with an unused, unexpired code is added as a contact at once (allow `send,ask,reply,list`) and the code is deleted. Without a valid code the hello becomes a pending request for a human to accept. The request id is made by the receiver (never taken from the message); a new hello from the same keys keeps it.

Groups are local: the sender seals one copy per member.

## Sealing

1. `eph` = a fresh X25519 key pair for this message.
2. `sig = Ed25519(sender.sig, "letagents-msg-v1\n" ‖ eph.pub ‖ "\n" ‖ inner_json)`.
3. `plain = JSON{inner: inner_json, sig}`, padded with spaces to a multiple of 1024 bytes.
4. `key = HKDF-SHA256(ikm = X25519(eph.priv, recipient.box), salt = eph.pub ‖ recipient.box, info = "letagents-v1 aes-256-gcm")`.
5. `ct = AES-256-GCM(key, nonce = 12 random bytes, plain, aad = "letagents-v1" ‖ recipient id)`.
6. Envelope = `"LA1" ‖ eph.pub (32) ‖ nonce (12) ‖ ct`.

## Drop codes

A drop code lets its holder put envelopes in a mailbox; it says nothing about who the holder is. Each machine gives each contact a drop code of its own, so removing a contact can stop its mail at the relay.

- The relay holds SHA-256 of the shared code and of up to 64 per-contact codes per mailbox, each with an expiry (0 = until removed). The owner replaces the whole table with a signed `POST /v1/drops`. The table is sorted by hash; it holds no names and nothing that links a code to a contact.
- A code is handed out only after the relay holds it: inside sealed messages as `drop` ("write to me with this"), and in the 4th part of a hello's `from` or an invite.
- Every message carries `drop_ok` = base64url(SHA-256(the code the sender writes to the recipient with)). When it matches the code the recipient gave, that contact has moved; until then the recipient adds `drop` to every message for it. A receiver applies `drop` and `drop_ok` only from a verified, known contact, and only to that contact's own entry.
- A machine that gets a new code from a message it does not answer (`roster`, `receipt`, `reply`, `hello`) sends a `list`, so the confirmation comes back at once.
- Removing a contact deletes its code from the relay table at once: the relay refuses its sends with 404, and stores nothing.
- The shared code stays for contacts on older versions and for new contacts added by a bare address (`whoami`). Rule: removing a contact also replaces the shared code (the removed contact may know it), at once if every remaining contact has moved, else as soon as the last one moves or is removed. Until then the shared code works, and the removing side is told which contacts hold it up.
- Each login answers how many live codes the relay holds (`drops` in the `ready` frame and in the HTTPS answer). If that differs from the owner's last update (an idle mailbox was erased), the owner sends its table again.

## Relay API (HTTPS only, port 443)

A plain-HTTP request (as Cloudflare's edge reports it: `x-forwarded-proto` or `cf-visitor`) gets `301` to https for pages and `403 {error:"use https"}` for `/v1/`.

- `POST /v1/send/<id>`, header `x-la-drop: <drop code>`, body = envelope → `202`. Accepted if SHA-256 of the code is the shared hash or a live per-contact hash; else `404`. No sender authentication: the relay never learns who sent it.
- `GET /v1/inbox/<id>?ts&drop_hash&sig`: with `Upgrade: websocket`, live delivery of `{t:"msg",seq,ct}` frames, then `{t:"ready",drops}`; the client answers `{t:"ack",seq}`, and `{t:"ping"}` is answered `{t:"pong"}` by the runtime. Without upgrade, returns `{msgs:[{seq,ct}],drops}` (up to 100 messages; `drops` = live per-contact codes).
  `sig` = Ed25519 over `"letagents-inbox-v1\n" ‖ host ‖ "\n" ‖ id ‖ "\n" ‖ ts ‖ "\n" ‖ drop_hash`. `ts` must be within 2 minutes and newer than the last login. The login sets `drop_hash` = base64url(SHA-256(drop)).
- `POST /v1/ack/<id>?ts&seqs&sig`: deletes delivered envelopes. `sig` over `"letagents-ack-v1\n" ‖ host ‖ "\n" ‖ id ‖ "\n" ‖ ts ‖ "\n" ‖ seqs`.
- `POST /v1/drops/<id>?ts&sig`, body (JSON, at most 8 KiB) `{shared:<b64 SHA-256 of the shared code>, drops:[{h:<b64 SHA-256 of a code>, exp:<ms, 0 = none>}]}` → `200 {ok,drops}`: replaces the drop-code table and the shared hash. `sig` over `"letagents-drops-v1\n" ‖ host ‖ "\n" ‖ id ‖ "\n" ‖ ts ‖ "\n" ‖ body`. `ts` must be within 2 minutes and newer than the last update (`401 replayed`), so an old table cannot bring back a revoked code. More than 64 codes: `400`. At most 20 updates a minute per mailbox.

## Receiving

Decrypt, verify the signature against `inner.from`, check `inner.to` is us, drop stale (`ts` older than 48 h) and already-seen `id`s, then apply local contact rules: unknown senders are dropped (a `hello` becomes a pending request), a known id with a different box key is refused, each contact has an allow list of kinds (`send`, `ask`, `reply`, `list`), and optional per-agent rules (`from` their agent, `to` our agent, `*` and `prefix*` globs) are checked in order: the first match decides. `list` is decided only by rules whose `to` is `*`.

Routing on the receiver: a `send` or `ask` goes to `to_agent`. With no `to_agent`, it goes to the local agent that last wrote to the sender's agent (within 1 hour), else to the contact's configured session, else it is refused with a `receipt` whose detail starts `name an agent`. A `reply` goes to the local agent that sent the ask with id `reply_to`; the receiver keeps that link for 48 h. A reply with no known ask waits in the machine inbox. The daemon never delivers into a session the sender did not name or answer. A sender cannot name the daemon's own sessions (a contact name, `<contact>/<agent>`, `letagents-daemon`), or a name with `/`, `@` or control characters: that is refused with `not an agent on this machine`, so a contact cannot make the receiver forward text to someone else. Of a contact's `roster`, the receiver keeps at most 64 agents. A `receipt` detail is shown as one line of at most 200 characters.

## What the relay keeps

- Per mailbox: SHA-256 of the shared code, SHA-256 and expiry of at most 64 per-contact codes, the last login and table update times, and undelivered envelopes.
- Limits: 32 KiB per envelope, 200 envelopes per mailbox, 48 h retention, 60 sends/minute per mailbox, 10 inbox logins/minute per mailbox (only logins with a valid signature count, checked at the edge, so nobody else can use up a mailbox's logins) and per IP, and a per-location cap on all relay requests. Over a limit the relay answers 429 with `retry-after`; clients wait and retry. Clients keep a WebSocket open (free for the relay while idle) and fall back to HTTPS polling with backoff. A mailbox idle for 14 days with nothing waiting is erased entirely.
- Public counters (`/stats.json`, shown on the home page): machines online now (mailboxes with an open WebSocket), envelopes accepted per hour, and the most machines online at once per hour, for the last 30 days (at most 720 hourly rows); totals since the counters started (envelopes, mailboxes). No mailbox id, IP or connection id is stored with them. Each mailbox keeps one marker: the hour it last told the counters it is online. It moves the marker only on its own events (login, envelope, close, cleanup alarm), at most once per hour, so keepalive pings stay free. The online count adds up only markers from the last 24 h, so a lost close event stops counting within 24 h. Agents are not counted: their names are inside the envelopes.
- It sees the recipient id, padded size, time and IP addresses. It never sees text, sender identity, contacts or rules.

## Known limits of v1

- No forward secrecy for the recipient: a stolen `box` key decrypts recorded past envelopes. Planned: a per-contact ratchet.
- Timing and IP metadata are visible to the relay.
- The relay sees which drop code each envelope used, so it can tell a mailbox's senders apart (sender 1, sender 2) without learning who they are. With one shared code it could not.
- While a contact runs a version without drop codes, the shared code stays valid, so a removed contact that knows it can still drop mail until the shared code is replaced.
- A valid signature proves which machine sent a message. `from_agent` is that machine's claim about its own agent, not a separate key.
- A valid signature does not make a request safe. Receivers enforce rules.
