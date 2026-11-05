//! The daemon: one per device. It owns the keys, the contact rules and the relay connection, and serves
//! every local agent, N to M:
//!
//! - pi: joins the pi-intercom broker. Each contact is a session (`devbox`), and each agent on a contact's
//!   machine is a session too (`devbox/planner`), so agents just `send`/`ask` the name they want.
//!   Inbound mail is pushed straight into the target pi session; asks keep their ask/reply link.
//! - everything else: `~/.letagents/daemon.sock` (same framing as pi-intercom). `letagents mcp` and the CLI
//!   use it. Mail for an agent that is not on intercom waits in the spool until it reads its inbox.
//!
//! Primitives over the relay (all sealed): send, ask, reply, list -> roster (who is online), unsolicited
//! roster (presence), receipt (pushed / queued / refused), hello (contact request). Groups fan out locally.

use crate::crypto::{now_ms, open, Identity, Inner, Peer};
use crate::intercom::{frame, read_frame, stable_session_id, Local, Proxy};
use crate::store::{self, Seen};
use crate::{compose_and_send, relay};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, Notify};

/// The local intercom session that receives notices (contact requests).
pub const SYSTEM_SESSION: &str = "letagents";
const ROSTER_REFRESH: Duration = Duration::from_secs(600);
const PRESENCE_CHECK: Duration = Duration::from_secs(30);
/// MCP agents heartbeat every 60 s; one missed by 3 minutes has left.
const SOCKET_AGENT_TTL_MS: i64 = 3 * 60 * 1000;

pub fn socket_path() -> PathBuf {
    store::home().join("daemon.sock")
}

fn log(s: impl AsRef<str>) {
    let t = now_ms() / 1000 % 86400;
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

/// One agent on some machine, as shared in a roster.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Agent {
    pub name: String,
    pub harness: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub r#where: String,
}

/// Who is waiting for the reply to an ask we sealed.
enum Waiter {
    Intercom { proxy: String, local_from: String, local_msg: String },
    Socket { asker: String, tx: oneshot::Sender<Spooled> },
    Spool { asker: String },
}

/// Who wants the receipt for a message we sealed.
enum ReceiptWaiter {
    Intercom { proxy: String, local_from: String, to: String },
    Socket(oneshot::Sender<Value>),
}

struct Hub {
    me: Identity,
    default_target: String,
    spool: Mutex<Spool>,
    waiters: Mutex<HashMap<String, Waiter>>,
    receipts: Mutex<HashMap<String, ReceiptWaiter>>,
    list_waiters: Mutex<HashMap<String, oneshot::Sender<Vec<Agent>>>>,
    /// intercom message id of an ask we delivered locally -> (contact, remote ask id)
    remote_ask_by_local: Mutex<HashMap<String, (String, String)>>,
    /// remote agent address -> the local session that last wrote to it (routes its plain sends back)
    last_peer: Mutex<HashMap<String, String>>,
    /// contact -> its last roster
    rosters: Mutex<HashMap<String, Vec<Agent>>>,
    /// intercom message id of a local ask pushed to a pi session -> our ask id
    local_ask_by_intercom: Mutex<HashMap<String, String>>,
    /// local agents seen on the socket (MCP, CLI): name -> (harness, last seen)
    socket_agents: Mutex<HashMap<String, (String, i64)>>,
    /// contact proxies ("devbox") and remote-agent proxies ("devbox/planner")
    proxies: RwLock<HashMap<String, Arc<Proxy>>>,
    system: RwLock<Option<Arc<Proxy>>>,
    intercom_in: Mutex<Option<mpsc::UnboundedSender<Local>>>,
    arrived: Notify,
}

/// An agent's address: `<machine>/<agent>`. (`@` is reserved by pi-intercom for its own routing.)
fn agent_addr(agent: &str, contact: &str) -> String {
    format!("{contact}/{agent}")
}

/// `<contact>/<agent>`, `<contact>`, or `@<group>`.
enum To {
    Contact { contact: String, agent: Option<String> },
    /// An agent on this machine: delivered locally, never through the relay.
    Local(String),
    Group(String),
}

fn parse_to(s: &str, me: &str, local: &[String]) -> Result<To> {
    let s = s.trim();
    if let Some(g) = s.strip_prefix('@') {
        return Ok(To::Group(g.to_string()));
    }
    if let Some(agent) = s.strip_prefix(&format!("{me}/")) {
        return Ok(To::Local(agent.to_string()));
    }
    if local.iter().any(|a| a == s) {
        return Ok(To::Local(s.to_string()));
    }
    let contacts = store::contacts();
    if contacts.contains_key(s) {
        return Ok(To::Contact { contact: s.into(), agent: None });
    }
    if let Some((contact, agent)) = s.split_once('/') {
        if contacts.contains_key(contact) && !agent.is_empty() {
            return Ok(To::Contact { contact: contact.into(), agent: Some(agent.into()) });
        }
    }
    bail!("unknown address {s}: use <machine>/<agent>, <machine> or @<group> (see letagents list)")
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
        let cut = now_ms() - crate::crypto::MAX_AGE_MS;
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

    fn touch_socket_agent(&self, name: &str, harness: Option<&str>) {
        let mut a = self.socket_agents.lock().unwrap();
        let h = harness.map(str::to_string).or_else(|| a.get(name).map(|x| x.0.clone())).unwrap_or_else(|| "cli".into());
        a.insert(name.to_string(), (h, now_ms()));
    }

    /// Local agents: pi sessions on the broker plus recently active socket agents.
    async fn local_roster(&self) -> Vec<Agent> {
        let mut out = vec![];
        let system = self.system.read().unwrap().clone();
        if let Some(sys) = system {
            if let Ok(sessions) = sys.list().await {
                for s in sessions {
                    let name = s["name"].as_str().unwrap_or_default();
                    let model = s["model"].as_str().unwrap_or_default();
                    if name.is_empty() || name == "pi-intercom-cli" || name == SYSTEM_SESSION || model == "letagents" || s["runtimeFallbackAlias"] == json!(true) {
                        continue;
                    }
                    let cwd = s["cwd"].as_str().unwrap_or_default();
                    out.push(Agent {
                        name: name.into(),
                        harness: "pi".into(),
                        status: s["status"].as_str().unwrap_or("idle").into(),
                        r#where: cwd.rsplit('/').next().unwrap_or_default().into(),
                    });
                }
            }
        }
        let cut = now_ms() - SOCKET_AGENT_TTL_MS;
        for (name, (harness, seen)) in self.socket_agents.lock().unwrap().iter() {
            if *seen >= cut && !out.iter().any(|a| &a.name == name) {
                out.push(Agent { name: name.clone(), harness: harness.clone(), status: "inbox".into(), r#where: String::new() });
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Seal and send one message; returns its id.
    async fn seal_send(&self, contact: &str, kind: &str, body: &str, reply_to: Option<String>, from: Option<String>, to: Option<String>) -> Result<String> {
        compose_and_send(&self.me, contact, kind, body, reply_to, from, to).await
    }

    /// Tell the sender what happened to its message. Never for receipts, rosters or hellos.
    async fn receipt(&self, contact: &str, msg_id: &str, status: &str, detail: &str) {
        let body = json!({ "status": status, "detail": detail }).to_string();
        if let Err(e) = self.seal_send(contact, "receipt", &body, Some(msg_id.into()), None, None).await {
            log(format!("receipt -> {contact} failed: {e}"));
        }
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
        receipts: Mutex::default(),
        list_waiters: Mutex::default(),
        remote_ask_by_local: Mutex::default(),
        last_peer: Mutex::default(),
        rosters: Mutex::default(),
        local_ask_by_intercom: Mutex::default(),
        socket_agents: Mutex::default(),
        proxies: RwLock::default(),
        system: RwLock::default(),
        intercom_in: Mutex::default(),
        arrived: Notify::new(),
    });
    log(format!("socket {} ready; default target \"{}\"", sock.display(), hub.default_target));

    tokio::spawn(intercom_manager(hub.clone()));
    tokio::spawn(relay_inbox(hub.clone()));
    tokio::spawn(roster_loop(hub.clone()));
    tokio::spawn(presence_loop(hub.clone()));
    loop {
        let (conn, _) = listener.accept().await?;
        tokio::spawn(serve_socket(hub.clone(), conn));
    }
}

// ---------- pi-intercom adapter ----------

/// Keeps the system session and one session per contact registered on the local broker, reconnecting
/// whenever the broker restarts. Remote-agent sessions are added by `sync_agent_proxies`.
async fn intercom_manager(hub: Arc<Hub>) {
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Local>();
    *hub.intercom_in.lock().unwrap() = Some(in_tx.clone());
    {
        let hub = hub.clone();
        tokio::spawn(async move {
            while let Some(m) = in_rx.recv().await {
                let hub = hub.clone();
                tokio::spawn(async move { from_intercom(&hub, m).await });
            }
        });
    }
    let mut announced = false;
    loop {
        let (closed_tx, mut closed_rx) = mpsc::unbounded_channel::<String>();
        match Proxy::connect(SYSTEM_SESSION, &stable_session_id(&format!("system:{}", hub.me.id())), in_tx.clone(), closed_tx.clone()).await {
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
        let rosters: Vec<(String, Vec<Agent>)> = hub.rosters.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (contact, agents) in rosters {
            sync_agent_proxies(&hub, &contact, &agents).await;
        }
        let gone = closed_rx.recv().await.unwrap_or_default();
        log(format!("pi-intercom connection \"{gone}\" closed; reconnecting"));
        for p in hub.proxies.write().unwrap().drain() {
            p.1.close();
        }
        *hub.system.write().unwrap() = None;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Make every agent in `contact`'s roster a local intercom session `contact/agent`, and drop gone ones.
async fn sync_agent_proxies(hub: &Arc<Hub>, contact: &str, agents: &[Agent]) {
    let Some(in_tx) = hub.intercom_in.lock().unwrap().clone() else { return };
    if hub.system.read().unwrap().is_none() {
        return;
    }
    let want: HashSet<String> = agents.iter().map(|a| agent_addr(&a.name, contact)).collect();
    let prefix = format!("{contact}/");
    let stale: Vec<String> = hub.proxies.read().unwrap().keys().filter(|k| k.starts_with(&prefix) && !want.contains(*k)).cloned().collect();
    for k in stale {
        if let Some(p) = hub.proxies.write().unwrap().remove(&k) {
            p.close();
        }
    }
    let Some(address) = store::contacts().get(contact).map(|c| c.address.clone()) else { return };
    for addr in want {
        if hub.proxies.read().unwrap().contains_key(&addr) {
            continue;
        }
        // Remote-agent sessions closing is normal; a separate channel keeps it from looking like a broker drop.
        let (ignore_tx, _ignore_rx) = mpsc::unbounded_channel::<String>();
        match Proxy::connect(&addr, &stable_session_id(&format!("agent:{address}:{addr}")), in_tx.clone(), ignore_tx).await {
            Ok(p) => {
                hub.proxies.write().unwrap().insert(addr.clone(), p);
            }
            Err(e) => log(format!("intercom session \"{addr}\" failed: {e}")),
        }
    }
}

/// A local pi agent sent or asked a contact or remote-agent session, or replied to a forwarded ask.
async fn from_intercom(hub: &Hub, m: Local) {
    if m.proxy == SYSTEM_SESSION {
        // A pi agent answered a local ask that we pushed into its session.
        let ask = m.reply_to.as_ref().and_then(|r| hub.local_ask_by_intercom.lock().unwrap().remove(r));
        if let Some(ask_id) = ask {
            let from = if m.from_name.is_empty() { m.from_id.clone() } else { m.from_name.clone() };
            let waiter = hub.waiters.lock().unwrap().remove(&ask_id);
            let reply = Spooled { id: uuid::Uuid::new_v4().to_string(), from: agent_addr(&from, &hub.me.name), kind: "reply".into(), target: hub.default_target.clone(), text: m.text, ts: now_ms(), reply_to: Some(ask_id) };
            resolve_waiter(hub, waiter, reply).await;
        }
        return;
    }
    let from_agent = if m.from_name.is_empty() { m.from_id.clone() } else { m.from_name.clone() };
    let link = m.reply_to.as_ref().and_then(|r| hub.remote_ask_by_local.lock().unwrap().remove(r));
    if let Some((contact, remote_id)) = link {
        return match hub.seal_send(&contact, "reply", &m.text, Some(remote_id), Some(from_agent), None).await {
            Ok(_) => log(format!("out reply -> {contact} (from intercom)")),
            Err(e) => log(format!("out reply -> {contact} FAILED: {e}")),
        };
    }
    let (contact, mut to_agent) = match m.proxy.split_once('/') {
        Some((contact, agent)) => (contact.to_string(), Some(agent.to_string())),
        None => (m.proxy.clone(), None),
    };
    let mut body = m.text.clone();
    if to_agent.is_none() {
        if let Some((s, b)) = m.text.trim_start().strip_prefix("to:").and_then(|r| r.split_once('\n')) {
            to_agent = Some(s.trim().to_string());
            body = b.trim().to_string();
        }
    }
    let kind = if m.expects_reply { "ask" } else { "send" };
    if let Some(a) = &to_agent {
        hub.last_peer.lock().unwrap().insert(agent_addr(a, &contact), m.from_id.clone());
    }
    let dest = to_agent.clone().map(|a| agent_addr(&a, &contact)).unwrap_or_else(|| contact.clone());
    match hub.seal_send(&contact, kind, &body, None, Some(from_agent), to_agent).await {
        Ok(id) => {
            if kind == "ask" {
                hub.waiters.lock().unwrap().insert(id.clone(), Waiter::Intercom { proxy: m.proxy.clone(), local_from: m.from_id.clone(), local_msg: m.id.clone() });
            }
            hub.receipts.lock().unwrap().insert(id, ReceiptWaiter::Intercom { proxy: m.proxy.clone(), local_from: m.from_id.clone(), to: dest.clone() });
            log(format!("out {kind} -> {dest} (from intercom)"));
        }
        Err(e) => {
            log(format!("out -> {dest} FAILED: {e}"));
            if let Some(p) = hub.proxy(&m.proxy) {
                let _ = p.send(&m.from_id, &format!("[letagents] delivery to {dest} failed: {e}"), None, false).await;
            }
        }
    }
}

// ---------- rosters and presence ----------

/// Ask every contact who is online, now and every 10 minutes.
async fn roster_loop(hub: Arc<Hub>) {
    tokio::time::sleep(Duration::from_secs(3)).await;
    loop {
        for contact in store::contacts().keys() {
            let _ = request_roster(&hub, contact).await;
        }
        tokio::time::sleep(ROSTER_REFRESH).await;
    }
}

async fn request_roster(hub: &Hub, contact: &str) -> Result<String> {
    hub.seal_send(contact, "list", "", None, None, None).await
}

/// Push our roster to contacts allowed to list us: on the first check after start and whenever the set
/// of local agents changes.
async fn presence_loop(hub: Arc<Hub>) {
    let mut last: Option<Vec<(String, String)>> = None;
    loop {
        tokio::time::sleep(PRESENCE_CHECK).await;
        let roster = hub.local_roster().await;
        let key: Vec<(String, String)> = roster.iter().map(|a| (a.name.clone(), a.harness.clone())).collect();
        if last.as_ref() == Some(&key) {
            continue;
        }
        last = Some(key);
        let body = serde_json::to_string(&roster).unwrap_or_default();
        for (name, c) in store::contacts() {
            if c.allow.iter().any(|k| k == "list") {
                if let Err(e) = hub.seal_send(&name, "roster", &body, None, None, None).await {
                    log(format!("presence -> {name} failed: {e}"));
                }
            }
        }
        log(format!("presence: pushed {} agents", roster.len()));
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
                tokio::spawn(async move { inbound(hub, inner, sender).await });
            }
        }
    })
    .await
}

async fn inbound(hub: Arc<Hub>, inner: Inner, sender: Peer) {
    let from_agent = inner.from_agent.clone().unwrap_or_else(|| "agent".into());
    match inner.kind.as_str() {
        "hello" => return hello(&hub, inner, sender).await,
        "receipt" | "roster" => {
            let Ok((contact, _)) = store::contact_of(&sender) else { return };
            return if inner.kind == "receipt" { on_receipt(&hub, &contact, inner).await } else { on_roster(&hub, &contact, inner).await };
        }
        _ => {}
    }
    let to_local = match inner.kind.as_str() {
        "list" | "reply" => "*".to_string(),
        _ => target_for(&hub, &inner, &sender),
    };
    let (contact, _) = match store::authorize(&sender, &inner.kind, &from_agent, &to_local) {
        Ok(x) => x,
        Err(reason) => {
            log(format!("REFUSED {} {} from {from_agent}: {reason}", inner.kind, inner.id));
            if let Ok((contact, _)) = store::contact_of(&sender) {
                hub.receipt(&contact, &inner.id, "refused", &reason).await;
            }
            return;
        }
    };
    let from_addr = agent_addr(&from_agent, &contact);
    match inner.kind.as_str() {
        "list" => {
            let roster = hub.local_roster().await;
            let body = serde_json::to_string(&roster).unwrap_or_default();
            match hub.seal_send(&contact, "roster", &body, Some(inner.id.clone()), None, None).await {
                Ok(_) => log(format!("roster -> {contact} ({} agents)", roster.len())),
                Err(e) => log(format!("roster -> {contact} failed: {e}")),
            }
        }
        "reply" => on_reply(&hub, &from_addr, inner).await,
        _ => deliver(&hub, &contact, &from_agent, &from_addr, to_local, inner).await,
    }
}

/// The local agent a send/ask is for: explicit target, else whoever last wrote to that remote agent,
/// else the contact's default, else the daemon's default.
fn target_for(hub: &Hub, inner: &Inner, sender: &Peer) -> String {
    if let Some(t) = &inner.to_agent {
        return t.clone();
    }
    let contact = store::contact_of(sender).ok();
    if let (Some((name, _)), Some(fa)) = (&contact, &inner.from_agent) {
        if let Some(local) = hub.last_peer.lock().unwrap().get(&agent_addr(fa, name)).cloned() {
            return local;
        }
    }
    contact.and_then(|(_, c)| c.session).unwrap_or_else(|| hub.default_target.clone())
}

async fn deliver(hub: &Arc<Hub>, contact: &str, from_agent: &str, from_addr: &str, target: String, inner: Inner) {
    let ask = inner.kind == "ask";
    // Prefer the remote agent's own session, so a plain answer goes back to that agent.
    let proxy = hub.proxy(from_addr).or_else(|| hub.proxy(contact));
    if let Some(proxy) = proxy {
        let header = if ask {
            format!("[letagents · verified {from_addr}] ASK: reply to this message to answer it")
        } else {
            format!("[letagents · verified {from_addr}] to answer, send to \"{from_addr}\"")
        };
        if let Ok(s) = proxy.send(&target, &format!("{header}\n\n{}", inner.body), None, ask).await {
            if s.delivered {
                if ask {
                    hub.remote_ask_by_local.lock().unwrap().insert(s.id, (contact.to_string(), inner.id.clone()));
                }
                log(format!("in {} from {from_addr} -> {target}: pushed via intercom", inner.kind));
                hub.receipt(contact, &inner.id, "pushed", &target).await;
                return;
            }
        }
    }
    let _ = from_agent;
    hub.spool(
        Spooled { id: inner.id.clone(), from: from_addr.to_string(), kind: inner.kind.clone(), target: target.clone(), text: inner.body.clone(), ts: inner.ts, reply_to: None },
        if ask { Some(contact) } else { None },
    );
    log(format!("in {} from {from_addr} -> {target}: queued", inner.kind));
    hub.receipt(contact, &inner.id, "queued", &target).await;
}

async fn on_reply(hub: &Hub, from_addr: &str, inner: Inner) {
    let waiter = inner.reply_to.as_ref().and_then(|r| hub.waiters.lock().unwrap().remove(r));
    let reply = Spooled { id: inner.id.clone(), from: from_addr.into(), kind: "reply".into(), target: hub.default_target.clone(), text: inner.body.clone(), ts: inner.ts, reply_to: inner.reply_to.clone() };
    resolve_waiter(hub, waiter, reply).await;
}

async fn resolve_waiter(hub: &Hub, waiter: Option<Waiter>, reply: Spooled) {
    let from = reply.from.clone();
    match waiter {
        Some(Waiter::Intercom { proxy, local_from, local_msg }) => {
            let ok = match hub.proxy(&proxy) {
                Some(p) => p.send(&local_from, &reply.text, Some(&local_msg), false).await.map(|s| s.delivered).unwrap_or(false),
                None => false,
            };
            if !ok {
                hub.spool(Spooled { target: local_from, ..reply }, None);
            }
            log(format!("in reply from {from} -> intercom asker: {}", if ok { "delivered" } else { "queued" }));
        }
        Some(Waiter::Socket { asker, tx }) => {
            if let Err(reply) = tx.send(reply) {
                hub.spool(Spooled { target: asker, ..reply }, None);
            }
            log(format!("in reply from {from} -> socket asker"));
        }
        Some(Waiter::Spool { asker }) => {
            hub.spool(Spooled { target: asker.clone(), ..reply }, None);
            log(format!("in reply from {from} -> queued for {asker}"));
        }
        None => {
            hub.spool(reply, None);
            log(format!("in reply from {from}: no open ask; queued for {}", hub.default_target));
        }
    }
}

async fn on_receipt(hub: &Hub, contact: &str, inner: Inner) {
    let Some(of) = inner.reply_to.clone() else { return };
    let v: Value = serde_json::from_str(&inner.body).unwrap_or(json!({}));
    let status = v["status"].as_str().unwrap_or("?").to_string();
    let detail = v["detail"].as_str().unwrap_or_default().to_string();
    log(format!("receipt from {contact}: {status} {detail}"));
    let waiter = hub.receipts.lock().unwrap().remove(&of);
    match waiter {
        Some(ReceiptWaiter::Socket(tx)) => {
            let _ = tx.send(json!({ "status": status, "detail": detail }));
        }
        Some(ReceiptWaiter::Intercom { proxy, local_from, to }) if status == "refused" => {
            if let Some(p) = hub.proxy(&proxy) {
                let _ = p.send(&local_from, &format!("[letagents] {to} refused your message: {detail}"), None, false).await;
            }
        }
        _ => {}
    }
    if status == "refused" {
        // A refused ask will never get a reply: answer it now so the asker is not left waiting.
        let waiter = hub.waiters.lock().unwrap().remove(&of);
        if waiter.is_some() {
            let reply = Spooled { id: inner.id, from: contact.into(), kind: "reply".into(), target: hub.default_target.clone(), text: format!("[letagents] refused: {detail}"), ts: now_ms(), reply_to: Some(of) };
            resolve_waiter(hub, waiter, reply).await;
        }
    }
}

async fn on_roster(hub: &Arc<Hub>, contact: &str, inner: Inner) {
    let agents: Vec<Agent> = serde_json::from_str(&inner.body).unwrap_or_default();
    let agents: Vec<Agent> = agents.into_iter().filter(|a| !a.name.is_empty() && !a.name.contains('/') && !a.name.contains('@') && a.name.len() <= 80).collect();
    hub.rosters.lock().unwrap().insert(contact.to_string(), agents.clone());
    if let Some(tx) = inner.reply_to.as_ref().and_then(|r| hub.list_waiters.lock().unwrap().remove(r)) {
        let _ = tx.send(agents.clone());
    }
    log(format!("roster from {contact}: {}", agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")));
    sync_agent_proxies(hub, contact, &agents).await;
}

async fn hello(hub: &Hub, inner: Inner, sender: Peer) {
    if store::contact_of(&sender).is_ok() {
        return log("hello from a known contact");
    }
    let (rid, req) = store::record_request(&sender, &inner.body, &inner.id, inner.ts);
    log(format!("contact request {rid} from \"{}\" ({})", req.name, req.safety));
    let note = format!(
        "[letagents] contact request {rid} from an agent calling itself \"{}\", safety number {}.\nAsk your human to confirm this safety number with them, then run: letagents contact accept {rid} <name>",
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
    /// the local agent making the request
    #[serde(default, rename = "as")]
    as_: Option<String>,
    #[serde(default)]
    harness: Option<String>,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    contact: Option<String>,
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

/// Every agent this machine can reach, as `contact/agent`, refreshed from contacts within `wait`.
async fn directory(hub: &Hub, only: Option<&str>, wait: Duration) -> Value {
    let contacts: Vec<String> = store::contacts().keys().filter(|c| only.is_none_or(|o| o == c.as_str())).cloned().collect();
    let mut pending = vec![];
    for c in &contacts {
        let (tx, rx) = oneshot::channel();
        if let Ok(id) = request_roster(hub, c).await {
            hub.list_waiters.lock().unwrap().insert(id, tx);
            pending.push(rx);
        }
    }
    let _ = tokio::time::timeout(wait, futures_util::future::join_all(pending)).await;
    let rosters = hub.rosters.lock().unwrap().clone();
    let mut remote = vec![];
    for c in &contacts {
        for a in rosters.get(c).cloned().unwrap_or_default() {
            remote.push(json!({ "address": agent_addr(&a.name, c), "harness": a.harness, "status": a.status, "where": a.r#where }));
        }
    }
    let groups: BTreeMap<String, Vec<String>> = store::groups();
    json!({ "ok": true, "me": hub.me.name, "local": hub.local_roster().await, "remote": remote,
            "groups": groups.keys().map(|g| format!("@{g}")).collect::<Vec<_>>() })
}

/// Deliver to an agent on this machine: push into its pi session, else queue it in its inbox.
async fn deliver_local(hub: &Hub, from: &str, agent: &str, kind: &str, text: &str, id: &str) -> Value {
    let from_addr = agent_addr(from, &hub.me.name);
    let system = hub.system.read().unwrap().clone();
    if let Some(sys) = system {
        let header = if kind == "ask" {
            format!("[letagents · {from_addr}] ASK: reply to this message to answer it")
        } else {
            format!("[letagents · {from_addr}] to answer, send to \"{from}\"")
        };
        if let Ok(s) = sys.send(agent, &format!("{header}\n\n{text}"), None, kind == "ask").await {
            if s.delivered {
                if kind == "ask" {
                    hub.local_ask_by_intercom.lock().unwrap().insert(s.id, id.to_string());
                }
                log(format!("local {kind} {from} -> {agent}: pushed via intercom"));
                return json!({ "status": "pushed", "detail": agent });
            }
        }
    }
    hub.spool(
        Spooled { id: id.to_string(), from: from_addr, kind: kind.into(), target: agent.into(), text: text.into(), ts: now_ms(), reply_to: None },
        if kind == "ask" { Some("") } else { None },
    );
    log(format!("local {kind} {from} -> {agent}: queued"));
    json!({ "status": "queued", "detail": agent })
}

async fn handle(hub: &Hub, r: Req) -> Result<Value> {
    let me_as = r.as_.clone().unwrap_or_else(|| hub.default_target.clone());
    if r.as_.is_some() && r.op != "bye" {
        hub.touch_socket_agent(&me_as, r.harness.as_deref());
    }
    match r.op.as_str() {
        "hello" => Ok(json!({ "ok": true, "as": me_as, "machine": hub.me.name })),
        "bye" => {
            hub.socket_agents.lock().unwrap().remove(&me_as);
            log(format!("{me_as} left"));
            Ok(json!({ "ok": true }))
        }
        "status" => {
            let s = hub.spool.lock().unwrap();
            Ok(json!({
                "ok": true, "name": hub.me.name, "address": hub.me.peer().address(true),
                "safety": hub.me.peer().safety_number(), "default_target": hub.default_target,
                "intercom": hub.system.read().unwrap().is_some(), "waiting": s.msgs.len(),
            }))
        }
        "list" => Ok(directory(hub, r.contact.as_deref(), Duration::from_millis(if r.wait_ms == 0 { 6000 } else { r.wait_ms.min(20_000) })).await),
        "send" | "ask" => {
            let local: Vec<String> = hub.local_roster().await.into_iter().map(|a| a.name).collect();
            let targets: Vec<String> = match parse_to(&r.to, &hub.me.name, &local)? {
                To::Group(g) => store::groups().get(&g).cloned().ok_or_else(|| anyhow!("no group @{g}"))?,
                _ => vec![r.to.clone()],
            };
            let mut results = vec![];
            let mut replies = vec![];
            for t in targets {
                let id = match parse_to(&t, &hub.me.name, &local)? {
                    To::Group(_) => bail!("groups cannot contain groups"),
                    To::Local(agent) => {
                        let id = uuid::Uuid::new_v4().to_string();
                        if r.op == "ask" {
                            let (tx, rx) = oneshot::channel();
                            hub.waiters.lock().unwrap().insert(id.clone(), Waiter::Socket { asker: me_as.clone(), tx });
                            replies.push((t.clone(), id.clone(), rx));
                        }
                        let receipt = deliver_local(hub, &me_as, &agent, &r.op, &r.text, &id).await;
                        let (rtx, rrx) = oneshot::channel();
                        let _ = rtx.send(receipt);
                        results.push((t, id, rrx));
                        continue;
                    }
                    To::Contact { contact, agent } => {
                        if let Some(a) = &agent {
                            hub.last_peer.lock().unwrap().insert(agent_addr(a, &contact), me_as.clone());
                        }
                        hub.seal_send(&contact, &r.op, &r.text, None, Some(me_as.clone()), agent).await?
                    }
                };
                log(format!("out {} -> {t} (from socket, as {me_as})", r.op));
                let (rtx, rrx) = oneshot::channel();
                hub.receipts.lock().unwrap().insert(id.clone(), ReceiptWaiter::Socket(rtx));
                if r.op == "ask" {
                    let (tx, rx) = oneshot::channel();
                    hub.waiters.lock().unwrap().insert(id.clone(), Waiter::Socket { asker: me_as.clone(), tx });
                    replies.push((t.clone(), id.clone(), rx));
                }
                results.push((t, id, rrx));
            }
            // Receipts arrive in about a second; wait briefly so the caller knows pushed / queued / refused.
            let mut sent = vec![];
            for (t, id, rrx) in results {
                let receipt = tokio::time::timeout(Duration::from_secs(5), rrx).await.ok().and_then(|r| r.ok()).unwrap_or(json!({ "status": "sent" }));
                sent.push(json!({ "to": t, "msg_id": id, "receipt": receipt }));
            }
            if r.op == "send" {
                return Ok(json!({ "ok": true, "sent": sent }));
            }
            let wait = Duration::from_millis(if r.timeout_ms == 0 { 50_000 } else { r.timeout_ms.min(600_000) });
            let deadline = tokio::time::Instant::now() + wait;
            let mut out = vec![];
            for (t, id, rx) in replies {
                match tokio::time::timeout_at(deadline, rx).await {
                    Ok(Ok(reply)) => out.push(json!({ "from": t, "reply": reply })),
                    _ => {
                        let mut w = hub.waiters.lock().unwrap();
                        if w.contains_key(&id) {
                            w.insert(id.clone(), Waiter::Spool { asker: me_as.clone() });
                        }
                        out.push(json!({ "from": t, "msg_id": id, "timeout": true, "note": "no reply yet; it will arrive in your inbox" }));
                    }
                }
            }
            Ok(json!({ "ok": true, "sent": sent, "replies": out }))
        }
        "reply" => {
            let (contact, _) = hub.spool.lock().unwrap().answerable.remove(&r.msg).ok_or_else(|| anyhow!("no open ask with id {}", r.msg))?;
            hub.save_spool(&hub.spool.lock().unwrap());
            if contact.is_empty() {
                // A local ask: hand the reply straight to the asker.
                let waiter = hub.waiters.lock().unwrap().remove(&r.msg);
                let reply = Spooled { id: uuid::Uuid::new_v4().to_string(), from: agent_addr(&me_as, &hub.me.name), kind: "reply".into(), target: hub.default_target.clone(), text: r.text.clone(), ts: now_ms(), reply_to: Some(r.msg.clone()) };
                resolve_waiter(hub, waiter, reply).await;
                return Ok(json!({ "ok": true, "msg_id": r.msg }));
            }
            let id = hub.seal_send(&contact, "reply", &r.text, Some(r.msg.clone()), Some(me_as), None).await?;
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
        .map_err(|_| anyhow!("letagents daemon is not running ({}); start it: letagents service install", sock.display()))?;
    s.write_all(&frame(&req)).await?;
    let v = read_frame(&mut s).await?;
    if v["ok"] != json!(true) {
        bail!("{}", v["error"].as_str().unwrap_or("daemon error"));
    }
    Ok(v)
}
