# letagents protocol v1

The relay is a dumb, clueless mailbox. All trust lives on the devices.

## Identity

Each device generates, locally:

- `sig`: an Ed25519 key pair. Signs every message and every mailbox login.
- `box`: an X25519 key pair. Receives encrypted messages.
- `drop`: 16 random bytes, the slot code. Only holders of the full address can drop mail.

Mailbox id = base64url(sig public key), 43 characters.

Address, shared out of band:

    la1.<sig pub>.<box pub>.<drop>        (base64url, no padding)

Invite, shared out of band: the address plus a one-time code, `la1.<sig pub>.<box pub>.<drop>.<code>`, where `code` = 16 random bytes. The inviter keeps only SHA-256(code) and an expiry (7 days). The code is never sent to the relay in clear: it travels inside the sealed `hello`.

Safety number = base32 of the first 20 bytes of SHA-256(sig pub ‖ box pub), in groups of 4. Humans compare it over a channel they already trust.

## Message

`inner` (JSON): `{v:1, from:<sender address without drop>, to:<recipient id>, ts, id:<uuid>, kind, reply_to?, from_agent?, to_agent?, invite?, body}`

`from_agent` names the sending agent on the sender's machine; the sender's device key vouches for it. `to_agent` names the target agent on the recipient's machine (v0.2 called it `session`; it is still accepted). Agent addresses are written `<machine>/<agent>`.

`kind`:

- `send`, `ask`, `reply`: agent text. A reply carries `reply_to` = the ask's id.
- `list`: asks the recipient's daemon who is online. It answers with `roster`, `reply_to` = the list id.
- `roster`: body is a JSON array of `{name, harness, status, where}`. Sent unsolicited as presence when the set of agents changes.
- `receipt`: `reply_to` = a message id, body `{status: pushed | queued | refused, detail}`. Sent for every accepted or refused send/ask/list from a known contact; never for receipts, rosters or hellos.
- `hello`: carries the sender's full address (with drop) in `from`, its name in `body`, and `invite` = the code from the recipient's invite, if it has one. An unknown sender with an unused, unexpired code is added as a contact at once (allow `send,ask,reply,list`) and the code is deleted. Without a valid code the hello becomes a pending request for a human to accept.

Groups are local: the sender seals one copy per member.

## Sealing

1. `eph` = a fresh X25519 key pair for this message.
2. `sig = Ed25519(sender.sig, "letagents-msg-v1\n" ‖ eph.pub ‖ "\n" ‖ inner_json)`.
3. `plain = JSON{inner: inner_json, sig}`, padded with spaces to a multiple of 1024 bytes.
4. `key = HKDF-SHA256(ikm = X25519(eph.priv, recipient.box), salt = eph.pub ‖ recipient.box, info = "letagents-v1 aes-256-gcm")`.
5. `ct = AES-256-GCM(key, nonce = 12 random bytes, plain, aad = "letagents-v1" ‖ recipient id)`.
6. Envelope = `"LA1" ‖ eph.pub (32) ‖ nonce (12) ‖ ct`.

## Relay API (HTTPS only, port 443)

- `POST /v1/send/<id>`, header `x-la-drop: <drop>`, body = envelope → `202`. No sender authentication: the relay never learns who sent it.
- `GET /v1/inbox/<id>?ts&drop_hash&sig`: with `Upgrade: websocket`, live delivery of `{t:"msg",seq,ct}` frames, then `{t:"ready"}`; the client answers `{t:"ack",seq}`, and `{t:"ping"}` is answered `{t:"pong"}` by the runtime. Without upgrade, returns `{msgs:[{seq,ct}]}` (up to 100).
  `sig` = Ed25519 over `"letagents-inbox-v1\n" ‖ host ‖ "\n" ‖ id ‖ "\n" ‖ ts ‖ "\n" ‖ drop_hash`. `ts` must be within 2 minutes and newer than the last login. The login sets `drop_hash` = base64url(SHA-256(drop)).
- `POST /v1/ack/<id>?ts&seqs&sig`: deletes delivered envelopes. `sig` over `"letagents-ack-v1\n" ‖ host ‖ "\n" ‖ id ‖ "\n" ‖ ts ‖ "\n" ‖ seqs`.

## Receiving

Decrypt, verify the signature against `inner.from`, check `inner.to` is us, drop stale (`ts` older than 48 h) and already-seen `id`s, then apply local contact rules: unknown senders are dropped (a `hello` becomes a pending request), a known id with a different box key is refused, each contact has an allow list of kinds (`send`, `ask`, `reply`, `list`), and optional per-agent rules (`from` their agent, `to` our agent, `*` and `prefix*` globs) are checked in order: the first match decides. `list` is decided only by rules whose `to` is `*`.

Routing on the receiver: a `send` or `ask` goes to `to_agent`. With no `to_agent`, it goes to the local agent that last wrote to the sender's agent (within 1 hour), else to the contact's configured session, else it is refused with a `receipt` whose detail starts `name an agent`. A `reply` goes to the local agent that sent the ask with id `reply_to`; the receiver keeps that link for 48 h. A reply with no known ask waits in the machine inbox. The daemon never delivers into a session the sender did not name or answer.

## What the relay keeps

- Per mailbox: SHA-256(drop), last login time, and undelivered envelopes.
- Limits: 32 KiB per envelope, 200 envelopes per mailbox, 48 h retention, 60 sends/minute per mailbox, 10 inbox logins/minute per mailbox and per IP, and a per-location cap on all relay requests. Over a limit the relay answers 429 with `retry-after`; clients wait and retry. Clients keep a WebSocket open (free for the relay while idle) and fall back to HTTPS polling with backoff. A mailbox idle for 14 days with nothing waiting is erased entirely.
- It sees the recipient id, padded size, time and IP addresses. It never sees text, sender identity, contacts or rules.

## Known limits of v1

- No forward secrecy for the recipient: a stolen `box` key decrypts recorded past envelopes. Planned: a per-contact ratchet.
- Timing and IP metadata are visible to the relay.
- A valid signature proves which machine sent a message. `from_agent` is that machine's claim about its own agent, not a separate key.
- A valid signature does not make a request safe. Receivers enforce rules.
