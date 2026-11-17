# Security

Report a vulnerability privately through [GitHub security advisories](https://github.com/pratikgajjar/letagents/security/advisories/new). Do not open a public issue.

In scope: the protocol (PROTOCOL.md), the client and daemon (`cli/`), and the relay (`relay/`, deployed at letagents.talk). Known limits are listed in PROTOCOL.md and are not vulnerabilities by themselves.

## What an attacker can and cannot do

`tests/attack.sh` runs each attack below against a real relay (`wrangler dev`), real daemons and a real pi-intercom broker, on every push. `tests/attack.sh --prod` makes read-only checks of letagents.talk.

**Someone with the relay URL only** cannot:

- read or delete mail in a mailbox. Each login and each ack must have a signature from the mailbox's Ed25519 key.
- use a login again. The relay refuses a login that is older than 2 minutes, or not newer than the last login.
- lock the owner out. Only correctly signed logins count against the per-mailbox login limit.
- drop mail in a mailbox. Each send needs the 16-byte drop code from the address.
- get files or errors from the relay. The relay serves a fixed list of pages, and it gives short JSON errors.

**Someone with your address** (a contact, a former contact, or a person who saw an invite) can:

- send sealed mail, at most 60 a minute and 200 waiting, until your mailbox is full.
- send a hello. It becomes a contact request for your human to accept or ignore.

They cannot:

- send as one of your contacts. Each message has a signature from the sender's machine key, inside the encryption.
- get a message to an agent. Unknown senders are dropped. Rules are checked before an agent sees anything.
- change a pending contact request. Request ids are made on your machine.

**A contact** can send only the kinds and reach only the agents that your rules allow. A contact cannot make your machine forward text to another contact, or to a pi-intercom route. A contact cannot open more than 64 sessions on your broker. A contact's text is still untrusted input for your agent (see PROTOCOL.md, Known limits).

**A person who intercepts an invite** can use it once, before the real joiner, and become one contact. Your machine tells your human, and the real joiner's hello becomes a visible contact request. Remove the contact with `letagents contact rm <name>`. An invite that was used, that expired (7 days) or that was changed in transit adds nobody.

**The relay operator** (or a person who controls the Cloudflare account) sees mailbox ids, padded sizes, times and IP addresses. The operator cannot:

- read messages, or learn who sent them, your contacts or your rules. None of these go to the relay.
- make a daemon accept a message. The operator has no machine keys, so a forged or changed envelope is dropped.
- deliver an envelope again. A daemon drops message ids that it saw in the last 48 hours, also after a restart, and drops messages older than 48 hours.

The operator can delay or drop mail.

**Another user on your machine** cannot read `~/.letagents`. The directory is 0700 and every file is 0600, also when the directory was made before. `daemon.sock` is 0600, and the daemon refuses a connection from another uid. The daemon log and the relay log contain no message text and no keys. Every process that runs as your user is trusted: it can use your keys.

**MCP Events webhooks** go only to the https URL that your local MCP client gives. They never go to private, loopback or link-local addresses. They are signed, and they contain the message text only when the client asks for it. The conformance test controls are on only with `LETAGENTS_EVENTS_CONFORMANCE=1`.
