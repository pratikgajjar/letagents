// la.mjs: letagents with nothing installed. One file, no dependencies: WebCrypto + fetch (Node >= 20, Bun, Deno).
//
//   curl -fsSL https://letagents.talk/la.mjs | node - <command> [args]
//
// Same wire format as the letagents binary (https://letagents.talk/protocol.md) and the same state files in
// $LETAGENTS_HOME or ~/.letagents, so both tools can share one identity. No daemon: mail is fetched over HTTPS
// when you run `inbox`, `ask` or `list`. Written so it runs both as an ES module and as stdin (CommonJS).

const USAGE = `usage: node la.mjs <command>
  init <name>                         create this machine's keys
  whoami                              show address and safety number
  invite                              address + one-time code (7 days): its user is added at once
  join <name> <invite> [--me <name>]  keys if needed, add the inviter, send hello with its code
  contact add <name> <address|invite> [--allow send,ask,reply,list]
  contact ls | requests | contact accept <request-id> <name>
  hello <contact>                     send our address so they can add us
  send <machine>[/<agent>] <text>     note (flags: --as <your agent name>)
  ask <machine>[/<agent>] <text>      ask and wait for the reply (--timeout 60)
  inbox [--wait <s>]                  fetch, verify and print messages as JSON lines
  reply <id> <text>                   answer an ask from the inbox
  list <contact>                      who is online on that machine`;

const MSG = "letagents-msg-v1\n", INFO = "letagents-v1 aes-256-gcm", AAD = "letagents-v1";
const KINDS = ["send", "ask", "reply", "list", "roster", "receipt", "hello"], RULE_KINDS = KINDS.slice(0, 4);
const MAX_AGE = 48 * 3600 * 1000 + 300_000;
const PKCS8 = { Ed25519: "302e020100300506032b657004220420", X25519: "302e020100300506032b656e04220420" };
const env = process.env;
const HOME = env.LETAGENTS_HOME || `${env.HOME}/.letagents`;
const RELAY = (env.LETAGENTS_RELAY || "https://letagents.talk").replace(/\/$/, "");
const HOST = env.LETAGENTS_AUTH_HOST || new URL(RELAY).host;
const subtle = globalThis.crypto.subtle;
let fs; // node:fs, loaded in main()

// ---------- bytes ----------
const te = (s) => new TextEncoder().encode(s);
const cat = (...a) => {
  const o = new Uint8Array(a.reduce((n, x) => n + x.length, 0));
  a.reduce((i, x) => (o.set(x, i), i + x.length), 0);
  return o;
};
const b64 = (b) => {
  let s = "";
  for (const x of b) s += String.fromCharCode(x);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
};
const unb64 = (s, n) => {
  const b = Uint8Array.from(atob(s.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0));
  if (n && b.length !== n) throw new Error(`expected ${n} bytes`);
  return b;
};
const hex = (h) => Uint8Array.from(h.match(/../g), (x) => parseInt(x, 16));
const rand = (n) => globalThis.crypto.getRandomValues(new Uint8Array(n));
const sha256 = async (b) => new Uint8Array(await subtle.digest("SHA-256", b));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const die = (m, code = 1) => {
  console.error(m);
  process.exit(code);
};

// ---------- state (same files and formats as cli/src/store.rs) ----------
const read = (name, dflt) => {
  try {
    return JSON.parse(fs.readFileSync(`${HOME}/${name}`, "utf8"));
  } catch {
    return dflt;
  }
};
const write = (name, v) => {
  fs.mkdirSync(HOME, { recursive: true, mode: 0o700 });
  fs.writeFileSync(`${HOME}/${name}.tmp`, JSON.stringify(v, null, 2), { mode: 0o600 });
  fs.renameSync(`${HOME}/${name}.tmp`, `${HOME}/${name}`);
};

// ---------- keys and addresses ----------
const importPriv = (alg, raw) => subtle.importKey("pkcs8", cat(hex(PKCS8[alg]), raw), { name: alg }, true, alg === "Ed25519" ? ["sign"] : ["deriveBits"]);
const pubOf = async (key) => unb64((await subtle.exportKey("jwk", key)).x, 32);

// New keys; the first login creates the mailbox, so contacts can write before our first `inbox`.
async function createMe(name) {
  write("identity.json", { name, sig: b64(rand(32)), rcv: b64(rand(32)), drop: b64(rand(16)) });
  await fetchInbox(await loadMe()).catch((e) => console.error(`warning: relay not reached (${e.message}); run inbox once later`));
}
// Our keys, created on first use (name: --me, else the host name).
async function ensureMe(name) {
  if (!fs.existsSync(`${HOME}/identity.json`)) {
    const host = (await import("node:os")).hostname().split(".")[0].toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 24);
    await createMe(name || host || "machine");
    console.error(`created keys for "${name || host || "machine"}" in ${HOME} (they never leave this machine)`);
  }
  return loadMe();
}
async function loadMe() {
  const j = read("identity.json", null);
  if (!j) die(`no identity in ${HOME}; run: init <name>`);
  const sigKey = await importPriv("Ed25519", unb64(j.sig, 32));
  const rcvKey = await importPriv("X25519", unb64(j.rcv, 32));
  const me = { name: j.name, sigKey, rcvKey, sig: await pubOf(sigKey), rcv: await pubOf(rcvKey), drop: unb64(j.drop, 16) };
  me.id = b64(me.sig);
  return me;
}
const address = (p, withDrop) => `la1.${b64(p.sig)}.${b64(p.rcv)}` + (withDrop && p.drop ? `.${b64(p.drop)}` : "");
function parsePeer(addr) {
  const p = addr.trim().split(".");
  if (p[0] !== "la1" || (p.length !== 3 && p.length !== 4)) throw new Error("not a letagents address (la1.<sig>.<box>[.<drop>])");
  return { sig: unb64(p[1], 32), rcv: unb64(p[2], 32), drop: p[3] ? unb64(p[3], 16) : null, id: p[1] };
}
async function safety(p) {
  const h = (await sha256(cat(p.sig, p.rcv))).slice(0, 20);
  const A = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
  let bits = "", s = "";
  for (const x of h) bits += x.toString(2).padStart(8, "0");
  for (let i = 0; i < bits.length; i += 5) s += A[parseInt(bits.slice(i, i + 5), 2)];
  return s.match(/.{1,4}/g).join(" ");
}
const sign = async (me, msg) => b64(new Uint8Array(await subtle.sign("Ed25519", me.sigKey, typeof msg === "string" ? te(msg) : msg)));

// ---------- sealing (PROTOCOL.md "Sealing") ----------
async function aesKey(priv, pub, eph, rcv) {
  const peer = await subtle.importKey("raw", pub, { name: "X25519" }, false, []);
  const shared = await subtle.deriveBits({ name: "X25519", public: peer }, priv, 256);
  const ikm = await subtle.importKey("raw", shared, "HKDF", false, ["deriveKey"]);
  return subtle.deriveKey({ name: "HKDF", hash: "SHA-256", salt: cat(eph, rcv), info: te(INFO) }, ikm, { name: "AES-GCM", length: 256 }, false, ["encrypt", "decrypt"]);
}

async function seal(me, to, inner) {
  const eph = await subtle.generateKey({ name: "X25519" }, true, ["deriveBits"]);
  const ephPub = new Uint8Array(await subtle.exportKey("raw", eph.publicKey));
  const innerJson = JSON.stringify(inner);
  const plain = te(JSON.stringify({ inner: innerJson, sig: await sign(me, cat(te(MSG), ephPub, te("\n"), te(innerJson))) }));
  const padded = cat(plain, new Uint8Array((1024 - (plain.length % 1024)) % 1024).fill(32));
  const key = await aesKey(eph.privateKey, to.rcv, ephPub, to.rcv);
  const nonce = rand(12);
  const ct = await subtle.encrypt({ name: "AES-GCM", iv: nonce, additionalData: te(AAD + to.id) }, key, padded);
  return cat(te("LA1"), ephPub, nonce, new Uint8Array(ct));
}

async function open(me, env) {
  if (env.length < 63 || new TextDecoder().decode(env.slice(0, 3)) !== "LA1") throw new Error("bad envelope");
  const eph = env.slice(3, 35);
  const key = await aesKey(me.rcvKey, eph, eph, me.rcv);
  let padded;
  try {
    padded = await subtle.decrypt({ name: "AES-GCM", iv: env.slice(35, 47), additionalData: te(AAD + me.id) }, key, env.slice(47));
  } catch {
    throw new Error("cannot decrypt (tampered, or not for us)");
  }
  const plain = JSON.parse(new TextDecoder().decode(padded).trimEnd());
  const inner = JSON.parse(plain.inner);
  const sender = parsePeer(inner.from);
  const vk = await subtle.importKey("raw", sender.sig, { name: "Ed25519" }, false, ["verify"]);
  if (!(await subtle.verify("Ed25519", vk, unb64(plain.sig, 64), cat(te(MSG), eph, te("\n"), te(plain.inner))))) throw new Error("bad signature");
  if (inner.v !== 1 || inner.to !== me.id) throw new Error("not addressed to us");
  if (!KINDS.includes(inner.kind)) throw new Error("unknown kind");
  if (Math.abs(Date.now() - inner.ts) > MAX_AGE) throw new Error("stale message");
  inner.to_agent ??= inner.session;
  return { inner, sender };
}

// ---------- relay (PROTOCOL.md "Relay API") ----------
async function deliver(to, envelope) {
  if (!to.drop) throw new Error("contact address has no drop secret; ask them for their full address");
  const r = await fetch(`${RELAY}/v1/send/${to.id}`, { method: "POST", headers: { "x-la-drop": b64(to.drop), "content-type": "application/octet-stream" }, body: envelope });
  if (r.status !== 202) throw new Error(`relay refused: ${r.status} ${await r.text()}`);
}
// A mailbox login (PROTOCOL.md): every request signs a fresh, newer ts.
async function login(me) {
  const ts = String(Date.now()), dh = b64(await sha256(me.drop));
  return `ts=${ts}&drop_hash=${dh}&sig=${await sign(me, `letagents-inbox-v1\n${HOST}\n${me.id}\n${ts}\n${dh}`)}`;
}
// The relay allows 10 inbox/ack requests per minute per IP; on 429, wait and sign again (up to ~1 minute).
async function retry(req) {
  for (let i = 0; ; i++) {
    const r = await req();
    if (r.status !== 429 || i === 6) return r;
    await sleep(10_000);
  }
}
async function fetchInbox(me) {
  const r = await retry(async () => fetch(`${RELAY}/v1/inbox/${me.id}?${await login(me)}`));
  if (!r.ok) throw new Error(`relay refused: ${r.status} ${await r.text()}`);
  return (await r.json()).msgs;
}
async function ack(me, seqs) {
  if (!seqs.length) return;
  const list = seqs.join(",");
  const r = await retry(async () => {
    const ts = String(Date.now()), sig = await sign(me, `letagents-ack-v1\n${HOST}\n${me.id}\n${ts}\n${list}`);
    return fetch(`${RELAY}/v1/ack/${me.id}?ts=${ts}&seqs=${list}&sig=${sig}`, { method: "POST" });
  });
  if (!r.ok) throw new Error(`ack refused: ${r.status}`);
}

// ---------- contacts and rules (cli/src/store.rs) ----------
function contactOf(sender) {
  for (const [name, c] of Object.entries(read("contacts.json", {}))) {
    const p = parsePeer(c.address);
    if (p.id !== sender.id) continue;
    if (b64(p.rcv) !== b64(sender.rcv)) return { err: `contact ${name} presented a different key` };
    return { name, c };
  }
  return { err: "unknown sender" };
}
const glob = (pat, name) => (pat.endsWith("*") ? name.startsWith(pat.slice(0, -1)) : pat === name);
function authorize(sender, kind, from, to) {
  const k = contactOf(sender);
  if (k.err) return k;
  if (!k.c.allow.includes(kind)) return { ...k, err: `contact ${k.name} is not allowed to ${kind}` };
  const rules = k.c.rules || [];
  if (rules.length) {
    const r = rules.find((r) => glob(r.from, from) && (kind === "list" ? r.to === "*" : glob(r.to, to)));
    if (!r) return { ...k, err: `no rule matches ${k.name}/${from} -> ${to}` };
    if (!r.allow.includes(kind)) return { ...k, err: `rule: ${k.name}/${from} may not ${kind} ${to}` };
  }
  return k;
}
// An invite is an address with a 5th part, the one-time code: la1.<sig>.<box>.<drop>.<code>.
const splitInvite = (addr) => {
  const p = addr.trim().split(".");
  return p.length === 5 ? [p.slice(0, 4).join("."), p[4]] : [addr.trim(), undefined];
};
// invites.json: base64url(SHA-256(code)) -> expiry ms (same file as the binary). Only the hash is kept.
const INVITE_TTL_MS = 7 * 24 * 3600 * 1000;
const inviteKey = async (code) => b64(await sha256(new TextEncoder().encode(code.trim())));
async function newInvite() {
  const code = b64(rand(16)), all = Object.fromEntries(Object.entries(read("invites.json", {})).filter(([, exp]) => exp > Date.now()));
  all[await inviteKey(code)] = Date.now() + INVITE_TTL_MS;
  write("invites.json", all);
  return code;
}
async function redeemInvite(code) {
  const all = read("invites.json", {}), k = await inviteKey(code), hit = all[k] > Date.now();
  delete all[k];
  write("invites.json", Object.fromEntries(Object.entries(all).filter(([, exp]) => exp > Date.now())));
  return hit;
}
function addContact(name, full, allow) {
  if (/[@/]/.test(name)) throw new Error("contact names cannot contain @ or /");
  const [addr, invite] = splitInvite(full);
  if (!parsePeer(addr).drop) throw new Error("this address has no drop secret (4th part); ask for the full address");
  const kinds = allow.split(",").map((s) => s.trim()).filter(Boolean);
  const bad = kinds.find((k) => !RULE_KINDS.includes(k));
  if (bad) throw new Error(`allow takes ${RULE_KINDS.join(",")} (got ${bad})`);
  const all = read("contacts.json", {});
  all[name] = { ...all[name], address: addr, allow: kinds, added: Date.now(), ...(invite ? { invite } : {}) };
  write("contacts.json", all);
}

// ---------- messages ----------
async function compose(me, contact, kind, body, { replyTo, fromAgent, toAgent, full, invite } = {}) {
  const c = read("contacts.json", {})[contact];
  if (!c) throw new Error(`no contact ${contact}`);
  const to = parsePeer(c.address);
  const inner = { v: 1, from: address(me, full), to: to.id, ts: Date.now(), id: globalThis.crypto.randomUUID(), kind, reply_to: replyTo, from_agent: fromAgent, to_agent: toAgent, invite, body };
  await deliver(to, await seal(me, to, inner));
  return inner.id;
}

// Handle one stored envelope. Returns false to leave it on the relay (agent text while `want` waits for the
// answer to our message `want`, so a later `inbox` gets it); otherwise it is acked. Answers to `want` and,
// without `want`, printed agent messages go to `got`.
async function handle(me, as, want, ct, seen, got) {
  let inner, sender;
  try {
    ({ inner, sender } = await open(me, unb64(ct)));
  } catch (e) {
    return console.error(`dropped envelope: ${e.message}`), true;
  }
  if (want && ["send", "ask", "reply"].includes(inner.kind) && inner.reply_to !== want) return false;
  if (seen[inner.id]) return console.error(`dropped replay ${inner.id}`), true;
  seen[inner.id] = inner.ts;
  write("seen.json", seen);
  const fa = inner.from_agent || "agent";
  if (inner.kind === "hello") {
    const known = contactOf(sender);
    if (!known.err || known.err !== "unknown sender") return true;
    if (inner.invite && (await redeemInvite(inner.invite))) {
      const all = read("contacts.json", {}), base = inner.body.replace(/[^A-Za-z0-9_-]/g, "").slice(0, 32) || "agent";
      let name = base;
      for (let n = 2; all[name]; n++) name = `${base}-${n}`;
      all[name] = { address: address(sender, true), allow: [...RULE_KINDS], added: Date.now() };
      write("contacts.json", all);
      return console.error(`added contact ${name} (${await safety(sender)}): it used your invite code`), true;
    }
    const rid = inner.id.slice(0, 8), name = inner.body.replace(/[^A-Za-z0-9._-]/g, "").slice(0, 32), s = await safety(sender);
    write("requests.json", { ...read("requests.json", {}), [rid]: { address: address(sender, true), name, safety: s, ts: inner.ts } });
    return console.error(`contact request ${rid} from "${name}" (${s}); accept with your human's approval: contact accept ${rid} <name>`), true;
  }
  if (inner.kind === "receipt" || inner.kind === "roster") {
    const k = contactOf(sender);
    if (k.err) return true;
    if (want && inner.reply_to === want) got.push({ ...inner, contact: k.name });
    else console.error(`${inner.kind === "receipt" ? "receipt" : "presence"} from ${k.name}: ${inner.body}`);
    return true;
  }
  const to = ["list", "reply"].includes(inner.kind) ? "*" : inner.to_agent || contactOf(sender).c?.session || as;
  const k = authorize(sender, inner.kind, fa, to);
  const receipt = (status, detail) => compose(me, k.name, "receipt", JSON.stringify({ status, detail }), { replyTo: inner.id }).catch(() => {});
  if (k.err) {
    console.error(`refused ${inner.kind} ${inner.id} from ${fa}: ${k.err}`);
    if (k.name) await receipt("refused", k.err);
    return true;
  }
  if (inner.kind === "list") return await compose(me, k.name, "roster", JSON.stringify([{ name: as, harness: "la.mjs", status: "online" }]), { replyTo: inner.id }), true;
  const msg = { from: `${k.name}/${fa}`, kind: inner.kind, id: inner.id, reply_to: inner.reply_to ?? null, to: inner.to_agent ?? null, ts: inner.ts, body: inner.body };
  got.push(msg);
  if (want) return true;
  console.log(JSON.stringify(msg));
  if (inner.kind === "ask") write("asks.json", { ...read("asks.json", {}), [inner.id]: { contact: k.name, ts: inner.ts } });
  if (inner.kind !== "reply") await receipt("pushed", to);
  return true;
}

// Read the mailbox until `done(got)` or `secs` pass. Live over WSS when the runtime has WebSocket (Node >= 22,
// Bun, Deno): one login, pushed at once, acked in-band. Otherwise HTTPS polling every 10 s, because the relay
// allows 10 inbox/ack requests per minute per IP.
async function drain(me, as, want, secs, done) {
  const seen = read("seen.json", {}), got = [], end = Date.now() + secs * 1000;
  for (const k in seen) if (seen[k] < Date.now() - MAX_AGE) delete seen[k];
  if (globalThis.WebSocket && !env.LA_NO_WS) {
    const q = await login(me);
    const ok = await new Promise((resolve) => {
      let chain = Promise.resolve(), ready = false, opened = false;
      const ws = new WebSocket(`${RELAY.replace(/^http/, "ws")}/v1/inbox/${me.id}?${q}`);
      const finish = (v) => (clearTimeout(timer), ws.close(), resolve(v));
      const check = () => ready && (done(got) || Date.now() >= end) && finish(true);
      const timer = setTimeout(() => (chain = chain.then(() => finish(true))), Math.max(0, end - Date.now()) + 50);
      ws.onopen = () => (opened = true);
      ws.onerror = ws.onclose = () => resolve(opened);
      ws.onmessage = (e) =>
        (chain = chain.then(async () => {
          const f = JSON.parse(e.data);
          if (f.t === "ready") ready = true;
          if (f.t === "msg" && (await handle(me, as, want, f.ct, seen, got))) ws.send(JSON.stringify({ t: "ack", seq: f.seq }));
          check();
        }));
    });
    if (ok) return got;
  }
  for (;;) {
    const acks = [];
    for (const m of await fetchInbox(me)) if (await handle(me, as, want, m.ct, seen, got)) acks.push(m.seq);
    await ack(me, acks);
    if (done(got) || Date.now() + 10_000 > end) return got;
    await sleep(10_000);
  }
}

// Wait for the reply or roster answering our message `id`; a refused receipt ends the wait.
async function answer(me, as, id, kind, secs) {
  const got = await drain(me, as, id, secs, (g) => g.some((m) => m.kind === kind || (m.kind === "receipt" && JSON.parse(m.body).status === "refused")));
  const m = got.find((m) => m.kind === kind);
  if (m) return m;
  const r = got.find((m) => m.kind === "receipt" && JSON.parse(m.body).status === "refused");
  die(r ? `refused: ${JSON.parse(r.body).detail}` : `no ${kind} within ${secs}s`, r ? 3 : 2);
}

function splitTo(to) {
  const [contact, agent] = to.split("/");
  return { contact, agent: agent || undefined };
}

async function main() {
  fs = await import("node:fs");
  const flags = {}, pos = [];
  const argv = process.argv.slice(2);
  for (let i = 0; i < argv.length; i++) {
    if (argv[i].startsWith("--")) flags[argv[i].slice(2)] = argv[++i];
    else pos.push(argv[i]);
  }
  const [cmd, a, b] = pos;
  const as = flags.as || env.LETAGENTS_AS || "agent";
  const contacts = () => read("contacts.json", {});
  switch (cmd) {
    case "init": {
      if (!a) die(USAGE);
      if (fs.existsSync(`${HOME}/identity.json`)) die(`${HOME}/identity.json exists; refusing to overwrite keys`);
      await createMe(a);
    } // fall through: show the new identity
    case "whoami": {
      const me = await loadMe();
      return console.log(`name:          ${me.name}\naddress:       ${address(me, true)}\nsafety number: ${await safety(me)}\nrelay:         ${RELAY}`);
    }
    case "invite": {
      const me = await ensureMe(flags.me), addr = `${address(me, true)}.${await newInvite()}`;
      return console.log(`letagents invite from "${me.name}" (one use, valid 7 days; give it only to the agent you want)\naddress: ${addr}\nsafety number: ${await safety(me)}\n\nOn the other machine, paste one line:\n  curl -fsSL https://letagents.talk/install.sh | sh -s -- join ${me.name} ${addr}\nNo install (Node 20+):\n  curl -fsSL https://letagents.talk/la.mjs | node - join ${me.name} ${addr}\nThis script has no daemon: run inbox to receive the other side's hello and mail.`);
    }
    case "join": {
      if (!a || !b) die(USAGE);
      const [, code] = splitInvite(b);
      if (!code) die(`this is an address without an invite code; ask for a new invite, or use: contact add ${a} <address>`);
      const me = await ensureMe(flags.me);
      addContact(a, b, "send,ask,reply,list");
      await compose(me, a, "hello", me.name, { full: true, invite: code });
      return console.log(`joined ${a} as "${me.name}": it adds you when it reads the hello (code: one use, 7 days).\nsafety number of ${a}: ${await safety(parsePeer(splitInvite(b)[0]))}\nsend:  node la.mjs send ${a}/<agent> "text" --as <you>     read: node la.mjs inbox --as <you> --wait 30`);
    }
    case "contact": {
      if (a === "add" && b && pos[3]) {
        addContact(b, pos[3], flags.allow || "send,ask,reply,list");
        return console.log(`added ${b}; safety number ${await safety(parsePeer(splitInvite(pos[3])[0]))} (confirm it with their human over a channel you trust)`);
      }
      if (a === "accept" && b && pos[3]) {
        const reqs = read("requests.json", {}), r = reqs[b];
        if (!r) die(`no request ${b}; see: requests`);
        addContact(pos[3], r.address, flags.allow || "send,ask,reply,list");
        delete reqs[b];
        write("requests.json", reqs);
        return console.log(`accepted ${pos[3]} (${r.safety})`);
      }
      if (a === "ls") {
        for (const [n, c] of Object.entries(contacts())) console.log(`${n.padEnd(16)} ${await safety(parsePeer(c.address))}  allow=${c.allow.join(",")}`);
        return;
      }
      return die(USAGE);
    }
    case "requests":
      for (const [id, r] of Object.entries(read("requests.json", {}))) console.log(`${id}  "${r.name}"  ${r.safety}`);
      return;
    case "hello": {
      const me = await loadMe();
      const invite = read("contacts.json", {})[a]?.invite;
      await compose(me, a, "hello", me.name, { full: true, invite });
      return console.log(invite ? `hello sent to ${a} with its invite code; it adds you at once if the code is unused and less than 7 days old` : `hello sent to ${a}; they must accept it before your messages reach their agents`);
    }
    case "send":
    case "ask": {
      if (!a || b === undefined) die(USAGE);
      const me = await loadMe(), t = splitTo(a);
      const id = await compose(me, t.contact, cmd, b, { fromAgent: as, toAgent: t.agent });
      if (cmd === "send") return console.log(`sent ${id}`);
      return console.log((await answer(me, as, id, "reply", Number(flags.timeout || 60))).body);
    }
    case "inbox": {
      const me = await loadMe();
      return void (await drain(me, as, null, Number(flags.wait || 0), (g) => g.length > 0));
    }
    case "reply": {
      const asks = read("asks.json", {}), q = asks[a];
      if (!q || b === undefined) die(`no open ask with id ${a}`);
      const me = await loadMe();
      const id = await compose(me, q.contact, "reply", b, { replyTo: a, fromAgent: as });
      delete asks[a];
      write("asks.json", asks);
      return console.log(`sent ${id}`);
    }
    case "list": {
      if (!contacts()[a]) die(`no contact ${a}`);
      const me = await loadMe(), id = await compose(me, a, "list", "");
      for (const x of JSON.parse((await answer(me, as, id, "roster", Number(flags.timeout || 20))).body)) console.log(`${a}/${x.name}\t${x.harness}\t${x.status || ""}`);
      return;
    }
    default:
      die(USAGE, cmd ? 1 : 0);
  }
}

main().catch((e) => die(`error: ${e.message || e}`));
