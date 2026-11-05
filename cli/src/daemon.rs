//! The daemon: one per device. It owns the keys, the contact rules and the relay connection, and serves
//! every local harness:
//!
//! - pi: joins the pi-intercom broker as one session per contact. Inbound mail is pushed straight into
//!   the target pi session; asks keep their ask/reply link.
//! - everything else: `~/.letagents/daemon.sock`, same framing as pi-intercom (4-byte big-endian length,
//!   then JSON). `letagents mcp` and the CLI use it. Mail for a session that is not on intercom waits in
//!   the spool until that agent reads its inbox.

use crate::crypto::{open, Identity, Inner, Peer};
use crate::intercom::{frame, read_frame, stable_session_id, Local, Proxy};
use crate::store::{self, Seen};
use crate::{compose_and_send, relay};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, Notify};

/// The local intercom session that receives notices (contact requests).
pub const SYSTEM_SESSION: &str = "letagents";

pub fn socket_path() -> PathBuf {
    store::home().join("daemon.sock")
}

fn log(s: impl AsRef<str>) {
    let t = crate::crypto::now_ms() / 1000 % 86400;
    println!("{:02}:{:02}:{:02} {}", t / 3600, t / 60 % 60, t % 60, s.as_ref());
}

/// A message waiting for a local agent that is not reachable through intercom.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Spooled {
    pub id: String,
    pub from: String,
    pub kind: String,
    pub target: String,
    pub text: String,
    pub ts: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Spool {
    msgs: Vec<Spooled>,
    /// inbound ask id -> (contact, received at); answerable with the `reply` op
    answerable: BTreeMap<String, (String, i64)>,
}

/// Who is waiting for the reply to an ask we sealed.
enum Waiter {
    Intercom { proxy: String, local_from: String, local_msg: String },
    Socket { asker: String, tx: oneshot::Sender<Spooled> },
    Spool { asker: String },
}

struct Hub {
    me: Identity,
    default_target: String,
    spool: Mutex<Spool>,
    waiters: Mutex<HashMap<String, Waiter>>,
    /// intercom message id of an ask we delivered locally -> (contact, remote ask id)
    remote_ask_by_local: Mutex<HashMap<String, (String, String)>>,
    proxies: RwLock<HashMap<String, Arc<Proxy>>>,
    system: RwLock<Option<Arc<Proxy>>>,
    arrived: Notify,
}

impl Hub {
    fn save_spool(&self, s: &Spool) {
        let _ = store::write("spool.json", s);
    }

    fn spool(&self, m: Spooled, answerable_from: Option<&str>) {
        let mut s = self.spool.lock().unwrap();
        if let Some(contact) = answerable_from {
            s.answerable.insert(m.id.clone(), (contact.to_string(), m.ts));
        }
        let cut = crate::crypto::now_ms() - crate::crypto::MAX_AGE_MS;
        s.msgs.retain(|x| x.ts >= cut);
        s.answerable.retain(|_, (_, t)| *t >= cut);
        s.msgs.push(m);
        self.save_spool(&s);
        drop(s);
        self.arrived.notify_waiters();
    }

    fn proxy(&self, name: &str) -> Option<Arc<Proxy>> {
        self.proxies.read().unwrap().get(name).cloned()
    }
}

pub async fn run(me: Identity, default_target: String) -> Result<()> {
    let sock = socket_path();
    if UnixStream::connect(&sock).await.is_ok() {
        bail!("a letagents daemon is already running ({})", sock.display());
    }
    let _ = std::fs::remove_file(&sock);
    store::write("spool.json", &store::read::<Spool>("spool.json"))?; // ensures ~/.letagents exists (0700)
    let listener = UnixListener::bind(&sock)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    }
    let hub = Arc::new(Hub {
        me,
        default_target,
        spool: Mutex::new(store::read("spool.json")),
        waiters: Mutex::default(),
        remote_ask_by_local: Mutex::default(),
        proxies: RwLock::default(),
        system: RwLock::default(),
        arrived: Notify::new(),
    });
    log(format!("socket {} ready; default target \"{}\"", sock.display(), hub.default_target));

    tokio::spawn(intercom_manager(hub.clone()));
    tokio::spawn(relay_inbox(hub.clone()));
    loop {
        let (conn, _) = listener.accept().await?;
        tokio::spawn(serve_socket(hub.clone(), conn));
    }
}

// ---------- pi-intercom adapter ----------

/// Keeps one intercom session per contact (plus the system session) registered on the local broker,
/// reconnecting whenever the broker restarts. Without a broker, everything still works through the socket.
async fn intercom_manager(hub: Arc<Hub>) {
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Local>();
    {
        let hub = hub.clone();
        tokio::spawn(async move {
            while let Some(m) = in_rx.recv().await {
                from_intercom(&hub, m).await;
            }
        });
    }
    let mut announced = false;
    loop {
        let (closed_tx, mut closed_rx) = mpsc::unbounded_channel::<String>();
        let system = Proxy::connect(SYSTEM_SESSION, &stable_session_id(&format!("system:{}", hub.me.id())), in_tx.clone(), closed_tx.clone()).await;
        match system {
            Err(e) => {
                if !announced {
                    log(format!("pi-intercom not available ({e}); socket and MCP clients still work"));
                    announced = true;
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
            Ok(p) => *hub.system.write().unwrap() = Some(p),
        }
        announced = false;
        let mut proxies = HashMap::new();
        for (name, c) in store::contacts() {
            match Proxy::connect(&name, &stable_session_id(&c.address), in_tx.clone(), closed_tx.clone()).await {
                Ok(p) => {
                    proxies.insert(name.clone(), p);
                }
                Err(e) => log(format!("intercom session \"{name}\" failed: {e}")),
            }
        }
        log(format!("pi-intercom connected; contacts as sessions: {}", proxies.keys().cloned().collect::<Vec<_>>().join(", ")));
        *hub.proxies.write().unwrap() = proxies;
        let gone = closed_rx.recv().await.unwrap_or_default();
        log(format!("pi-intercom connection \"{gone}\" closed; reconnecting"));
        hub.proxies.write().unwrap().clear();
        *hub.system.write().unwrap() = None;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// A local pi agent sent or asked one of our contact sessions, or replied to a forwarded ask.
async fn from_intercom(hub: &Hub, m: Local) {
    if m.proxy == SYSTEM_SESSION {
        return;
    }
    let link = m.reply_to.as_ref().and_then(|r| hub.remote_ask_by_local.lock().unwrap().remove(r));
    let result = if let Some((contact, remote_id)) = link.filter(|(c, _)| *c == m.proxy) {
        compose_and_send(&hub.me, &contact, "reply", &m.text, Some(remote_id), None).await.map(|_| "reply")
    } else {
        let (session, body) = split_session(&m.text);
        let kind = if m.expects_reply { "ask" } else { "send" };
        let who = if m.from_name.is_empty() { &m.from_id } else { &m.from_name };
        let body = format!("[from {}/{who}]\n{body}", hub.me.name);
        match compose_and_send(&hub.me, &m.proxy, kind, &body, None, session).await {
            Ok(id) => {
                if kind == "ask" {
                    hub.waiters.lock().unwrap().insert(id, Waiter::Intercom { proxy: m.proxy.clone(), local_from: m.from_id.clone(), local_msg: m.id.clone() });
                }
                Ok(kind)
            }
            Err(e) => Err(e),
        }
    };
    match result {
        Ok(kind) => log(format!("out {kind} -> {} (from intercom)", m.proxy)),
        Err(e) => {
            log(format!("out -> {} FAILED: {e}", m.proxy));
            if let Some(p) = hub.proxy(&m.proxy) {
                let _ = p.send(&m.from_id, &format!("[letagents] delivery to {} failed: {e}", m.proxy), None, false).await;
            }
        }
    }
}

fn split_session(text: &str) -> (Option<String>, String) {
    match text.trim_start().strip_prefix("to:").and_then(|r| r.split_once('\n')) {
        Some((s, b)) => (Some(s.trim().to_string()), b.trim().to_string()),
        None => (None, text.to_string()),
    }
}

// ---------- relay -> local ----------

async fn relay_inbox(hub: Arc<Hub>) {
    let mut seen: Seen = store::read("seen.json");
    let me = hub.me.clone();
    relay::live(&me, |env| {
        let hub = hub.clone();
        let fresh = match open(&hub.me, &env) {
            Ok((inner, sender)) => {
                let new = seen.insert(&inner.id, inner.ts);
                let _ = store::write("seen.json", &seen);
                if new {
                    Some((inner, sender))
                } else {
                    log(format!("dropped replay {}", inner.id));
                    None
                }
            }
            Err(e) => {
                log(format!("dropped envelope: {e}"));
                None
            }
        };
        async move {
            if let Some((inner, sender)) = fresh {
                inbound(&hub, inner, sender).await;
            }
        }
    })
    .await
}

async fn inbound(hub: &Hub, inner: Inner, sender: Peer) {
    let (name, contact) = match store::authorize(&sender, &inner.kind) {
        Ok(x) => x,
        Err(reason) if inner.kind == "hello" && reason.starts_with("unknown sender") => {
            let (rid, req) = store::record_request(&sender, &inner.body, &inner.id, inner.ts);
            log(format!("contact request {rid} from \"{}\" ({})", req.name, req.safety));
            let note = format!(
                "[letagents] contact request {rid} from an agent calling itself \"{}\", safety number {}.\nAsk your human to confirm this safety number with them, then run: letagents contact accept {rid} <name> --allow send,ask,reply",
                req.name, req.safety
            );
            let system = hub.system.read().unwrap().clone();
            let delivered = match system {
                Some(p) => p.send(&hub.default_target, &note, None, false).await.map(|s| s.delivered).unwrap_or(false),
                None => false,
            };
            if !delivered {
                hub.spool(Spooled { id: inner.id, from: SYSTEM_SESSION.into(), kind: "notice".into(), target: hub.default_target.clone(), text: note, ts: inner.ts, reply_to: None }, None);
            }
            return;
        }
        Err(reason) => return log(format!("REJECTED {} {}: {reason}", inner.kind, inner.id)),
    };
    if inner.kind == "hello" {
        return log(format!("hello from known contact {name}"));
    }

    if inner.kind == "reply" {
        let waiter = inner.reply_to.as_ref().and_then(|r| hub.waiters.lock().unwrap().remove(r));
        let reply = Spooled { id: inner.id.clone(), from: name.clone(), kind: "reply".into(), target: hub.default_target.clone(), text: inner.body.clone(), ts: inner.ts, reply_to: inner.reply_to.clone() };
        match waiter {
            Some(Waiter::Intercom { proxy, local_from, local_msg }) => {
                let r = match hub.proxy(&proxy) {
                    Some(p) => p.send(&local_from, &inner.body, Some(&local_msg), false).await.map(|s| s.delivered).unwrap_or(false),
                    None => false,
                };
                if !r {
                    hub.spool(Spooled { target: local_from, ..reply }, None);
                }
                log(format!("in reply from {name} -> intercom asker: {}", if r { "delivered" } else { "spooled" }));
            }
            Some(Waiter::Socket { asker, tx }) => {
                if let Err(reply) = tx.send(reply) {
                    hub.spool(Spooled { target: asker, ..reply }, None);
                }
                log(format!("in reply from {name} -> socket asker"));
            }
            Some(Waiter::Spool { asker }) => {
                hub.spool(Spooled { target: asker.clone(), ..reply }, None);
                log(format!("in reply from {name} -> spooled for {asker}"));
            }
            None => {
                hub.spool(reply, None);
                log(format!("in reply from {name}: no open ask; spooled for {}", hub.default_target));
            }
        }
        return;
    }

    // send / ask: push through intercom when the target session is there, else spool it.
    let target = inner.session.clone().or(contact.session.clone()).unwrap_or_else(|| hub.default_target.clone());
    let ask = inner.kind == "ask";
    if let Some(proxy) = hub.proxy(&name) {
        let header = if ask {
            format!("[letagents · verified {name}] ASK: reply to this message to answer it")
        } else {
            format!("[letagents · verified {name}] to answer, send to \"{name}\" (first line \"to: <their session>\" to pick one)")
        };
        if let Ok(s) = proxy.send(&target, &format!("{header}\n\n{}", inner.body), None, ask).await {
            if s.delivered {
                if ask {
                    hub.remote_ask_by_local.lock().unwrap().insert(s.id, (name.clone(), inner.id.clone()));
                }
                return log(format!("in {} from {name} -> {target}: pushed via intercom", inner.kind));
            }
        }
    }
    hub.spool(
        Spooled { id: inner.id.clone(), from: name.clone(), kind: inner.kind.clone(), target: target.clone(), text: inner.body, ts: inner.ts, reply_to: None },
        if ask { Some(&name) } else { None },
    );
    log(format!("in {} from {name} -> {target}: spooled", inner.kind));
}

// ---------- daemon.sock ----------

#[derive(Deserialize)]
struct Req {
    #[serde(default)]
    id: Value,
    op: String,
    #[serde(default)]
    to: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    session: Option<String>,
    /// the local agent making the request (its session name)
    #[serde(default, rename = "as")]
    as_: Option<String>,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    wait_ms: u64,
    #[serde(default)]
    timeout_ms: u64,
}

async fn serve_socket(hub: Arc<Hub>, conn: UnixStream) {
    let (mut rd, mut wr) = conn.into_split();
    while let Ok(v) = read_frame(&mut rd).await {
        let resp = match serde_json::from_value::<Req>(v) {
            Ok(req) => {
                let id = req.id.clone();
                let mut r = handle(&hub, req).await.unwrap_or_else(|e| json!({ "ok": false, "error": e.to_string() }));
                r["id"] = id;
                r
            }
            Err(e) => json!({ "ok": false, "error": format!("bad request: {e}") }),
        };
        if wr.write_all(&frame(&resp)).await.is_err() {
            break;
        }
    }
}

async fn handle(hub: &Hub, r: Req) -> Result<Value> {
    let me_as = r.as_.clone().unwrap_or_else(|| hub.default_target.clone());
    match r.op.as_str() {
        "status" => {
            let s = hub.spool.lock().unwrap();
            Ok(json!({
                "ok": true, "name": hub.me.name, "address": hub.me.peer().address(true),
                "safety": hub.me.peer().safety_number(), "default_target": hub.default_target,
                "intercom": hub.system.read().unwrap().is_some(), "waiting": s.msgs.len(),
            }))
        }
        "contacts" => Ok(json!({ "ok": true, "contacts": store::contacts().into_iter().map(|(n, c)| json!({
            "name": n, "allow": c.allow, "safety": Peer::parse(&c.address).map(|p| p.safety_number()).unwrap_or_default(),
        })).collect::<Vec<_>>() })),
        "send" | "ask" => {
            let body = format!("[from {}/{me_as}]\n{}", hub.me.name, r.text);
            let id = compose_and_send(&hub.me, &r.to, &r.op, &body, None, r.session.clone()).await?;
            log(format!("out {} -> {} (from socket, as {me_as})", r.op, r.to));
            if r.op == "send" {
                return Ok(json!({ "ok": true, "msg_id": id }));
            }
            let (tx, rx) = oneshot::channel();
            hub.waiters.lock().unwrap().insert(id.clone(), Waiter::Socket { asker: me_as.clone(), tx });
            let wait = Duration::from_millis(if r.timeout_ms == 0 { 50_000 } else { r.timeout_ms.min(600_000) });
            match tokio::time::timeout(wait, rx).await {
                Ok(Ok(reply)) => Ok(json!({ "ok": true, "msg_id": id, "reply": reply })),
                _ => {
                    // Keep the link: a late reply goes to the asker's inbox.
                    let mut w = hub.waiters.lock().unwrap();
                    if w.contains_key(&id) {
                        w.insert(id.clone(), Waiter::Spool { asker: me_as });
                    }
                    Ok(json!({ "ok": true, "msg_id": id, "timeout": true, "note": "no reply yet; it will arrive in your inbox" }))
                }
            }
        }
        "reply" => {
            let (contact, _) = hub.spool.lock().unwrap().answerable.remove(&r.msg).ok_or_else(|| anyhow::anyhow!("no open ask with id {}", r.msg))?;
            hub.save_spool(&hub.spool.lock().unwrap());
            let id = compose_and_send(&hub.me, &contact, "reply", &r.text, Some(r.msg.clone()), None).await?;
            log(format!("out reply -> {contact} (from socket)"));
            Ok(json!({ "ok": true, "msg_id": id }))
        }
        "inbox" => {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(r.wait_ms.min(600_000));
            loop {
                let notified = hub.arrived.notified();
                let mine: Vec<Spooled> = {
                    let mut s = hub.spool.lock().unwrap();
                    let (mine, rest): (Vec<_>, Vec<_>) = s.msgs.drain(..).partition(|m| m.target == me_as);
                    s.msgs = rest;
                    if !mine.is_empty() {
                        hub.save_spool(&s);
                    }
                    mine
                };
                if !mine.is_empty() || tokio::time::Instant::now() >= deadline {
                    return Ok(json!({ "ok": true, "messages": mine }));
                }
                let _ = tokio::time::timeout_at(deadline, notified).await;
            }
        }
        op => bail!("unknown op {op}"),
    }
}

/// Client side: one request to the local daemon.
pub async fn call(req: Value) -> Result<Value> {
    let sock = socket_path();
    let mut s = UnixStream::connect(&sock)
        .await
        .map_err(|_| anyhow::anyhow!("letagents daemon is not running ({}); start it: letagents service install", sock.display()))?;
    s.write_all(&frame(&req)).await?;
    let v = read_frame(&mut s).await?;
    if v["ok"] != json!(true) {
        bail!("{}", v["error"].as_str().unwrap_or("daemon error"));
    }
    Ok(v)
}
