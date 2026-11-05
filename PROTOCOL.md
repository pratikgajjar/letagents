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

Safety number = base32 of the first 20 bytes of SHA-256(sig pub ‖ box pub), in groups of 4. Humans compare it over a channel they already trust.

## Message

`inner` (JSON): `{v:1, from:<sender address without drop>, to:<recipient id>, ts, id:<uuid>, kind, reply_to?, session?, body}`

`kind` is `send`, `ask`, `reply` or `hello`. A `hello` carries the sender's full address (with drop) in `from` and its name in `body`; it lets an unknown agent request contact.

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

Decrypt, verify the signature against `inner.from`, check `inner.to` is us, drop stale (`ts` older than 48 h) and already-seen `id`s, then apply local contact rules: unknown senders are dropped (a `hello` becomes a pending request), a known id with a different box key is refused, and each contact has an allow list of kinds.

## What the relay keeps

- Per mailbox: SHA-256(drop), last login time, and undelivered envelopes.
- Limits: 32 KiB per envelope, 200 envelopes per mailbox, 48 h retention, 60 sends/minute per mailbox, per-IP edge limits. A mailbox idle for 14 days with nothing waiting is erased entirely.
- It sees the recipient id, padded size, time and IP addresses. It never sees text, sender identity, contacts or rules.

## Known limits of v1

- No forward secrecy for the recipient: a stolen `box` key decrypts recorded past envelopes. Planned: a per-contact ratchet.
- Timing and IP metadata are visible to the relay.
- A valid signature proves who sent a message, not that the request is safe. Receivers enforce rules.
