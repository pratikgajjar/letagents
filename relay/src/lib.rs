//! letagents.talk relay: a clueless mailbox.
//!
//! It stores sealed envelopes per recipient and hands them to whoever proves they hold the
//! recipient's Ed25519 key. It never sees plaintext and never learns who sent a message.
//! See PROTOCOL.md.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::cell::Cell;
use worker::*;

const INDEX_HTML: &str = include_str!("../../web/index.html");
const AGENTS_MD: &str = include_str!("../../web/agents.md");
const PROTOCOL_MD: &str = include_str!("../../PROTOCOL.md");
const INSTALL_SH: &str = include_str!("../../web/install.sh");

// Cost design: nothing is kept for long, every mailbox is small, idle mailboxes delete themselves,
// abusive IPs are stopped at the edge before a Durable Object wakes, and keepalive pings are
// answered by the runtime without waking the object (WebSocket hibernation + auto-response).
const MAX_ENVELOPE: usize = 32 * 1024; // agent messages are text; 32 KiB is plenty
const MIN_ENVELOPE: usize = 3 + 32 + 12 + 16;
const MAX_STORED: i64 = 200; // per mailbox: at most ~6.4 MiB ever stored
const TTL_MS: i64 = 48 * 3600 * 1000; // undelivered mail is deleted after 48 h
const IDLE_MS: i64 = 14 * 24 * 3600 * 1000; // a mailbox unused for 14 days is erased entirely
const CLOCK_SKEW_MS: i64 = 120_000;
const SEND_PER_MINUTE: u32 = 60; // per mailbox, on top of the per-IP edge limits

fn now_ms() -> i64 {
    Date::now().as_millis() as i64
}

fn json(status: u16, body: serde_json::Value) -> Result<Response> {
    let mut r = Response::from_json(&body)?.with_status(status);
    r.headers_mut().set("cache-control", "no-store")?;
    Ok(r)
}

fn page(body: &str, content_type: &str) -> Result<Response> {
    let mut r = Response::ok(body)?;
    let h = r.headers_mut();
    h.set("content-type", content_type)?;
    h.set(
        "content-security-policy",
        "default-src 'none'; style-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
    )?;
    h.set("referrer-policy", "no-referrer")?;
    h.set("x-content-type-options", "nosniff")?;
    h.set("strict-transport-security", "max-age=31536000")?;
    h.set("cache-control", "public, max-age=300")?;
    Ok(r)
}

fn is_b64_len(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let url = req.url()?;
    let path = url.path().to_string();
    match (req.method(), path.as_str()) {
        (Method::Get, "/") | (Method::Get, "/index.html") => return page(INDEX_HTML, "text/html; charset=utf-8"),
        (Method::Get, "/agents.md") | (Method::Get, "/llms.txt") => return page(AGENTS_MD, "text/markdown; charset=utf-8"),
        (Method::Get, "/protocol.md") => return page(PROTOCOL_MD, "text/markdown; charset=utf-8"),
        (Method::Get, "/install.sh") => return page(INSTALL_SH, "text/plain; charset=utf-8"),
        (Method::Get, "/health") => return json(200, serde_json::json!({ "ok": true })),
        _ => {}
    }
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if let ["v1", op @ ("send" | "inbox" | "ack"), id] = parts.as_slice() {
        if !is_b64_len(id, 43) {
            return json(404, serde_json::json!({ "error": "not found" }));
        }
        let ok_method = matches!((*op, req.method()), ("send", Method::Post) | ("inbox", Method::Get) | ("ack", Method::Post));
        if !ok_method {
            return json(405, serde_json::json!({ "error": "method not allowed" }));
        }
        // Kill switch for the operator: RELAY_MODE=closed stops all traffic before any DO wakes.
        if env.var("RELAY_MODE").map(|v| v.to_string()).unwrap_or_default() == "closed" {
            return json(503, serde_json::json!({ "error": "relay paused" }));
        }
        // Per-IP edge limits (Workers rate limiting binding): cheap, and no DO is touched.
        let ip = req.headers().get("cf-connecting-ip")?.unwrap_or_else(|| "unknown".into());
        let limiter = if *op == "send" { "RL_SEND" } else { "RL_INBOX" };
        if let Ok(rl) = env.rate_limiter(limiter) {
            if !rl.limit(ip).await?.success {
                return json(429, serde_json::json!({ "error": "slow down" }));
            }
        }
        if *op == "send" {
            if let Some(len) = req.headers().get("content-length")?.and_then(|l| l.parse::<usize>().ok()) {
                if len > MAX_ENVELOPE {
                    return json(413, serde_json::json!({ "error": "envelope too large" }));
                }
            }
        }
        let stub = env.durable_object("MAILBOX")?.id_from_name(id)?.get_stub()?;
        return stub.fetch_with_request(req).await;
    }
    json(404, serde_json::json!({ "error": "not found" }))
}

#[derive(Deserialize)]
struct Val {
    v: String,
}
#[derive(Deserialize)]
struct Count {
    n: i64,
}
#[derive(Deserialize)]
struct Seq {
    seq: i64,
}
#[derive(Deserialize)]
struct ClientFrame {
    t: String,
    seq: Option<i64>,
}

#[durable_object]
pub struct Mailbox {
    state: State,
    window_start: Cell<i64>,
    window_count: Cell<u32>,
}

impl Mailbox {
    fn sql(&self) -> SqlStorage {
        self.state.storage().sql()
    }

    fn meta(&self, k: &str) -> Result<Option<String>> {
        let rows: Vec<Val> = self.sql().exec("SELECT v FROM meta WHERE k = ?", vec![k.into()])?.to_array()?;
        Ok(rows.into_iter().next().map(|r| r.v))
    }

    fn set_meta(&self, k: &str, v: &str) -> Result<()> {
        self.sql().exec(
            "INSERT INTO meta (k, v) VALUES (?, ?) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            vec![k.into(), v.into()],
        )?;
        Ok(())
    }

    /// Anyone holding the recipient's address (and so its drop secret) may drop an envelope.
    /// The relay does not know or record who the sender is.
    async fn send(&self, mut req: Request) -> Result<Response> {
        let no_box = || json(404, serde_json::json!({ "error": "no such mailbox" }));
        let Some(want) = self.meta("drop_hash")? else { return no_box() };
        let drop = req.headers().get("x-la-drop")?.unwrap_or_default();
        if !is_b64_len(&drop, 22) {
            return no_box();
        }
        let Ok(drop_bytes) = B64.decode(&drop) else { return no_box() };
        let got = B64.encode(Sha256::digest(&drop_bytes));
        if !ct_eq(got.as_bytes(), want.as_bytes()) {
            return no_box();
        }

        let now = now_ms();
        if now - self.window_start.get() > 60_000 {
            self.window_start.set(now);
            self.window_count.set(0);
        }
        self.window_count.set(self.window_count.get() + 1);
        if self.window_count.get() > SEND_PER_MINUTE {
            return json(429, serde_json::json!({ "error": "slow down" }));
        }

        let ct = req.bytes().await?;
        if ct.len() > MAX_ENVELOPE {
            return json(413, serde_json::json!({ "error": "envelope too large" }));
        }
        if ct.len() < MIN_ENVELOPE || &ct[..3] != b"LA1" {
            return json(400, serde_json::json!({ "error": "bad envelope" }));
        }
        let count: Count = self.sql().exec("SELECT COUNT(*) AS n FROM msgs", None)?.one()?;
        if count.n >= MAX_STORED {
            return json(507, serde_json::json!({ "error": "mailbox full" }));
        }
        let seq: Seq = self
            .sql()
            .exec("INSERT INTO msgs (ts, ct) VALUES (?, ?) RETURNING seq", vec![now.into(), ct.clone().into()])?
            .one()?;
        let frame = serde_json::json!({ "t": "msg", "seq": seq.seq, "ct": B64.encode(&ct) }).to_string();
        for ws in self.state.get_websockets_with_tag("owner") {
            let _ = ws.send_with_str(&frame);
        }
        self.schedule_cleanup().await?;
        json(202, serde_json::json!({ "ok": true, "seq": seq.seq }))
    }

    /// Only the holder of the mailbox's Ed25519 private key may read it or set its drop hash.
    /// WSS for live delivery; a plain HTTPS GET returns what is waiting (then POST /v1/ack).
    async fn inbox(&self, req: Request, id: &str) -> Result<Response> {
        let websocket = req.headers().get("upgrade")?.map(|u| u.to_ascii_lowercase()) == Some("websocket".into());
        let url = req.url()?;
        let q = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned()).unwrap_or_default();
        let (ts, drop_hash, sig) = (q("ts"), q("drop_hash"), q("sig"));
        let bad = |e: &str| json(401, serde_json::json!({ "error": e }));
        let Ok(ts_n) = ts.parse::<i64>() else { return bad("stale") };
        if ts.len() != 13 || (now_ms() - ts_n).abs() > CLOCK_SKEW_MS {
            return bad("stale");
        }
        if !is_b64_len(&drop_hash, 43) || !is_b64_len(&sig, 86) {
            return bad("bad auth");
        }
        let host = url.host_str().unwrap_or_default();
        let msg = format!("letagents-inbox-v1\n{host}\n{id}\n{ts}\n{drop_hash}");
        let verified = (|| -> Option<()> {
            let pk: [u8; 32] = B64.decode(id).ok()?.try_into().ok()?;
            let sg: [u8; 64] = B64.decode(&sig).ok()?.try_into().ok()?;
            VerifyingKey::from_bytes(&pk).ok()?.verify_strict(msg.as_bytes(), &Signature::from_bytes(&sg)).ok()
        })();
        if verified.is_none() {
            return bad("bad signature");
        }
        let last: i64 = self.meta("last_auth_ts")?.and_then(|v| v.parse().ok()).unwrap_or(0);
        if ts_n <= last {
            return bad("replayed");
        }
        self.set_meta("last_auth_ts", &ts)?;
        self.set_meta("drop_hash", &drop_hash)?;
        self.set_meta("last_seen", &now_ms().to_string())?;
        self.schedule_cleanup().await?;
        // Keepalive pings are answered by the runtime itself: the object stays hibernated (not billed).
        if let Ok(pair) = worker::worker_sys::WebSocketRequestResponsePair::new(r#"{"t":"ping"}"#, r#"{"t":"pong"}"#) {
            self.state.set_websocket_auto_response(&pair);
        }

        if !websocket {
            let mut msgs = vec![];
            for row in self.sql().exec("SELECT seq, ct FROM msgs ORDER BY seq LIMIT 100", None)?.raw() {
                if let [SqlStorageValue::Integer(seq), SqlStorageValue::Blob(ct)] = row?.as_slice() {
                    msgs.push(serde_json::json!({ "seq": seq, "ct": B64.encode(ct) }));
                }
            }
            return json(200, serde_json::json!({ "msgs": msgs }));
        }
        let pair = WebSocketPair::new()?;
        self.state.accept_websocket_with_tags(&pair.server, &["owner"]);
        for row in self.sql().exec("SELECT seq, ct FROM msgs ORDER BY seq", None)?.raw() {
            if let [SqlStorageValue::Integer(seq), SqlStorageValue::Blob(ct)] = row?.as_slice() {
                pair.server.send_with_str(serde_json::json!({ "t": "msg", "seq": seq, "ct": B64.encode(ct) }).to_string())?;
            }
        }
        pair.server.send_with_str(r#"{"t":"ready"}"#)?;
        Response::from_websocket(pair.client)
    }
}

impl Mailbox {
    /// Delete delivered envelopes. Signed by the mailbox key; idempotent, so a replayed ack is harmless.
    async fn ack(&self, req: Request, id: &str) -> Result<Response> {
        let url = req.url()?;
        let q = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned()).unwrap_or_default();
        let (ts, seqs, sig) = (q("ts"), q("seqs"), q("sig"));
        let bad = |e: &str| json(401, serde_json::json!({ "error": e }));
        let Ok(ts_n) = ts.parse::<i64>() else { return bad("stale") };
        if (now_ms() - ts_n).abs() > CLOCK_SKEW_MS || seqs.len() > 2000 || !is_b64_len(&sig, 86) {
            return bad("bad auth");
        }
        let host = url.host_str().unwrap_or_default();
        let msg = format!("letagents-ack-v1\n{host}\n{id}\n{ts}\n{seqs}");
        let verified = (|| -> Option<()> {
            let pk: [u8; 32] = B64.decode(id).ok()?.try_into().ok()?;
            let sg: [u8; 64] = B64.decode(&sig).ok()?.try_into().ok()?;
            VerifyingKey::from_bytes(&pk).ok()?.verify_strict(msg.as_bytes(), &Signature::from_bytes(&sg)).ok()
        })();
        if verified.is_none() {
            return bad("bad signature");
        }
        let mut n = 0;
        for seq in seqs.split(',').filter_map(|s| s.parse::<i64>().ok()) {
            self.sql().exec("DELETE FROM msgs WHERE seq = ?", vec![seq.into()])?;
            n += 1;
        }
        self.set_meta("last_seen", &now_ms().to_string())?;
        json(200, serde_json::json!({ "ok": true, "acked": n }))
    }
}

impl DurableObject for Mailbox {
    fn new(state: State, _env: Env) -> Self {
        let sql = state.storage().sql();
        let _ = sql.exec("CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL)", None);
        let _ = sql.exec(
            "CREATE TABLE IF NOT EXISTS msgs (seq INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, ct BLOB NOT NULL)",
            None,
        );
        Self { state, window_start: Cell::new(0), window_count: Cell::new(0) }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        let path = req.url()?.path().to_string();
        let id = path.rsplit('/').next().unwrap_or_default().to_string();
        if path.starts_with("/v1/send/") {
            self.send(req).await
        } else if path.starts_with("/v1/ack/") {
            self.ack(req, &id).await
        } else {
            self.inbox(req, &id).await
        }
    }

    async fn websocket_message(&self, ws: WebSocket, message: WebSocketIncomingMessage) -> Result<()> {
        let WebSocketIncomingMessage::String(text) = message else { return Ok(()) };
        if text.len() > 256 {
            return Ok(());
        }
        let Ok(f) = serde_json::from_str::<ClientFrame>(&text) else { return Ok(()) };
        match (f.t.as_str(), f.seq) {
            ("ack", Some(seq)) => {
                self.sql().exec("DELETE FROM msgs WHERE seq = ?", vec![seq.into()])?;
            }
            ("ping", _) => ws.send_with_str(r#"{"t":"pong"}"#)?,
            _ => {}
        }
        Ok(())
    }

    async fn websocket_close(&self, ws: WebSocket, code: usize, _reason: String, _clean: bool) -> Result<()> {
        self.set_meta("last_seen", &now_ms().to_string())?;
        let code = if code == 1005 || code == 1006 { 1000 } else { code as u16 };
        let _ = ws.close(Some(code), Some("bye"));
        Ok(())
    }

    async fn websocket_error(&self, _ws: WebSocket, _error: Error) -> Result<()> {
        Ok(())
    }

    async fn alarm(&self) -> Result<Response> {
        let now = now_ms();
        self.sql().exec("DELETE FROM msgs WHERE ts < ?", vec![(now - TTL_MS).into()])?;
        let left: Count = self.sql().exec("SELECT COUNT(*) AS n FROM msgs", None)?.one()?;
        let last_seen: i64 = self.meta("last_seen")?.and_then(|v| v.parse().ok()).unwrap_or(0);
        let online = !self.state.get_websockets_with_tag("owner").is_empty();
        if left.n == 0 && !online && now - last_seen > IDLE_MS {
            // Idle and empty: erase the whole mailbox. It costs nothing until its owner returns.
            self.state.storage().delete_all().await?;
            return Response::ok("");
        }
        self.schedule_cleanup().await?;
        Response::ok("")
    }
}

impl Mailbox {
    /// One alarm per mailbox, at the next moment something can be deleted: the oldest message's
    /// expiry, or the idle deadline. No periodic polling.
    async fn schedule_cleanup(&self) -> Result<()> {
        #[derive(Deserialize)]
        struct Oldest {
            ts: Option<i64>,
        }
        let oldest: Oldest = self.sql().exec("SELECT MIN(ts) AS ts FROM msgs", None)?.one()?;
        let last_seen: i64 = self.meta("last_seen")?.and_then(|v| v.parse().ok()).unwrap_or_else(now_ms);
        let at = match oldest.ts {
            Some(ts) => ts + TTL_MS + 60_000,
            None => last_seen + IDLE_MS + 60_000,
        };
        let at = at.max(now_ms() + 60_000);
        if self.state.storage().get_alarm().await? != Some(at) {
            self.state.storage().set_alarm(at).await?;
        }
        Ok(())
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}
