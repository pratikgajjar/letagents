// attack.mjs: the attacker's tools for tests/attack.sh. Each command does one real thing against the real
// relay, a real daemon socket or a real `letagents mcp` process, and prints what came back. No checks here:
// attack.sh decides what counts as a pass.
//
// Keys come from real identity.json files (made by `letagents init`). Sealing follows PROTOCOL.md exactly,
// so a forged envelope differs from a real one only in what the attacker chose to change.
//
//   seal <home> <to-address> [k=v ...]   envelope (base64url) on stdout. k: kind body from from_agent to_agent
//                                        reply_to invite id ts_offset_ms sign_with=<home> to_id=<id> full=1
//   post <address-with-drop> <envelope-file|-> [drop=<b64>] [ip=<ip>]   HTTP status and body
//   login <home> [ts=<ms>] [drop_hash=<b64>] [host=<host>]               signed inbox query string
//   ack <home> <seqs> [ts=<ms>]                                          signed ack query string
//   get <path> [ip=<ip>] [proto=http] [method=GET]                       status, headers, body
//   ws <path> [ip=<ip>] [secs=3]                                         frames received (one per line)
//   sock <socket> <json>                                                 one request on daemon.sock
//   watch <socket> <agent> <secs>                                        pushes received (one per line)
//   proxy <port-file> <target-port> <capture-file>                      TCP proxy that records client bytes
//   race <address-with-drop> <envelope-file>...                          post envelopes at the same moment
//   mcp <bin> <json-lines-file>                                          JSON-RPC over stdio, prints answers
//   pi <broker.sock> <name> <secs> [list]       a pi session on a pi-intercom broker: prints each message it
//                                               gets as JSON; with `list`, then the names of all sessions

import fs from "node:fs";
import net from "node:net";
import { spawn } from "node:child_process";

const MSG = "letagents-msg-v1\n", INFO = "letagents-v1 aes-256-gcm", AAD = "letagents-v1";
const PKCS8 = { Ed25519: "302e020100300506032b657004220420", X25519: "302e020100300506032b656e04220420" };
const RELAY = (process.env.LETAGENTS_RELAY || "http://127.0.0.1:8787").replace(/\/$/, "");
const HOST = process.env.LETAGENTS_AUTH_HOST || new URL(RELAY).host;
const subtle = globalThis.crypto.subtle;

const te = (s) => new TextEncoder().encode(s);
const cat = (...a) => {
  const o = new Uint8Array(a.reduce((n, x) => n + x.length, 0));
  a.reduce((i, x) => (o.set(x, i), i + x.length), 0);
  return o;
};
const b64 = (b) => Buffer.from(b).toString("base64url");
const unb64 = (s) => new Uint8Array(Buffer.from(s, "base64url"));
const hex = (h) => Uint8Array.from(h.match(/../g), (x) => parseInt(x, 16));
const rand = (n) => globalThis.crypto.getRandomValues(new Uint8Array(n));
const sha256 = async (b) => new Uint8Array(await subtle.digest("SHA-256", b));
const opts = (args) => Object.fromEntries(args.filter((a) => a.includes("=")).map((a) => [a.slice(0, a.indexOf("=")), a.slice(a.indexOf("=") + 1)]));

// A machine name is looked up in $ATK_HOMES (attack.sh sets it); a path is used as it is.
async function loadId(home) {
  if (!home.includes("/")) home = `${process.env.ATK_HOMES}/${home}`;
  const j = JSON.parse(fs.readFileSync(`${home}/identity.json`, "utf8"));
  const sigKey = await subtle.importKey("pkcs8", cat(hex(PKCS8.Ed25519), unb64(j.sig)), { name: "Ed25519" }, true, ["sign"]);
  const rcvKey = await subtle.importKey("pkcs8", cat(hex(PKCS8.X25519), unb64(j.rcv)), { name: "X25519" }, true, ["deriveBits"]);
  const pub = async (k) => unb64((await subtle.exportKey("jwk", k)).x);
  const me = { name: j.name, sigKey, sig: await pub(sigKey), rcv: await pub(rcvKey), drop: unb64(j.drop) };
  me.id = b64(me.sig);
  me.address = (full) => `la1.${b64(me.sig)}.${b64(me.rcv)}` + (full ? `.${b64(me.drop)}` : "");
  return me;
}
const peer = (addr) => {
  const p = addr.trim().split(".");
  return { id: p[1], sig: unb64(p[1]), rcv: unb64(p[2]), drop: p[3] };
};
const sign = async (key, msg) => b64(new Uint8Array(await subtle.sign("Ed25519", key, typeof msg === "string" ? te(msg) : msg)));

async function seal(signer, to, inner, toId) {
  const eph = await subtle.generateKey({ name: "X25519" }, true, ["deriveBits"]);
  const ephPub = new Uint8Array(await subtle.exportKey("raw", eph.publicKey));
  const innerJson = JSON.stringify(inner);
  const plain = te(JSON.stringify({ inner: innerJson, sig: await sign(signer.sigKey, cat(te(MSG), ephPub, te("\n"), te(innerJson))) }));
  const padded = cat(plain, new Uint8Array((1024 - (plain.length % 1024)) % 1024).fill(32));
  const peerKey = await subtle.importKey("raw", to.rcv, { name: "X25519" }, false, []);
  const shared = await subtle.deriveBits({ name: "X25519", public: peerKey }, eph.privateKey, 256);
  const ikm = await subtle.importKey("raw", shared, "HKDF", false, ["deriveKey"]);
  const key = await subtle.deriveKey({ name: "HKDF", hash: "SHA-256", salt: cat(ephPub, to.rcv), info: te(INFO) }, ikm, { name: "AES-GCM", length: 256 }, false, ["encrypt"]);
  const nonce = rand(12);
  const ct = await subtle.encrypt({ name: "AES-GCM", iv: nonce, additionalData: te(AAD + (toId || to.id)) }, key, padded);
  return cat(te("LA1"), ephPub, nonce, new Uint8Array(ct));
}

const headersOf = (o) => ({ ...(o.ip ? { "cf-connecting-ip": o.ip } : {}), ...(o.proto ? { "x-forwarded-proto": o.proto } : {}) });
const show = async (r) => console.log(`${r.status} ${await r.text()}`);

const [cmd, ...args] = process.argv.slice(2);
const o = opts(args);
const pos = args.filter((a) => !a.includes("=") || a.startsWith("la1.") || a.startsWith("/") || a.startsWith("{"));

switch (cmd) {
  case "seal": {
    const me = await loadId(pos[0]);
    const signer = o.sign_with ? await loadId(o.sign_with) : me;
    const to = peer(pos[1]);
    const inner = {
      v: 1,
      from: o.from || me.address(o.full === "1"),
      to: o.to_field || to.id,
      ts: Date.now() + Number(o.ts_offset_ms || 0),
      id: o.id || globalThis.crypto.randomUUID(),
      kind: o.kind || "send",
      ...(o.reply_to ? { reply_to: o.reply_to } : {}),
      ...(o.from_agent ? { from_agent: o.from_agent } : {}),
      ...(o.to_agent ? { to_agent: o.to_agent } : {}),
      ...(o.invite ? { invite: o.invite } : {}),
      body: o.body ?? "",
    };
    process.stdout.write(b64(await seal(signer, to, inner, o.to_id)));
    break;
  }
  case "post": {
    const to = peer(pos[0]);
    const body = pos[1] === "-" ? fs.readFileSync(0) : fs.readFileSync(pos[1]);
    const env = /^[A-Za-z0-9_-]+$/.test(body.toString().trim()) ? unb64(body.toString().trim()) : body;
    const r = await fetch(`${RELAY}/v1/send/${to.id}`, { method: "POST", headers: { "x-la-drop": o.drop ?? to.drop ?? "", "content-type": "application/octet-stream", ...headersOf(o) }, body: env });
    await show(r);
    break;
  }
  case "login": {
    const me = await loadId(pos[0]);
    const ts = o.ts || String(Date.now());
    const dh = o.drop_hash || b64(await sha256(me.drop));
    console.log(`/v1/inbox/${me.id}?ts=${ts}&drop_hash=${dh}&sig=${await sign(me.sigKey, `letagents-inbox-v1\n${o.host || HOST}\n${me.id}\n${ts}\n${dh}`)}`);
    break;
  }
  case "ack": {
    const me = await loadId(pos[0]);
    const ts = o.ts || String(Date.now());
    console.log(`/v1/ack/${me.id}?ts=${ts}&seqs=${args[1]}&sig=${await sign(me.sigKey, `letagents-ack-v1\n${HOST}\n${me.id}\n${ts}\n${args[1]}`)}`);
    break;
  }
  case "get": {
    const r = await fetch(`${RELAY}${pos[0]}`, { method: o.method || "GET", headers: headersOf(o), redirect: "manual" });
    console.log(`${r.status}`);
    for (const [k, v] of r.headers) console.log(`${k}: ${v}`);
    console.log("");
    console.log(await r.text());
    break;
  }
  case "ws": {
    const url = `${RELAY.replace(/^http/, "ws")}${pos[0]}`;
    await new Promise((resolve) => {
      const ws = new WebSocket(url, { headers: headersOf(o) });
      const done = () => (ws.close(), resolve());
      ws.onopen = () => console.log("open");
      ws.onmessage = (m) => console.log(String(m.data));
      ws.onerror = (e) => (console.log(`error ${e.message || ""}`), resolve());
      ws.onclose = () => resolve();
      setTimeout(done, Number(o.secs || 3) * 1000);
    });
    break;
  }
  case "sock":
  case "watch": {
    const s = net.createConnection(pos[0]);
    const req = cmd === "sock" ? JSON.parse(pos[1]) : { op: "watch", as: args[1], harness: "attack" };
    const b = te(JSON.stringify(req));
    const len = Buffer.alloc(4);
    len.writeUInt32BE(b.length);
    s.on("error", (e) => (console.log(`error ${e.code}`), process.exit(0)));
    s.write(Buffer.concat([len, b]));
    let buf = Buffer.alloc(0);
    s.on("data", (d) => {
      buf = Buffer.concat([buf, d]);
      while (buf.length >= 4 && buf.length >= 4 + buf.readUInt32BE(0)) {
        const f = JSON.parse(buf.subarray(4, 4 + buf.readUInt32BE(0)).toString());
        buf = buf.subarray(4 + buf.readUInt32BE(0));
        console.log(JSON.stringify(f));
        if (cmd === "sock") process.exit(0);
        if (f.push) {
          const a = te(JSON.stringify({ ok: true }));
          const l = Buffer.alloc(4);
          l.writeUInt32BE(a.length);
          s.write(Buffer.concat([l, a]));
        }
      }
    });
    if (cmd === "watch") setTimeout(() => process.exit(0), Number(args[2]) * 1000);
    break;
  }
  case "proxy": {
    const out = fs.createWriteStream(args[2]);
    const srv = net.createServer((c) => {
      const up = net.createConnection(Number(args[1]), "127.0.0.1");
      c.on("data", (d) => (out.write(d), up.write(d)));
      up.on("data", (d) => c.write(d));
      c.on("close", () => up.destroy());
      up.on("close", () => c.destroy());
      c.on("error", () => up.destroy());
      up.on("error", () => c.destroy());
    });
    srv.listen(0, "127.0.0.1", () => fs.writeFileSync(args[0], String(srv.address().port)));
    break;
  }
  case "race": {
    const to = peer(args[0]);
    const envs = args.slice(1).map((f) => unb64(fs.readFileSync(f, "utf8").trim()));
    const rs = await Promise.all(envs.map((e) => fetch(`${RELAY}/v1/send/${to.id}`, { method: "POST", headers: { "x-la-drop": to.drop }, body: e })));
    console.log(rs.map((r) => r.status).join(" "));
    break;
  }
  case "mcp": {
    const p = spawn(args[0], ["mcp", "--as", "attacker"], { stdio: ["pipe", "pipe", "inherit"] });
    let out = "";
    p.stdout.on("data", (d) => (out += d));
    for (const line of fs.readFileSync(args[1], "utf8").split("\n").filter(Boolean)) {
      p.stdin.write(line + "\n");
      await new Promise((r) => setTimeout(r, 300));
    }
    await new Promise((r) => setTimeout(r, 1500));
    p.kill();
    console.log(out);
    break;
  }
  case "pi": {
    const s = net.createConnection(args[0]);
    const send = (v) => {
      const b = Buffer.from(JSON.stringify(v));
      const l = Buffer.alloc(4);
      l.writeUInt32BE(b.length);
      s.write(Buffer.concat([l, b]));
    };
    const now = Date.now();
    s.on("error", (e) => (console.log(`error ${e.code}`), process.exit(1)));
    send({ type: "register", sessionId: globalThis.crypto.randomUUID(), session: { cwd: "/tmp", model: "test", pid: process.pid, startedAt: now, lastActivity: now, name: args[1], status: "idle" } });
    let buf = Buffer.alloc(0);
    s.on("data", (d) => {
      buf = Buffer.concat([buf, d]);
      while (buf.length >= 4 && buf.length >= 4 + buf.readUInt32BE(0)) {
        const f = JSON.parse(buf.subarray(4, 4 + buf.readUInt32BE(0)).toString());
        buf = buf.subarray(4 + buf.readUInt32BE(0));
        if (f.type === "message") console.log(JSON.stringify({ from: f.from?.name, text: f.message?.content?.text }));
        if (f.type === "sessions") (console.log(`sessions ${JSON.stringify(f.sessions.map((x) => x.name))}`), process.exit(0));
      }
    });
    setTimeout(() => (args[3] === "list" ? send({ type: "list", requestId: "l1" }) : process.exit(0)), Number(args[2]) * 1000);
    setTimeout(() => process.exit(0), Number(args[2]) * 1000 + 5000);
    break;
  }
  default:
    console.error("unknown command; see the header of tests/attack.mjs");
    process.exit(2);
}
