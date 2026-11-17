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
use std::sync::OnceLock;
use worker::*;

const INDEX_HTML: &str = include_str!("../../web/index.html");
const AGENTS_MD: &str = include_str!("../../web/agents.md");
const PROTOCOL_MD: &str = include_str!("../../PROTOCOL.md");
const INSTALL_SH: &str = include_str!("../../web/install.sh");
const LA_MJS: &str = include_str!("../../web/la.mjs");
const SKILL_MD: &str = include_str!("../../skills/letagents/SKILL.md");
const LLMS_TXT: &str = include_str!("../../web/llms.txt");
const LLMS_FULL: &str = concat!(include_str!("../../web/llms.txt"), "\n\n---\n\n", include_str!("../../web/agents.md"), "\n\n---\n\n", include_str!("../../PROTOCOL.md"));
const ROBOTS: &str = include_str!("../../web/robots.txt");
const SITEMAP: &str = include_str!("../../web/sitemap.xml");
const SECURITY_TXT: &str = include_str!("../../web/security.txt");
/// Social previews, rendered by web/src/social.py: Open Graph (1200x630) and X (1200x600), at 2x.
const OG_PNG: &[u8] = include_bytes!("../../web/og.png");
const X_PNG: &[u8] = include_bytes!("../../web/x.png");

/// Agents fetching "/" with curl, fetch or an explicit text/markdown Accept get the setup guide as markdown.
fn wants_markdown(req: &Request) -> bool {
    let h = |k: &str| req.headers().get(k).ok().flatten().unwrap_or_default().to_ascii_lowercase();
    let (accept, ua) = (h("accept"), h("user-agent"));
    if accept.contains("text/markdown") || accept.contains("text/x-markdown") {
        return true;
    }
    !accept.contains("text/html")
        && ["curl/", "wget/", "httpie", "python-", "go-http-client", "node", "undici", "axios", "okhttp", "reqwest"].iter().any(|a| ua.contains(a))
}

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
    page_with_script(body, content_type, "")
}

/// `script` is a CSP source for the one inline script allowed on the page ('sha256-...'), or empty. That
/// script may also read same-origin URLs (/stats.json).
fn page_with_script(body: &str, content_type: &str, script: &str) -> Result<Response> {
    let mut r = Response::ok(body)?;
    let h = r.headers_mut();
    h.set("content-type", content_type)?;
    let script = if script.is_empty() { String::new() } else { format!(" script-src {script}; connect-src 'self';") };
    h.set(
        "content-security-policy",
        &format!("default-src 'none'; style-src 'unsafe-inline';{script} img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"),
    )?;
    h.set("referrer-policy", "no-referrer")?;
    h.set("x-content-type-options", "nosniff")?;
    h.set("strict-transport-security", "max-age=31536000")?;
    h.set("cache-control", "public, max-age=300")?;
    Ok(r)
}

fn image(body: &[u8]) -> Result<Response> {
    let mut r = Response::from_bytes(body.to_vec())?;
    let h = r.headers_mut();
    h.set("content-type", "image/png")?;
    h.set("x-content-type-options", "nosniff")?;
    h.set("cache-control", "public, max-age=86400")?;
    Ok(r)
}

fn is_b64_len(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let url = req.url()?;
    let path = url.path().to_string();
    // HEAD on a static route is answered like GET (link checkers and preview crawlers use it); the
    // runtime drops the body.
    let method = if req.method() == Method::Head { Method::Get } else { req.method() };
    match (method, path.as_str()) {
        (Method::Get, "/") | (Method::Get, "/index.html") => {
            let mut r = if wants_markdown(&req) {
                page(AGENTS_MD, "text/markdown; charset=utf-8")?
            } else {
                page_with_script(&with_stats(&stats(&env, &url).await), "text/html; charset=utf-8", chart_script_hash())?
            };
            r.headers_mut().set("vary", "Accept, User-Agent")?;
            r.headers_mut().set("link", r#"</agents.md>; rel="alternate"; type="text/markdown", </llms.txt>; rel="alternate"; type="text/plain""#)?;
            return Ok(r);
        }
        (Method::Get, "/agents.md") => return page(AGENTS_MD, "text/markdown; charset=utf-8"),
        (Method::Get, "/skill.md") | (Method::Get, "/SKILL.md") => return page(SKILL_MD, "text/markdown; charset=utf-8"),
        (Method::Get, "/llms.txt") => return page(LLMS_TXT, "text/plain; charset=utf-8"),
        (Method::Get, "/llms-full.txt") => return page(LLMS_FULL, "text/plain; charset=utf-8"),
        (Method::Get, "/robots.txt") => return page(ROBOTS, "text/plain; charset=utf-8"),
        (Method::Get, "/sitemap.xml") => return page(SITEMAP, "application/xml; charset=utf-8"),
        (Method::Get, "/.well-known/security.txt") | (Method::Get, "/security.txt") => return page(SECURITY_TXT, "text/plain; charset=utf-8"),
        (Method::Get, "/protocol.md") => return page(PROTOCOL_MD, "text/markdown; charset=utf-8"),
        (Method::Get, "/install.sh") => return page(INSTALL_SH, "text/plain; charset=utf-8"),
        (Method::Get, "/la.mjs") => return page(LA_MJS, "text/javascript; charset=utf-8"),
        (Method::Get, "/stats.json") => {
            let mut r = json(200, stats(&env, &url).await)?;
            r.headers_mut().set("cache-control", "public, max-age=60")?;
            r.headers_mut().set("access-control-allow-origin", "*")?;
            return Ok(r);
        }
        (Method::Get, "/og.png") => return image(OG_PNG),
        (Method::Get, "/x.png") => return image(X_PNG),
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
        // Edge limits (Workers rate limiting binding): cheap, and no DO is touched. Cost guard rails:
        // per IP, per mailbox (inbox logins) and per location (all relay requests). Over a limit: 429.
        let ip = req.headers().get("cf-connecting-ip")?.unwrap_or_else(|| "unknown".into());
        let mut checks = vec![(if *op == "send" { "RL_SEND" } else { "RL_INBOX" }, ip), ("RL_GLOBAL", "all".to_string())];
        if *op == "inbox" {
            checks.push(("RL_MAILBOX", id.to_string()));
        }
        for (limiter, key) in checks {
            if let Ok(rl) = env.rate_limiter(limiter) {
                if !rl.limit(key).await?.success {
                    let mut r = json(429, serde_json::json!({ "error": "slow down" }))?;
                    r.headers_mut().set("retry-after", "30")?;
                    return Ok(r);
                }
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
    env: Env,
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
        self.report(vec!["sent=1".into()]).await?;
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
        let counted = self.first_login()?;
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
            self.report(counted).await?;
            return json(200, serde_json::json!({ "msgs": msgs }));
        }
        let pair = WebSocketPair::new()?;
        self.state.accept_websocket_with_tags(&pair.server, &["owner"]);
        self.report(counted).await?;
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
    fn new(state: State, env: Env) -> Self {
        let sql = state.storage().sql();
        let _ = sql.exec("CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL)", None);
        let _ = sql.exec(
            "CREATE TABLE IF NOT EXISTS msgs (seq INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, ct BLOB NOT NULL)",
            None,
        );
        Self { state, env, window_start: Cell::new(0), window_count: Cell::new(0) }
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
        self.report(vec![]).await
    }

    async fn websocket_error(&self, _ws: WebSocket, _error: Error) -> Result<()> {
        self.report(vec![]).await
    }

    async fn alarm(&self) -> Result<Response> {
        self.report(vec![]).await?;
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

// ---------- public usage counters ----------
//
// Public: machines online now, sealed messages relayed per hour, and peak machines online per hour (30 days).
// "Machine" = a mailbox with an open WebSocket. Agents are not counted: their names are inside the envelopes.
//
// Live gauge, self-healing and without timers: a mailbox that is online is counted in the Stats row of the
// hour it last confirmed it (`live_h` in its own meta). It confirms on its own events only (login, message,
// close, alarm), and moves itself to the current hour at most once per hour. Stats sums the rows of the last
// LIVE_LEASE_H hours, so a mailbox whose close event was lost drops out after at most that long. Hibernation
// is untouched: keepalive pings never reach the object.

const LIVE_LEASE_H: i64 = 24;
const KEEP_HOURS: i64 = 720; // 30 days of hourly rows in the Stats object

fn hour_now() -> i64 {
    now_ms() / 3_600_000
}

impl Mailbox {
    fn online(&self) -> bool {
        // 1 = OPEN: a socket that is closing still shows up while its close event runs.
        self.state.get_websockets_with_tag("owner").iter().any(|ws| ws.as_ref().ready_state() == 1)
    }

    /// `new=1` the first time this mailbox logs in (machines_total). Also drops the day/week markers of v0.5.0.
    fn first_login(&self) -> Result<Vec<String>> {
        self.sql().exec("DELETE FROM meta WHERE k IN ('stat_day', 'stat_week')", None)?;
        if self.meta("counted")?.is_some() {
            return Ok(vec![]);
        }
        self.set_meta("counted", "1")?;
        Ok(vec!["new=1".into()])
    }

    /// One call to the Stats object with `q` plus this mailbox's live state, only if there is something to say.
    /// Stats gets increments and hour numbers, never an id.
    async fn report(&self, mut q: Vec<String>) -> Result<()> {
        let h = hour_now();
        let cur: Option<i64> = self.meta("live_h")?.and_then(|v| v.parse().ok());
        let counted = cur.filter(|c| *c >= h - LIVE_LEASE_H);
        if self.online() {
            if !cur.is_some_and(|c| c >= h) {
                q.push("on=1".into());
                q.extend(counted.map(|c| format!("off={c}")));
                self.set_meta("live_h", &h.to_string())?;
            }
        } else if cur.is_some() {
            q.extend(counted.map(|c| format!("off={c}")));
            self.sql().exec("DELETE FROM meta WHERE k = 'live_h'", None)?;
        }
        if !q.is_empty() {
            bump_stat(&self.env, &q.join("&")).await;
        }
        Ok(())
    }
}

fn stats_stub(env: &Env) -> Result<Stub> {
    env.durable_object("STATS")?.id_from_name("global")?.get_stub()
}

/// Add to the public counters. Best effort: a failure never fails the request it counts.
async fn bump_stat(env: &Env, query: &str) {
    if let Ok(stub) = stats_stub(env) {
        let _ = stub.fetch_with_str(&format!("https://stats/inc?{query}")).await;
    }
}

/// The public counters, from the edge cache when it has them (60 s), so page views rarely wake the Stats object.
async fn stats(env: &Env, url: &Url) -> serde_json::Value {
    let key = format!("{}/stats.json", url.origin().ascii_serialization());
    let cache = Cache::default();
    if let Ok(Some(mut r)) = cache.get(key.as_str(), false).await {
        if let Ok(v) = r.json::<serde_json::Value>().await {
            return v;
        }
    }
    let got = async {
        let mut r = stats_stub(env)?.fetch_with_str("https://stats/get").await?;
        r.json::<serde_json::Value>().await
    };
    let Ok(v) = got.await else { return serde_json::json!({}) };
    if let Ok(mut r) = Response::from_json(&v) {
        if r.headers_mut().set("cache-control", "public, max-age=60").is_ok() {
            let _ = cache.put(key.as_str(), r).await;
        }
    }
    v
}

fn fmt_n(n: i64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// CSP source for the chart script on the home page: the SHA-256 of the one plain `<script>` element.
fn chart_script_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| {
        let Some((_, rest)) = INDEX_HTML.split_once("<script>") else { return String::new() };
        let Some((js, _)) = rest.split_once("</script>") else { return String::new() };
        format!("'sha256-{}'", base64::engine::general_purpose::STANDARD.encode(Sha256::digest(js.as_bytes())))
    })
}

/// The page with the counters filled in (readable with scripts off) and the series for the chart.
fn with_stats(s: &serde_json::Value) -> String {
    let n = |k: &str| s[k].as_i64().map(fmt_n).unwrap_or_else(|| "-".into());
    let last24 = |k: &str| s[k].as_array().map(|a| a.iter().rev().take(24).filter_map(|v| v.as_i64()).collect::<Vec<_>>());
    let msgs_24h = last24("messages").map(|v| fmt_n(v.iter().sum())).unwrap_or_else(|| "-".into());
    let peak_24h = last24("peak_online").map(|v| fmt_n(v.into_iter().max().unwrap_or(0))).unwrap_or_else(|| "-".into());
    let data = serde_json::to_string(s).unwrap_or_else(|_| "{}".into()).replace("</", "<\\/");
    INDEX_HTML
        .replace("__ONLINE__", &n("machines_online"))
        .replace("__MSGS_24H__", &msgs_24h)
        .replace("__PEAK_24H__", &peak_24h)
        .replace("__MSGS_TOTAL__", &n("messages_total"))
        .replace("__MACHINES_TOTAL__", &n("machines_total"))
        .replace("__STATS_SINCE__", s["since"].as_str().unwrap_or("-"))
        .replace("__STATS_JSON__", &data)
}

#[derive(Deserialize)]
struct Num {
    n: Option<i64>,
}

fn iso_day(d: i64) -> String {
    let z = d + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let dd = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{m:02}-{dd:02}", yoe + era * 400 + if m <= 2 { 1 } else { 0 })
}

/// One object for the whole relay. Tables: `hour` (messages and peak machines online per hour, at most
/// KEEP_HOURS rows), `live` (online mailboxes by the hour they last confirmed it, at most LIVE_LEASE_H + 1
/// rows) and `total` (since, machines, sent). No ids, no IPs.
#[durable_object]
pub struct Stats {
    state: State,
}

impl Stats {
    fn get(&self, sql: &str, args: Vec<SqlStorageValue>) -> i64 {
        self.state.storage().sql().exec(sql, Some(args)).and_then(|c| c.one::<Num>()).ok().and_then(|r| r.n).unwrap_or(0)
    }

    fn online(&self, h: i64) -> i64 {
        self.get("SELECT SUM(n) AS n FROM live WHERE h >= ?", vec![(h - LIVE_LEASE_H).into()])
    }
}

impl DurableObject for Stats {
    fn new(state: State, _env: Env) -> Self {
        let sql = state.storage().sql();
        let _ = sql.exec("DROP TABLE IF EXISTS day", None);
        let _ = sql.exec("DROP TABLE IF EXISTS week", None);
        let _ = sql.exec("CREATE TABLE IF NOT EXISTS hour (h INTEGER PRIMARY KEY, sent INTEGER NOT NULL DEFAULT 0, peak INTEGER NOT NULL DEFAULT 0)", None);
        let _ = sql.exec("CREATE TABLE IF NOT EXISTS live (h INTEGER PRIMARY KEY, n INTEGER NOT NULL)", None);
        let _ = sql.exec("CREATE TABLE IF NOT EXISTS total (k TEXT PRIMARY KEY, v INTEGER NOT NULL)", None);
        Self { state }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        let url = req.url()?;
        let sql = self.state.storage().sql();
        let h = hour_now();
        if url.path() == "/inc" {
            let arg = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
            let bump = |k: &str| sql.exec("INSERT INTO total (k, v) VALUES (?, 1) ON CONFLICT(k) DO UPDATE SET v = v + 1", vec![k.into()]);
            if sql.exec("SELECT v AS n FROM total WHERE k = 'since'", None)?.to_array::<Num>()?.is_empty() {
                sql.exec("INSERT INTO total (k, v) VALUES ('since', ?)", vec![(h / 24).into()])?;
            }
            let before = self.online(h);
            if let Some(off) = arg("off").and_then(|v| v.parse::<i64>().ok()).filter(|o| *o >= h - LIVE_LEASE_H) {
                sql.exec("UPDATE live SET n = MAX(n - 1, 0) WHERE h = ?", vec![off.into()])?;
            }
            if arg("on").is_some() {
                sql.exec("INSERT INTO live (h, n) VALUES (?, 1) ON CONFLICT(h) DO UPDATE SET n = n + 1", vec![h.into()])?;
            }
            let after = self.online(h);
            // Hours with no event at all: the gauge did not change, so they get the value it had.
            let last = self.get("SELECT MAX(h) AS n FROM hour", vec![]);
            if last > 0 {
                for gap in (last + 1).max(h - KEEP_HOURS + 1)..h {
                    sql.exec("INSERT OR IGNORE INTO hour (h, peak) VALUES (?, ?)", vec![gap.into(), before.into()])?;
                }
            }
            let sent = i64::from(arg("sent").is_some());
            sql.exec(
                "INSERT INTO hour (h, sent, peak) VALUES (?, ?, ?) ON CONFLICT(h) DO UPDATE SET sent = sent + excluded.sent, peak = MAX(peak, excluded.peak)",
                vec![h.into(), sent.into(), before.max(after).into()],
            )?;
            if sent == 1 {
                bump("sent")?;
            }
            if arg("new").is_some() {
                bump("machines")?;
            }
            sql.exec("DELETE FROM hour WHERE h <= ?", vec![(h - KEEP_HOURS).into()])?;
            sql.exec("DELETE FROM live WHERE h < ? OR n = 0", vec![(h - LIVE_LEASE_H).into()])?;
            return Response::ok("");
        }
        let online = self.online(h);
        let mut rows = std::collections::BTreeMap::new();
        for row in sql.exec("SELECT h, sent, peak FROM hour WHERE h > ? ORDER BY h", vec![(h - KEEP_HOURS).into()])?.raw() {
            if let [SqlStorageValue::Integer(k), SqlStorageValue::Integer(s), SqlStorageValue::Integer(p)] = row?.as_slice() {
                rows.insert(*k, (*s, *p));
            }
        }
        let start = rows.keys().next().copied().unwrap_or(h).min(h);
        let last = rows.keys().next_back().copied().unwrap_or(h);
        let (mut messages, mut peak) = (vec![], vec![]);
        for k in start..=h {
            let (s, p) = rows.get(&k).copied().unwrap_or((0, if k > last { online } else { 0 }));
            messages.push(s);
            peak.push(if k == h { p.max(online) } else { p });
        }
        let since = self.get("SELECT v AS n FROM total WHERE k = 'since'", vec![]);
        Response::from_json(&serde_json::json!({
            "machines_online": online,
            "machines_total": self.get("SELECT v AS n FROM total WHERE k = 'machines'", vec![]),
            "messages_total": self.get("SELECT v AS n FROM total WHERE k = 'sent'", vec![]),
            "since": if since > 0 { iso_day(since) } else { "-".into() },
            "start": start * 3600,
            "step": 3600,
            "messages": messages,
            "peak_online": peak,
            "note": "Hourly, UTC, last 30 days. machines_online = mailboxes with an open relay connection now; messages = sealed envelopes relayed; peak_online = most machines online at once in that hour. Agents are not counted: their names are inside the sealed envelopes."
        }))
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}
