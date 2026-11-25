//! The daemon: one per device. It owns the keys, the contact rules and the relay connection, and serves
//! every local agent, N to M:
//!
//! - pi: joins the pi-intercom broker. Each contact is a session (`devbox`), and each agent on a contact's
//!   machine is a session too (`devbox/planner`), so agents just `send`/`ask` the name they want.
//!   Inbound mail is pushed straight into the target pi session; asks keep their ask/reply link.
//! - everything else: `~/.letagents/daemon.sock` (same framing as pi-intercom). `letagents mcp` and the CLI
//!   use it. Mail is pushed into the session when its harness allows it, else it waits in the spool until
//!   the agent reads its inbox:
//!   - Claude Code: `letagents mcp` holds a `watch` connection and posts each message to its own session's
//!     inbox socket (Claude Code cross-session messaging); an idle session starts a turn.
//!   - Codex: open threads of the shared Codex app-server are local agents `codex-<id>`; a message starts a
//!     turn on that thread (`turn/start`).
//!
//! Primitives over the relay (all sealed): send, ask, reply, list -> roster (who is online), unsolicited
//! roster (presence), receipt (pushed / queued / refused), hello (contact request). Groups fan out locally.
//!
//! Routing never guesses a live session: a send/ask must name an agent (or come back to the agent that
//! last wrote to its sender, or use the contact's configured session); a reply goes to whoever asked, by
//! ask id, even after a restart. Anything else is refused, or waits in the machine inbox.

use crate::codex;
use crate::crypto::{now_ms, open, Identity, Inner, Peer};
use crate::intercom::{frame, read_frame, stable_session_id, Local, Proxy};
use crate::store::{self, Seen};
use crate::{compose_and_send, drops, relay};
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
/// Not "letagents": a pi session in a project named letagents must stay visible and reachable.
pub const SYSTEM_SESSION: &str = "letagents-daemon";
/// Mail waiting for a live session is pushed again this often.
const REDELIVER_EVERY: Duration = Duration::from_secs(15);
/// The machine inbox: callers with no agent name (plain CLI) and replies nobody asked for. Never pushed.
pub const MACHINE_INBOX: &str = "inbox";
/// A remote agent's plain send goes back to the local agent that wrote to it within this window.
const LAST_PEER_TTL_MS: i64 = 60 * 60 * 1000;
/// Rosters are event-driven (docs/DECISIONS.md #14): a `list` at start, when the relay is reached again, for a
/// new contact and for a new drop code; presence pushes on change. This slow refresh is a safety net only.
const ROSTER_REFRESH: Duration = Duration::from_secs(6 * 3600);
/// How often the set of local agents is checked (local only: nothing is sent unless it changed).
const PRESENCE_CHECK: Duration = Duration::from_secs(30);
/// A contact we have not heard from for this long may be offline: it gets one roster push, then the rest
/// waits until it writes (it asks for a `list` when it comes back).
const SILENT: Duration = Duration::from_secs(30 * 60);
/// At most one presence push per contact this often; the last state wins.
const PRESENCE_GAP: Duration = Duration::from_secs(60);
/// An unsolicited roster is answered (echo_presence) after this delay, if still needed.
const ECHO_DELAY: Duration = Duration::from_secs(3);
/// When the relay is reached again, a contact gets a `list` at most this often.
const REACH_LIST_GAP: Duration = Duration::from_secs(10 * 60);

/// `d` for this daemon. Tests only: LETAGENTS_TEST_SPEEDUP=<n> divides the roster and presence intervals
/// above by n (tests/cost.sh runs an hour in a few minutes). Never set by the service.
fn timing(d: Duration) -> Duration {
    static SPEEDUP: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    let n = *SPEEDUP.get_or_init(|| std::env::var("LETAGENTS_TEST_SPEEDUP").ok().and_then(|v| v.parse().ok()).filter(|n| *n >= 1).unwrap_or(1));
    d / n
}

fn ms(d: Duration) -> i64 {
    timing(d).as_millis() as i64
}
/// MCP agents heartbeat every 60 s; one missed by 3 minutes has left.
const SOCKET_AGENT_TTL_MS: i64 = 3 * 60 * 1000;
/// A contact's roster becomes local intercom sessions; at most this many per contact.
const MAX_ROSTER: usize = 64;

/// An agent name a contact may use (roster entries): no routing characters, no control characters.
fn valid_agent_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 80 && !n.contains(['/', '@']) && !n.chars().any(char::is_control)
}

/// A target a contact may not name: our own intercom sessions (contacts, their agents, the system session)
/// and pi-intercom's routing syntax. Delivering there would make this machine forward the text to a third
/// party, signed as itself.
fn reserved_target(hub: &Hub, t: &str) -> bool {
    !valid_agent_name(t) || t == SYSTEM_SESSION || store::contacts().contains_key(t) || hub.proxy(t).is_some()
}

/// True when a broker message comes from one of our own sessions: the daemon must never forward it.
fn own_session(hub: &Hub, id: &str) -> bool {
    hub.system.read().unwrap().as_ref().is_some_and(|p| p.session_id == id) || hub.proxies.read().unwrap().values().any(|p| p.session_id == id)
}

/// The binding check for every message a local agent writes (socket, MCP server, pi-intercom adapter).
/// `to`: the contact it goes to, or None for an agent on this machine. A bound agent writes only to its
/// contact; an exclusive contact takes messages only from its bound agent. Err: the reason, for the sender.
fn may_write(all: &store::Contacts, me_as: &str, to: Option<&str>) -> std::result::Result<(), String> {
    if let Some((c, _)) = store::bound_to(me_as, all) {
        if to != Some(c.as_str()) {
            let what = to.map(|t| format!("contact {t}")).unwrap_or_else(|| "agents on this machine".into());
            return Err(format!("{me_as} is bound to contact {c}: it may write only to {c}/<agent>, not to {what}"));
        }
    }
    if let Some(t) = to {
        if let Some(b) = all.get(t).and_then(|c| c.bind.as_ref()).filter(|b| b.exclusive && b.agent != me_as) {
            return Err(format!("contact {t} is bound to {} only (exclusive): other agents may not write to it", b.agent));
        }
    }
    Ok(())
}

/// The roster a contact may see: agents bound to another contact are left out.
fn roster_for(roster: &[Agent], contact: &str, all: &store::Contacts) -> Vec<Agent> {
    roster.iter().filter(|a| store::bound_to(&a.name, all).is_none_or(|(c, _)| c == contact)).cloned().collect()
}

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
    /// our ask id -> who asked; routes a reply that comes after a timeout or a daemon restart
    #[serde(default)]
    asks: BTreeMap<String, Asker>,
}

/// The local agent that sent an ask. `proxy` is set when it asked through pi-intercom: then `agent` is
/// its intercom session id and the reply goes back through that proxy session.
#[derive(Clone, Serialize, Deserialize, Debug)]
struct Asker {
    agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proxy: Option<String>,
    ts: i64,
    /// The contact it was sent to (None: a local ask, or recorded by a version before 0.6.0)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    contact: Option<String>,
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

/// A message for a watching MCP server to post into its session; `done` gets whether it was posted.
struct Push {
    msg: Value,
    done: oneshot::Sender<bool>,
}

/// The label that names the sender: `[letagents · verified <machine>/<agent> · note: <note>]`. `verified`
/// marks another machine (its key signed it); the note is the human's own reminder about that contact.
fn label(from: &str, verified: bool, note: Option<&str>) -> String {
    let v = if verified { "verified " } else { "" };
    match note {
        Some(n) => format!("[letagents · {v}{from} · note: {n}]"),
        None => format!("[letagents · {v}{from}]"),
    }
}

/// The note kept for the contact of a remote address (`<contact>/<agent>` or `<contact>`).
fn note_of(from: &str) -> Option<String> {
    let contact = from.split('/').next()?;
    store::contacts().get(contact)?.note.clone()
}

/// The text an agent sees for a message: a short header naming the sender once, then the body.
/// The MCP tool descriptions say how to answer.
fn render(m: &Spooled, machine: &str, note: Option<&str>) -> String {
    let local = m.from.starts_with(&format!("{machine}/"));
    let who = label(&m.from, !local, if local { None } else { note });
    match m.kind.as_str() {
        "ask" => format!("{who} ASK {}\n\n{}", m.id, m.text),
        "reply" => format!("{who} REPLY {}\n\n{}", m.reply_to.as_deref().unwrap_or("?"), m.text),
        "notice" => m.text.clone(),
        _ => format!("{who}\n\n{}", m.text),
    }
}

/// What this daemon knows about talking with one contact. Times are wall-clock ms (a monotonic clock stops
/// while a laptop sleeps, and silence must count that time).
#[derive(Default)]
struct PeerState {
    /// When the last verified envelope from it was written (its clock, at most our now; 0: none since start).
    heard: i64,
    /// When we got the last verified envelope from it (our clock).
    got: i64,
    /// Last envelope we sent it.
    sent: i64,
    /// Last `list` we sent it.
    listed: i64,
    /// The roster (name, harness) it last got from us, and when.
    pushed: Option<Vec<(String, String)>>,
    pushed_at: i64,
    /// It is silent and already got its one push: hold the rest until it writes.
    held: bool,
}

impl PeerState {
    fn silent(&self, now: i64) -> bool {
        now - self.heard > ms(SILENT)
    }

    /// Should it get a `list` now, for `why`?
    fn list_due(&self, why: Why, now: i64) -> bool {
        let unanswered = self.listed > self.got && now - self.listed < crate::crypto::MAX_AGE_MS;
        match why {
            Why::Now => true,
            Why::Refresh => !unanswered,
            Why::Reach => !unanswered && now - self.listed >= ms(REACH_LIST_GAP),
        }
    }

    /// Should it get a presence push of roster `key` now? Some(silent) if yes.
    fn push_due(&self, key: &[(String, String)], now: i64) -> Option<bool> {
        if self.pushed.as_deref() == Some(key) || now - self.pushed_at < ms(PRESENCE_GAP) {
            return None;
        }
        let silent = self.silent(now);
        if silent && self.held {
            return None;
        }
        Some(silent)
    }
}

/// Why we ask a contact for its roster.
#[derive(Clone, Copy, PartialEq)]
enum Why {
    /// A new contact, a new drop code, or a person asked: always.
    Now,
    /// The relay was reached again: not if we asked it within REACH_LIST_GAP, nor while an earlier list is
    /// unanswered (it waits in the contact's mailbox; the contact asks us when it comes back).
    Reach,
    /// The slow safety refresh: not while an earlier list is unanswered.
    Refresh,
}

/// presence.json: contact -> (its mailbox id, the roster it last got from us). A restart then pushes only
/// what changed. A contact that lost our roster asks for it (`list` at its start).
type Pushed = BTreeMap<String, (String, Vec<(String, String)>)>;

fn mailbox_of(c: &store::Contact) -> Option<String> {
    crate::crypto::Peer::parse(&c.address).ok().map(|p| p.id())
}

fn load_pushed() -> HashMap<String, PeerState> {
    let all = store::contacts();
    let saved: Pushed = store::read("presence.json");
    saved
        .into_iter()
        .filter(|(n, (id, _))| all.get(n).and_then(mailbox_of).as_ref() == Some(id))
        .map(|(n, (_, key))| (n, PeerState { pushed: Some(key), ..Default::default() }))
        .collect()
}

fn save_pushed(peers: &HashMap<String, PeerState>) {
    let all = store::contacts();
    let out: Pushed = peers
        .iter()
        .filter_map(|(n, p)| Some((n.clone(), (all.get(n).and_then(mailbox_of)?, p.pushed.clone()?))))
        .collect();
    let _ = store::write("presence.json", &out);
}

fn roster_key(r: &[Agent]) -> Vec<(String, String)> {
    r.iter().map(|a| (a.name.clone(), a.harness.clone())).collect()
}

/// Who wants the receipt for a message we sealed.
enum ReceiptWaiter {
    Intercom { proxy: String, local_from: String, to: String },
    Socket(oneshot::Sender<Value>),
}

struct Hub {
    me: Identity,
    /// Local agent told about contact requests (they also wait in the machine inbox)
    notify: Option<String>,
    spool: Mutex<Spool>,
    waiters: Mutex<HashMap<String, Waiter>>,
    receipts: Mutex<HashMap<String, ReceiptWaiter>>,
    list_waiters: Mutex<HashMap<String, oneshot::Sender<Vec<Agent>>>>,
    /// intercom message id of an ask we delivered locally -> (contact, remote ask id)
    remote_ask_by_local: Mutex<HashMap<String, (String, String)>>,
    /// remote agent address -> (the local session that last wrote to it, when); routes its plain sends back
    last_peer: Mutex<HashMap<String, (String, i64)>>,
    /// contact -> its last roster
    rosters: Mutex<HashMap<String, Vec<Agent>>>,
    /// intercom message id of a local ask pushed to a pi session -> our ask id
    local_ask_by_intercom: Mutex<HashMap<String, String>>,
    /// local agents seen on the socket (MCP, CLI): name -> (harness, last seen)
    socket_agents: Mutex<HashMap<String, (String, i64)>>,
    /// MCP servers that push messages into their session (Claude Code): name -> channel
    watchers: Mutex<HashMap<String, mpsc::UnboundedSender<Push>>>,
    /// open Codex threads: codex-<id> -> thread id
    codex_threads: Mutex<HashMap<String, String>>,
    /// contact proxies ("devbox") and remote-agent proxies ("devbox/planner")
    proxies: RwLock<HashMap<String, Arc<Proxy>>>,
    system: RwLock<Option<Arc<Proxy>>>,
    intercom_in: Mutex<Option<mpsc::UnboundedSender<Local>>>,
    arrived: Notify,
    /// contact -> what we know about talking with it (presence and roster requests)
    peers: Mutex<HashMap<String, PeerState>>,
    /// contact -> ts of the roster we keep (an older one that arrives late is ignored)
    roster_ts: Mutex<HashMap<String, i64>>,
    /// contact -> address, as this daemon last saw it (a new entry on reload is a new contact)
    known: Mutex<HashMap<String, String>>,
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
        s.asks.retain(|_, a| a.ts >= cut);
        s.msgs.push(m);
        self.save_spool(&s);
        drop(s);
        self.arrived.notify_waiters();
    }

    /// Hand a message to a local agent: push it into its session if its harness allows that, else spool
    /// it for its inbox. Asks stay answerable either way. Returns true when pushed.
    async fn hand_over(&self, m: Spooled, answerable_from: Option<&str>) -> bool {
        if let Some(contact) = answerable_from {
            let mut s = self.spool.lock().unwrap();
            s.answerable.insert(m.id.clone(), (contact.to_string(), m.ts));
            self.save_spool(&s);
        }
        if self.push(&m).await {
            return true;
        }
        self.spool(m, None);
        false
    }

    /// Push into the target's session: through its watching MCP server (Claude Code), or as a new turn on
    /// its open Codex thread. Never logs the text.
    async fn push(&self, m: &Spooled) -> bool {
        let text = render(m, &self.me.name, note_of(&m.from).as_deref());
        let tx = self.watchers.lock().unwrap().get(&m.target).cloned();
        if let Some(tx) = tx {
            let (done, rx) = oneshot::channel();
            let msg = json!({ "id": m.id, "from": m.from, "kind": m.kind, "text": text });
            if tx.send(Push { msg, done }).is_ok() && matches!(tokio::time::timeout(Duration::from_secs(5), rx).await, Ok(Ok(true))) {
                return true;
            }
        }
        if m.target.starts_with("codex-") {
            let mut thread = self.codex_threads.lock().unwrap().get(&m.target).cloned();
            if thread.is_none() {
                self.refresh_codex().await;
                thread = self.codex_threads.lock().unwrap().get(&m.target).cloned();
            }
            if let Some(t) = thread {
                match codex::start_turn(&t, &text).await {
                    Ok(()) => return true,
                    Err(e) => log(format!("codex push -> {} failed: {e}", m.target)),
                }
            }
        }
        // A pi session. An ask pushed this way is answered with `letagents reply`.
        if m.target != MACHINE_INBOX {
            let system = self.system.read().unwrap().clone();
            if let Some(sys) = system {
                let text = if m.kind == "ask" { format!("{text}\n\n(answer with: letagents reply {} \"<text>\")", m.id) } else { text };
                match sys.send(&m.target, &text, None, false).await {
                    Ok(s) if s.delivered => return true,
                    Ok(s) => log(format!("push -> {}: broker refused: {}", m.target, s.reason.unwrap_or_default())),
                    Err(e) => log(format!("push -> {}: {e}", m.target)),
                }
            }
        }
        false
    }

    /// Open Codex threads, also remembered as codex-<id> -> thread id.
    async fn refresh_codex(&self) -> Vec<codex::Thread> {
        let threads = codex::threads().await;
        let mut map = self.codex_threads.lock().unwrap();
        for t in &threads {
            map.insert(codex::name_for(&t.id), t.id.clone());
        }
        threads
    }

    /// Remember who sent ask `id`, so its reply finds them after a timeout or a restart.
    fn record_ask(&self, id: &str, agent: &str, proxy: Option<&str>, contact: Option<&str>) {
        let mut s = self.spool.lock().unwrap();
        let cut = now_ms() - crate::crypto::MAX_AGE_MS;
        s.asks.retain(|_, a| a.ts >= cut);
        s.asks.insert(id.to_string(), Asker { agent: agent.to_string(), proxy: proxy.map(str::to_string), ts: now_ms(), contact: contact.map(str::to_string) });
        self.save_spool(&s);
    }

    fn take_ask(&self, id: &str) -> Option<Asker> {
        let mut s = self.spool.lock().unwrap();
        let a = s.asks.remove(id);
        if a.is_some() {
            self.save_spool(&s);
        }
        a
    }

    fn set_last_peer(&self, remote: String, local: &str) {
        self.last_peer.lock().unwrap().insert(remote, (local.to_string(), now_ms()));
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
        for t in self.refresh_codex().await {
            let name = codex::name_for(&t.id);
            if !out.iter().any(|a| a.name == name) {
                out.push(Agent { name, harness: "codex".into(), status: t.status, r#where: t.cwd.rsplit('/').next().unwrap_or_default().into() });
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
        let id = compose_and_send(&self.me, contact, kind, body, reply_to, from, to).await?;
        let mut peers = self.peers.lock().unwrap();
        let p = peers.entry(contact.to_string()).or_default();
        p.sent = now_ms();
        if kind == "list" {
            p.listed = p.sent;
        }
        Ok(id)
    }

    /// A verified envelope from this contact, written at `ts` (its clock): it was online then, so presence
    /// pushes to it resume. An envelope that waited in our mailbox counts from when it was written.
    fn heard(&self, contact: &str, ts: i64) {
        let mut peers = self.peers.lock().unwrap();
        let p = peers.entry(contact.to_string()).or_default();
        p.got = now_ms();
        p.heard = p.heard.max(ts.min(p.got));
        p.held = p.held && p.silent(now_ms());
    }

    /// Send our roster (already filtered for this contact). It is marked as sent before it goes, so a
    /// presence check running at the same time does not send it again; a failure clears the mark.
    async fn send_roster(&self, contact: &str, roster: &[Agent], reply_to: Option<String>) -> Result<String> {
        self.mark_pushed(contact, Some(roster_key(roster)));
        let body = serde_json::to_string(roster).unwrap_or_default();
        let sent = self.seal_send(contact, "roster", &body, reply_to, None, None).await;
        if sent.is_err() {
            self.mark_pushed(contact, None);
        }
        sent
    }

    fn mark_pushed(&self, contact: &str, key: Option<Vec<(String, String)>>) {
        let mut peers = self.peers.lock().unwrap();
        let p = peers.entry(contact.to_string()).or_default();
        let changed = p.pushed != key;
        p.pushed = key;
        p.pushed_at = now_ms();
        if changed {
            save_pushed(&peers);
        }
    }

    /// Tell the sender what happened to its message. Never for receipts, rosters or hellos.
    async fn receipt(&self, contact: &str, msg_id: &str, status: &str, detail: &str) {
        let body = json!({ "status": status, "detail": detail }).to_string();
        if let Err(e) = self.seal_send(contact, "receipt", &body, Some(msg_id.into()), None, None).await {
            log(format!("receipt -> {contact} failed: {e}"));
        }
    }
}

pub async fn run(me: Identity, notify: Option<String>) -> Result<()> {
    let sock = socket_path();
    if UnixStream::connect(&sock).await.is_ok() {
        bail!("a letagents daemon is already running ({})", sock.display());
    }
    let _ = std::fs::remove_file(&sock);
    store::write("spool.json", &store::read::<Spool>("spool.json"))?; // ensures ~/.letagents exists (0700)
    let listener = UnixListener::bind(&sock)?;
    let my_uid = {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
        // The service writes its log here; it holds no message text, but keep it private too.
        let _ = std::fs::set_permissions(store::home().join("daemon.log"), std::fs::Permissions::from_mode(0o600));
        std::fs::metadata(&sock)?.uid()
    };
    let hub = Arc::new(Hub {
        me,
        notify,
        spool: Mutex::new(store::read("spool.json")),
        waiters: Mutex::default(),
        receipts: Mutex::default(),
        list_waiters: Mutex::default(),
        remote_ask_by_local: Mutex::default(),
        last_peer: Mutex::default(),
        rosters: Mutex::default(),
        local_ask_by_intercom: Mutex::default(),
        socket_agents: Mutex::default(),
        watchers: Mutex::default(),
        codex_threads: Mutex::default(),
        proxies: RwLock::default(),
        system: RwLock::default(),
        intercom_in: Mutex::default(),
        arrived: Notify::new(),
        peers: Mutex::new(load_pushed()),
        roster_ts: Mutex::default(),
        known: Mutex::new(store::contacts().into_iter().map(|(n, c)| (n, c.address)).collect()),
    });
    log(format!("socket {} ready; contact requests -> {}", sock.display(), hub.notify.as_deref().unwrap_or(MACHINE_INBOX)));

    tokio::spawn(intercom_manager(hub.clone()));
    tokio::spawn(relay_inbox(hub.clone()));
    tokio::spawn(roster_loop(hub.clone()));
    tokio::spawn(presence_loop(hub.clone()));
    tokio::spawn(redeliver_loop(hub.clone()));
    loop {
        let (conn, _) = listener.accept().await?;
        // Only our own user: the socket is 0600 in a 0700 directory, and the peer's uid is checked as well.
        match conn.peer_cred() {
            Ok(c) if c.uid() == my_uid => {
                tokio::spawn(serve_socket(hub.clone(), conn));
            }
            _ => log("refused a daemon.sock connection from another user"),
        }
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
    if own_session(hub, &m.from_id) {
        return log(format!("ignored a message from our own session to {}", m.proxy));
    }
    if m.proxy == SYSTEM_SESSION {
        // A pi agent answered a local ask that we pushed into its session.
        let ask = m.reply_to.as_ref().and_then(|r| hub.local_ask_by_intercom.lock().unwrap().remove(r));
        if let Some(ask_id) = ask {
            let from = if m.from_name.is_empty() { m.from_id.clone() } else { m.from_name.clone() };
            if let Err(e) = may_write(&store::contacts(), &from, None) {
                log(format!("REFUSED local reply from {from} (intercom): bound"));
                let system = hub.system.read().unwrap().clone();
                if let Some(sys) = system {
                    let _ = sys.send(&m.from_id, &format!("[letagents] refused: {e}"), None, false).await;
                }
                return;
            }
            let waiter = hub.waiters.lock().unwrap().remove(&ask_id);
            let reply = Spooled { id: uuid::Uuid::new_v4().to_string(), from: agent_addr(&from, &hub.me.name), kind: "reply".into(), target: MACHINE_INBOX.into(), text: m.text, ts: now_ms(), reply_to: Some(ask_id) };
            resolve_waiter(hub, waiter, reply).await;
        }
        return;
    }
    let from_agent = if m.from_name.is_empty() { m.from_id.clone() } else { m.from_name.clone() };
    // The binding check, before anything is sealed: the contact this goes to is the proxy's machine.
    let to_contact = m.proxy.split('/').next().unwrap_or_default().to_string();
    let link_contact = m.reply_to.as_ref().and_then(|r| hub.remote_ask_by_local.lock().unwrap().get(r).map(|x| x.0.clone()));
    if let Err(e) = may_write(&store::contacts(), &from_agent, Some(link_contact.as_deref().unwrap_or(&to_contact))) {
        log(format!("REFUSED out from {from_agent} via intercom -> {}: bound", m.proxy));
        if let Some(p) = hub.proxy(&m.proxy) {
            let _ = p.send(&m.from_id, &format!("[letagents] refused: {e}"), None, false).await;
        }
        return;
    }
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
        hub.set_last_peer(agent_addr(a, &contact), &m.from_id);
    }
    let dest = to_agent.clone().map(|a| agent_addr(&a, &contact)).unwrap_or_else(|| contact.clone());
    match hub.seal_send(&contact, kind, &body, None, Some(from_agent), to_agent).await {
        Ok(id) => {
            if kind == "ask" {
                hub.record_ask(&id, &m.from_id, Some(&m.proxy), Some(&contact));
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

/// Mail spooled for a session that is live now (a pi session on the broker, a watching MCP server, an open
/// Codex thread) is pushed again until it lands: a push that failed once is not lost in the inbox.
async fn redeliver_loop(hub: Arc<Hub>) {
    loop {
        tokio::time::sleep(REDELIVER_EVERY).await;
        let cut = now_ms() - crate::crypto::MAX_AGE_MS;
        let waiting: Vec<Spooled> = hub.spool.lock().unwrap().msgs.iter().filter(|m| m.target != MACHINE_INBOX && m.ts >= cut).cloned().collect();
        if waiting.is_empty() {
            continue;
        }
        let mut live: HashSet<String> = hub.watchers.lock().unwrap().keys().cloned().collect();
        live.extend(hub.codex_threads.lock().unwrap().keys().cloned());
        let system = hub.system.read().unwrap().clone();
        if let Some(sys) = system {
            for x in sys.list().await.unwrap_or_default() {
                for k in ["id", "name"] {
                    if let Some(v) = x[k].as_str().filter(|v| !v.is_empty()) {
                        live.insert(v.to_string());
                    }
                }
            }
        }
        for m in waiting.into_iter().filter(|m| live.contains(&m.target)) {
            // Take it out of the spool first: an inbox read or a new watcher must not get it a second time.
            let at = {
                let mut s = hub.spool.lock().unwrap();
                let Some(i) = s.msgs.iter().position(|x| x.id == m.id && x.target == m.target) else { continue };
                s.msgs.remove(i);
                hub.save_spool(&s);
                i
            };
            if hub.push(&m).await {
                log(format!("redelivered {} {} -> {}", m.kind, m.id, m.target));
            } else {
                let mut s = hub.spool.lock().unwrap();
                let i = at.min(s.msgs.len());
                s.msgs.insert(i, m);
                hub.save_spool(&s);
                drop(s);
                hub.arrived.notify_waiters();
            }
        }
    }
}

// ---------- rosters and presence ----------

/// Give contacts without a drop code their own and update the relay's table (drops.rs). `force`: send the
/// table even if our last update matches (at start: the relay may have erased an idle mailbox). Returns the
/// contacts that got a new code the relay now holds: each needs a message to carry it (a `list`).
async fn refresh_drops(force: bool) -> Vec<String> {
    let before: HashSet<String> = store::contacts().into_iter().filter(|(_, c)| c.our_drop.is_none()).map(|(n, _)| n).collect();
    drops::issue_missing();
    match drops::sync(force).await {
        Ok(changed) => {
            if changed {
                log("drop codes: relay table updated");
            }
            store::contacts().into_iter().filter(|(n, c)| before.contains(n) && c.our_drop.is_some()).map(|(n, _)| n).collect()
        }
        Err(e) => {
            log(format!("drop codes: relay update failed: {e}"));
            vec![]
        }
    }
}

/// Update drop codes and ask every contact who is online: at start, then every ROSTER_REFRESH (with jitter)
/// as a safety net. Rosters normally move by events: a `list` when the relay is reached again or a contact or
/// code is new, and presence pushes on change.
async fn roster_loop(hub: Arc<Hub>) {
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut start = true;
    loop {
        // At start: send the table even if it looks current (the relay may have erased an idle mailbox).
        for c in refresh_drops(start).await {
            let _ = request_roster(&hub, &c, Why::Now).await;
        }
        // At start the relay login may have asked already (Reach skips a contact asked a moment ago).
        list_all(&hub, if start { Why::Reach } else { Why::Refresh }, if start { "start" } else { "safety refresh" }).await;
        start = false;
        let jitter = 0.9 + 0.2 * rand::random::<f64>();
        tokio::time::sleep(timing(ROSTER_REFRESH).mul_f64(jitter)).await;
    }
}

/// Ask every contact for its roster, within the limits of `why`.
async fn list_all(hub: &Hub, why: Why, what: &str) {
    let mut n = 0;
    for contact in store::contacts().keys() {
        if matches!(request_roster(hub, contact, why).await, Ok(Some(_))) {
            n += 1;
        }
    }
    if n > 0 {
        log(format!("roster: asked {n} contact(s) ({what})"));
    }
}

/// Send `contact` a `list` unless `why` says it is not needed now. Ok(None): skipped.
async fn request_roster(hub: &Hub, contact: &str, why: Why) -> Result<Option<String>> {
    if hub.peers.lock().unwrap().get(contact).is_some_and(|p| !p.list_due(why, now_ms())) {
        return Ok(None);
    }
    hub.seal_send(contact, "list", "", None, None, None).await.map(Some)
}

/// Push our roster to contacts allowed to list us when what they last got from us differs. Per contact: at
/// most one push per PRESENCE_GAP (the last state wins), and a contact silent for SILENT gets one push, then
/// nothing until it writes again.
async fn presence_loop(hub: Arc<Hub>) {
    loop {
        tokio::time::sleep(timing(PRESENCE_CHECK)).await;
        let roster = hub.local_roster().await;
        let all = store::contacts();
        let mut pushed = vec![];
        for (name, c) in &all {
            if !store::may_see_roster(c) {
                continue;
            }
            let theirs = roster_for(&roster, name, &all);
            let due = {
                let mut peers = hub.peers.lock().unwrap();
                let p = peers.entry(name.clone()).or_default();
                let due = p.push_due(&roster_key(&theirs), now_ms());
                // A silent contact gets this one push; the next ones wait until it writes.
                p.held |= due == Some(true);
                due
            };
            let Some(silent) = due else { continue };
            match hub.send_roster(name, &theirs, None).await {
                Ok(_) => {
                    pushed.push(name.clone());
                    if silent {
                        log(format!("presence: {name} is silent; it got this roster, the next ones wait until it writes"));
                    }
                }
                Err(e) => {
                    hub.peers.lock().unwrap().entry(name.clone()).or_default().held = false;
                    log(format!("presence -> {name} failed: {e}"));
                }
            }
        }
        if !pushed.is_empty() {
            log(format!("presence: pushed {} agents to {}", roster.len(), pushed.join(", ")));
        }
    }
}

/// An unsolicited roster from `contact`: if we have sent it nothing for a while, it may count us as silent
/// and hold its next pushes, so answer with our roster (or a `list` if it may not see ours).
async fn echo_presence(hub: &Hub, contact: &str) {
    // Decide and mark under one lock: rosters that came together (from the mailbox) get one answer.
    {
        let mut peers = hub.peers.lock().unwrap();
        let p = peers.entry(contact.to_string()).or_default();
        if now_ms() - p.sent <= ms(SILENT) / 2 {
            return;
        }
        p.sent = now_ms();
    }
    let all = store::contacts();
    let Some(c) = all.get(contact) else { return };
    if store::may_see_roster(c) {
        let theirs = roster_for(&hub.local_roster().await, contact, &all);
        match hub.send_roster(contact, &theirs, None).await {
            Ok(_) => log(format!("presence: {contact} wrote after a quiet spell; sent our roster back")),
            Err(e) => log(format!("presence -> {contact} failed: {e}")),
        }
    } else {
        let _ = request_roster(hub, contact, Why::Reach).await;
    }
}

// ---------- relay -> local ----------

async fn relay_inbox(hub: Arc<Hub>) {
    let mut seen: Seen = store::read("seen.json");
    let me = hub.me.clone();
    let reached = hub.clone();
    let on_reach = move || {
        let hub = reached.clone();
        tokio::spawn(async move { list_all(&hub, Why::Reach, "relay reached").await });
    };
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
    }, on_reach)
    .await
}

async fn inbound(hub: Arc<Hub>, inner: Inner, sender: Peer) {
    let from_agent = inner.from_agent.clone().unwrap_or_else(|| "agent".into());
    // Drop codes first, for any kind from a known contact: its answer below already carries our confirmation.
    if let Ok((contact, _)) = store::contact_of(&sender) {
        hub.heard(&contact, inner.ts);
        let learned = drops::learn(&sender, &inner);
        if learned.rotated {
            log("drop codes: every contact has its own code; replaced the shared code");
            tokio::spawn(refresh_drops(false));
        }
        // A new code from a message we do not answer: confirm it with a list (answered with a roster).
        if learned.new_code && !matches!(inner.kind.as_str(), "list" | "send" | "ask") {
            let _ = request_roster(&hub, &contact, Why::Now).await;
        }
    }
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
        _ => match target_for(&hub, &inner, &sender) {
            Some(t) if reserved_target(&hub, &t) => {
                if let Ok((contact, _)) = store::contact_of(&sender) {
                    log(format!("REFUSED {} {} from {contact}/{from_agent}: {t:?} is not an agent here", inner.kind, inner.id));
                    hub.receipt(&contact, &inner.id, "refused", "not an agent on this machine (see letagents list)").await;
                }
                return;
            }
            // An agent bound to one contact takes messages from that contact only.
            Some(t) if store::contact_of(&sender).is_ok_and(|(c, _)| store::bound_to(&t, &store::contacts()).is_some_and(|(b, _)| b != c)) => {
                if let Ok((contact, _)) = store::contact_of(&sender) {
                    log(format!("REFUSED {} {} from {contact}/{from_agent}: {t} is bound to another contact", inner.kind, inner.id));
                    hub.receipt(&contact, &inner.id, "refused", &format!("{t} takes messages from one contact only")).await;
                }
                return;
            }
            Some(t) => t,
            None => {
                // No agent named and no conversation to continue: never guess a session.
                if let Ok((contact, _)) = store::contact_of(&sender) {
                    let detail = format!("name an agent: send to {}/<agent> (see letagents list)", hub.me.name);
                    log(format!("REFUSED {} {} from {contact}/{from_agent}: no agent named", inner.kind, inner.id));
                    hub.receipt(&contact, &inner.id, "refused", &detail).await;
                }
                return;
            }
        },
    };
    // A reply to an ask we sent: only from the contact we asked. Then the ask invited it, and per-agent rules
    // do not apply (it goes only to the agent that asked).
    let mut answer = false;
    if inner.kind == "reply" {
        let asked = inner.reply_to.as_ref().and_then(|r| hub.spool.lock().unwrap().asks.get(r).and_then(|a| a.contact.clone()));
        if let (Some(asked), Ok((contact, _))) = (asked, store::contact_of(&sender)) {
            if asked != contact {
                log(format!("REFUSED reply {} from {contact}/{from_agent}: that ask went to another contact", inner.id));
                return hub.receipt(&contact, &inner.id, "refused", "no open ask with that id").await;
            }
            answer = true;
        }
    }
    let authorized = if answer { store::authorize_answer(&sender) } else { store::authorize(&sender, &inner.kind, &from_agent, &to_local) };
    let (contact, _) = match authorized {
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
            let roster = roster_for(&hub.local_roster().await, &contact, &store::contacts());
            match hub.send_roster(&contact, &roster, Some(inner.id.clone())).await {
                Ok(_) => log(format!("roster -> {contact} ({} agents)", roster.len())),
                Err(e) => log(format!("roster -> {contact} failed: {e}")),
            }
        }
        "reply" => on_reply(&hub, &from_addr, inner).await,
        _ => deliver(&hub, &contact, &from_agent, &from_addr, to_local, inner).await,
    }
}

/// The local agent a send/ask is for: the named agent, else the local agent that wrote to that remote
/// agent in the last hour, else the contact's configured session. None: the sender must name an agent.
fn target_for(hub: &Hub, inner: &Inner, sender: &Peer) -> Option<String> {
    if let Some(t) = inner.to_agent.as_ref().filter(|t| !t.is_empty()) {
        return Some(t.clone());
    }
    let contact = store::contact_of(sender).ok();
    if let (Some((name, _)), Some(fa)) = (&contact, &inner.from_agent) {
        let cut = now_ms() - LAST_PEER_TTL_MS;
        if let Some((local, _)) = hub.last_peer.lock().unwrap().get(&agent_addr(fa, name)).filter(|(_, t)| *t >= cut).cloned() {
            return Some(local);
        }
    }
    contact.and_then(|(_, c)| c.session)
}

async fn deliver(hub: &Arc<Hub>, contact: &str, from_agent: &str, from_addr: &str, target: String, inner: Inner) {
    let ask = inner.kind == "ask";
    // Prefer the remote agent's own session, so a plain answer goes back to that agent.
    let proxy = hub.proxy(from_addr).or_else(|| hub.proxy(contact));
    if let Some(proxy) = proxy {
        let who = label(from_addr, true, note_of(from_addr).as_deref());
        let header = if ask { format!("{who} ASK") } else { who };
        let sent = proxy.send(&target, &format!("{header}\n\n{}", inner.body), None, ask).await;
        match &sent {
            Ok(s) if !s.delivered => log(format!("intercom -> {target}: broker refused: {}", s.reason.clone().unwrap_or_default())),
            Err(e) => log(format!("intercom -> {target}: {e}")),
            _ => {}
        }
        if let Ok(s) = sent {
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
    let pushed = hub
        .hand_over(
            Spooled { id: inner.id.clone(), from: from_addr.to_string(), kind: inner.kind.clone(), target: target.clone(), text: inner.body.clone(), ts: inner.ts, reply_to: None },
            if ask { Some(contact) } else { None },
        )
        .await;
    let status = if pushed { "pushed" } else { "queued" };
    log(format!("in {} from {from_addr} -> {target}: {status}", inner.kind));
    hub.receipt(contact, &inner.id, status, &target).await;
}

async fn on_reply(hub: &Hub, from_addr: &str, inner: Inner) {
    let waiter = inner.reply_to.as_ref().and_then(|r| hub.waiters.lock().unwrap().remove(r));
    let reply = Spooled { id: inner.id.clone(), from: from_addr.into(), kind: "reply".into(), target: MACHINE_INBOX.into(), text: inner.body.clone(), ts: inner.ts, reply_to: inner.reply_to.clone() };
    resolve_waiter(hub, waiter, reply).await;
}

/// Hand a reply to whoever asked: the live waiter, else the asker recorded by ask id (after a timeout or
/// a restart), else the machine inbox. Never a session that did not ask.
async fn resolve_waiter(hub: &Hub, waiter: Option<Waiter>, reply: Spooled) {
    let from = reply.from.clone();
    let asker = reply.reply_to.as_deref().and_then(|r| hub.take_ask(r));
    let waiter = waiter.or_else(|| {
        asker.map(|a| match a.proxy {
            Some(proxy) => Waiter::Intercom { proxy, local_from: a.agent, local_msg: String::new() },
            None => Waiter::Spool { asker: a.agent },
        })
    });
    match waiter {
        Some(Waiter::Intercom { proxy, local_from, local_msg }) => {
            // Without the original intercom message (after a restart), say which ask this answers.
            let text = if local_msg.is_empty() { format!("{} REPLY {}\n\n{}", label(&from, true, note_of(&from).as_deref()), reply.reply_to.as_deref().unwrap_or("?"), reply.text) } else { reply.text.clone() };
            let link = Some(local_msg.as_str()).filter(|s| !s.is_empty());
            let ok = match hub.proxy(&proxy) {
                Some(p) => p.send(&local_from, &text, link, false).await.map(|s| s.delivered).unwrap_or(false),
                None => false,
            };
            if !ok {
                hub.hand_over(Spooled { target: local_from, ..reply }, None).await;
            }
            log(format!("in reply from {from} -> intercom asker: {}", if ok { "delivered" } else { "queued" }));
        }
        Some(Waiter::Socket { asker, tx }) => {
            if let Err(reply) = tx.send(reply) {
                hub.hand_over(Spooled { target: asker, ..reply }, None).await;
            }
            log(format!("in reply from {from} -> socket asker"));
        }
        Some(Waiter::Spool { asker }) => {
            let pushed = hub.hand_over(Spooled { target: asker.clone(), ..reply }, None).await;
            log(format!("in reply from {from} -> {} for {asker}", if pushed { "pushed" } else { "queued" }));
        }
        None => {
            hub.spool(Spooled { target: MACHINE_INBOX.into(), ..reply }, None);
            log(format!("in reply from {from}: no open ask; queued in the machine inbox"));
        }
    }
}

async fn on_receipt(hub: &Hub, contact: &str, inner: Inner) {
    let Some(of) = inner.reply_to.clone() else { return };
    let v: Value = serde_json::from_str(&inner.body).unwrap_or(json!({}));
    let status = v["status"].as_str().unwrap_or("?").chars().filter(char::is_ascii_alphabetic).take(16).collect::<String>();
    // The detail is the contact's text: one short line, so it cannot pose as a message header in a session.
    let detail: String = v["detail"].as_str().unwrap_or_default().chars().map(|c| if c.is_control() { ' ' } else { c }).take(200).collect();
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
        if waiter.is_some() || hub.spool.lock().unwrap().asks.contains_key(&of) {
            let reply = Spooled { id: inner.id, from: contact.into(), kind: "reply".into(), target: MACHINE_INBOX.into(), text: format!("[letagents] refused: {detail}"), ts: now_ms(), reply_to: Some(of) };
            resolve_waiter(hub, waiter, reply).await;
        }
    }
}

async fn on_roster(hub: &Arc<Hub>, contact: &str, inner: Inner) {
    if inner.reply_to.is_none() {
        // A little later: rosters that waited in the mailbox come just before the relay login is ready, and
        // the `list` that login sends already tells the contact we are here.
        let (hub, contact) = (hub.clone(), contact.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(ECHO_DELAY).await;
            echo_presence(&hub, &contact).await;
        });
    }
    // Rosters can arrive out of order (one waited in the mailbox): keep the newest.
    {
        let mut ts = hub.roster_ts.lock().unwrap();
        if ts.get(contact).is_some_and(|t| *t > inner.ts) {
            return log(format!("roster from {contact}: older than the one we have; ignored"));
        }
        ts.insert(contact.to_string(), inner.ts);
    }
    let agents: Vec<Agent> = serde_json::from_str(&inner.body).unwrap_or_default();
    let clean = |s: &str| s.chars().filter(|c| !c.is_control()).take(80).collect::<String>();
    let agents: Vec<Agent> = agents
        .into_iter()
        .filter(|a| valid_agent_name(&a.name))
        .take(MAX_ROSTER)
        .map(|a| Agent { harness: clean(&a.harness), status: clean(&a.status), r#where: clean(&a.r#where), name: a.name })
        .collect();
    hub.rosters.lock().unwrap().insert(contact.to_string(), agents.clone());
    if let Some(tx) = inner.reply_to.as_ref().and_then(|r| hub.list_waiters.lock().unwrap().remove(r)) {
        let _ = tx.send(agents.clone());
    }
    log(format!("roster from {contact}: {}", agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")));
    sync_agent_proxies(hub, contact, &agents).await;
}

async fn hello(hub: &Arc<Hub>, inner: Inner, sender: Peer) {
    match store::contact_of(&sender) {
        Ok(_) => return log("hello from a known contact"),
        // A known id with a different key: never re-add it through an invite.
        Err(e) if !e.starts_with("unknown sender") => return log(format!("hello refused: {e}")),
        Err(_) => {}
    }
    // A valid one-time invite code from our `letagents invite`: the human who shared it approved this contact.
    if inner.invite.as_deref().is_some_and(store::redeem_invite) {
        let code = inner.invite.clone().unwrap_or_default();
        let note = match store::accept_invited(&sender, &inner.body, drops::take_invite(&code), &code) {
            Ok((name, bound)) => {
                log(format!("added contact {name} ({}) by invite code", sender.safety_number()));
                if let Some(agent) = &bound {
                    log(format!("bound {name} to {agent} (from the invite)"));
                }
                // Its invite's drop code is now its own: no expiry at the relay any more.
                refresh_drops(false).await;
                hub.known.lock().unwrap().insert(name.clone(), store::contacts().get(&name).map(|c| c.address.clone()).unwrap_or_default());
                connect_contact(hub, &name).await;
                let _ = request_roster(hub, &name, Why::Now).await;
                let bound = bound.map(|a| format!(" It reaches only {a}, and {a} writes only to it.")).unwrap_or_default();
                format!("[letagents] added contact {name} (safety number {}): it used your invite code.{bound} Remove it with: letagents contact rm {name}", sender.safety_number())
            }
            Err(e) => return log(format!("invite: could not add contact: {e}")),
        };
        return notify(hub, &inner.id, note).await;
    }
    let (rid, req) = store::record_request(&sender, &inner.body, inner.ts);
    log(format!("contact request {rid} from \"{}\" ({})", req.name, req.safety));
    let note = format!(
        "[letagents] contact request {rid} from an agent calling itself \"{}\", safety number {}.\nAsk your human to confirm this safety number with them, then run: letagents contact accept {rid} <name>",
        req.name, req.safety
    );
    notify(hub, &inner.id, note).await;
}

/// Register a newly added contact as a pi-intercom session (normally done when the daemon starts).
async fn connect_contact(hub: &Hub, name: &str) {
    let Some(in_tx) = hub.intercom_in.lock().unwrap().clone() else { return };
    if hub.system.read().unwrap().is_none() || hub.proxy(name).is_some() {
        return;
    }
    let Some(address) = store::contacts().get(name).map(|c| c.address.clone()) else { return };
    let (ignore_tx, _ignore_rx) = mpsc::unbounded_channel::<String>();
    match Proxy::connect(name, &stable_session_id(&address), in_tx, ignore_tx).await {
        Ok(p) => {
            hub.proxies.write().unwrap().insert(name.to_string(), p);
        }
        Err(e) => log(format!("intercom session \"{name}\" failed: {e}")),
    }
}

/// A note for the human: kept in the machine inbox, and also told to the --notify agent if there is one.
async fn notify(hub: &Hub, id: &str, note: String) {
    let inner_id = id.to_string();
    let inner_ts = now_ms();
    // The machine inbox always keeps it; the --notify agent (if any) is also told.
    hub.spool(Spooled { id: inner_id.clone(), from: SYSTEM_SESSION.into(), kind: "notice".into(), target: MACHINE_INBOX.into(), text: note.clone(), ts: inner_ts, reply_to: None }, None);
    let Some(notify) = hub.notify.clone() else { return };
    let system = hub.system.read().unwrap().clone();
    let delivered = match system {
        Some(p) => p.send(&notify, &note, None, false).await.map(|s| s.delivered).unwrap_or(false),
        None => false,
    };
    if !delivered {
        hub.hand_over(Spooled { id: format!("{inner_id}:{notify}"), from: SYSTEM_SESSION.into(), kind: "notice".into(), target: notify, text: note, ts: inner_ts, reply_to: None }, None).await;
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
    /// Codex thread id of the MCP server's session
    #[serde(default)]
    thread: Option<String>,
    /// peek: message ids the caller already knows
    #[serde(default)]
    seen: Vec<String>,
}

async fn serve_socket(hub: Arc<Hub>, conn: UnixStream) {
    let (mut rd, mut wr) = conn.into_split();
    while let Ok(v) = read_frame(&mut rd).await {
        let resp = match serde_json::from_value::<Req>(v) {
            Ok(req) if req.op == "watch" => {
                let Some(me) = req.as_.clone() else { break };
                return watch(hub, me, req.harness, rd, wr).await;
            }
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

/// A `watch` connection: the MCP server of a session that can take pushed messages (Claude Code). We send
/// `{"push": {id, from, kind, text}}`; it answers `{"ok": bool}` once it has posted the text into its session.
async fn watch(hub: Arc<Hub>, me: String, harness: Option<String>, mut rd: tokio::net::unix::OwnedReadHalf, mut wr: tokio::net::unix::OwnedWriteHalf) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Push>();
    hub.watchers.lock().unwrap().insert(me.clone(), tx.clone());
    hub.touch_socket_agent(&me, harness.as_deref());
    if wr.write_all(&frame(&json!({ "ok": true }))).await.is_ok() {
        log(format!("{me} watching: messages are pushed into its session"));
        // Mail that waited for this agent: push it now (in the background; the acks come through this loop).
        let waiting: Vec<Spooled> = {
            let mut s = hub.spool.lock().unwrap();
            let (mine, rest): (Vec<_>, Vec<_>) = s.msgs.drain(..).partition(|m| m.target == me);
            s.msgs = rest;
            if !mine.is_empty() {
                hub.save_spool(&s);
            }
            mine
        };
        if !waiting.is_empty() {
            let hub = hub.clone();
            tokio::spawn(async move {
                for m in waiting {
                    if !hub.push(&m).await {
                        hub.spool(m, None);
                    }
                }
            });
        }
        while let Some(p) = rx.recv().await {
            let posted = async {
                wr.write_all(&frame(&json!({ "push": p.msg }))).await.ok()?;
                let v = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut rd)).await.ok()?.ok()?;
                Some(v["ok"] == json!(true))
            }
            .await;
            let _ = p.done.send(posted == Some(true));
            if posted.is_none() {
                break;
            }
        }
    }
    let mut w = hub.watchers.lock().unwrap();
    if w.get(&me).is_some_and(|t| t.same_channel(&tx)) {
        w.remove(&me);
        log(format!("{me} stopped watching"));
    }
}

/// Every agent this machine can reach, as `contact/agent`, refreshed from contacts within `wait`.
async fn directory(hub: &Hub, only: Option<&str>, wait: Duration) -> Value {
    let contacts: Vec<String> = store::contacts().keys().filter(|c| only.is_none_or(|o| o == c.as_str())).cloned().collect();
    let mut pending = vec![];
    for c in &contacts {
        let (tx, rx) = oneshot::channel();
        if let Ok(Some(id)) = request_roster(hub, c, Why::Now).await {
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
            format!("[letagents · {from_addr}] ASK")
        } else {
            format!("[letagents · {from_addr}]")
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
    let pushed = hub
        .hand_over(
            Spooled { id: id.to_string(), from: from_addr, kind: kind.into(), target: agent.into(), text: text.into(), ts: now_ms(), reply_to: None },
            if kind == "ask" { Some("") } else { None },
        )
        .await;
    let status = if pushed { "pushed" } else { "queued" };
    log(format!("local {kind} {from} -> {agent}: {status}"));
    json!({ "status": status, "detail": agent })
}

async fn handle(hub: &Hub, r: Req) -> Result<Value> {
    let me_as = r.as_.clone().filter(|a| !a.is_empty()).unwrap_or_else(|| MACHINE_INBOX.into());
    if r.as_.is_some() && r.op != "bye" {
        hub.touch_socket_agent(&me_as, r.harness.as_deref());
    }
    match r.op.as_str() {
        "hello" => {
            if let Some(t) = &r.thread {
                hub.codex_threads.lock().unwrap().insert(me_as.clone(), t.clone());
            }
            Ok(json!({ "ok": true, "as": me_as, "machine": hub.me.name }))
        }
        "bye" => {
            hub.socket_agents.lock().unwrap().remove(&me_as);
            hub.watchers.lock().unwrap().remove(&me_as);
            log(format!("{me_as} left"));
            Ok(json!({ "ok": true }))
        }
        "status" => {
            let s = hub.spool.lock().unwrap();
            Ok(json!({
                "ok": true, "name": hub.me.name, "address": store::identity().map(|i| i.peer().address(true)).unwrap_or_default(),
                "safety": hub.me.peer().safety_number(), "notify": hub.notify,
                "intercom": hub.system.read().unwrap().is_some(), "waiting": s.msgs.len(),
            }))
        }
        "reload" => {
            // Contacts added by the CLI (join, contact add/accept): give them drop codes, make them intercom
            // sessions, ask who is online. A removal: the relay drops its code.
            let mut ask: HashSet<String> = refresh_drops(false).await.into_iter().collect();
            let all = store::contacts();
            {
                let mut known = hub.known.lock().unwrap();
                known.retain(|n, _| all.contains_key(n));
                for (name, c) in &all {
                    if known.insert(name.clone(), c.address.clone()).as_ref() != Some(&c.address) {
                        ask.insert(name.clone());
                    }
                }
            }
            for name in all.keys() {
                if hub.proxy(name).is_none() {
                    connect_contact(hub, name).await;
                }
            }
            for name in ask {
                let _ = request_roster(hub, &name, Why::Now).await;
            }
            Ok(json!({ "ok": true }))
        }
        "list" => {
            let wait = Duration::from_millis(if r.wait_ms == 0 { 6000 } else { r.wait_ms.min(20_000) });
            // A bound agent sees only its contact's agents: not the agents on this machine, not other contacts.
            match store::bound_to(&me_as, &store::contacts()) {
                Some((c, _)) => {
                    let mut v = directory(hub, Some(&c), wait).await;
                    v["local"] = json!([]);
                    v["groups"] = json!([]);
                    Ok(v)
                }
                None => Ok(directory(hub, r.contact.as_deref(), wait).await),
            }
        }
        "send" | "ask" => {
            let local: Vec<String> = hub.local_roster().await.into_iter().map(|a| a.name).collect();
            let targets: Vec<String> = match parse_to(&r.to, &hub.me.name, &local)? {
                To::Group(g) => store::groups().get(&g).cloned().ok_or_else(|| anyhow!("no group @{g}"))?,
                _ => vec![r.to.clone()],
            };
            // The binding check for every target before anything is sent.
            let all = store::contacts();
            for t in &targets {
                let dest = match parse_to(t, &hub.me.name, &local) {
                    Ok(To::Contact { contact, .. }) => Some(contact),
                    Ok(_) => None,
                    Err(e) if store::bound_to(&me_as, &all).is_none() => return Err(e),
                    Err(_) => None,
                };
                if let Err(e) = may_write(&all, &me_as, dest.as_deref()) {
                    log(format!("REFUSED out {} from {me_as} -> {t}: bound", r.op));
                    bail!("refused: {e}");
                }
            }
            let mut results = vec![];
            let mut replies = vec![];
            for t in targets {
                let id = match parse_to(&t, &hub.me.name, &local)? {
                    To::Group(_) => bail!("groups cannot contain groups"),
                    To::Local(agent) => {
                        let id = uuid::Uuid::new_v4().to_string();
                        if r.op == "ask" {
                            let (tx, rx) = oneshot::channel();
                            hub.record_ask(&id, &me_as, None, None);
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
                            hub.set_last_peer(agent_addr(a, &contact), &me_as);
                        }
                        let id = hub.seal_send(&contact, &r.op, &r.text, None, Some(me_as.clone()), agent).await?;
                        if r.op == "ask" {
                            hub.record_ask(&id, &me_as, None, Some(&contact));
                        }
                        id
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
            let (contact, _) = hub.spool.lock().unwrap().answerable.get(&r.msg).cloned().ok_or_else(|| anyhow!("no open ask with id {}", r.msg))?;
            if let Err(e) = may_write(&store::contacts(), &me_as, Some(contact.as_str()).filter(|c| !c.is_empty())) {
                log(format!("REFUSED reply from {me_as}: bound"));
                bail!("refused: {e}");
            }
            hub.spool.lock().unwrap().answerable.remove(&r.msg);
            hub.save_spool(&hub.spool.lock().unwrap());
            if contact.is_empty() {
                // A local ask: hand the reply straight to the asker.
                let waiter = hub.waiters.lock().unwrap().remove(&r.msg);
                let reply = Spooled { id: uuid::Uuid::new_v4().to_string(), from: agent_addr(&me_as, &hub.me.name), kind: "reply".into(), target: MACHINE_INBOX.into(), text: r.text.clone(), ts: now_ms(), reply_to: Some(r.msg.clone()) };
                resolve_waiter(hub, waiter, reply).await;
                return Ok(json!({ "ok": true, "msg_id": r.msg }));
            }
            let id = hub.seal_send(&contact, "reply", &r.text, Some(r.msg.clone()), Some(me_as), None).await?;
            log(format!("out reply -> {contact} (from socket)"));
            Ok(json!({ "ok": true, "msg_id": id }))
        }
        "peek" => {
            // MCP Events source: the messages waiting for this agent, without removing them. Waits up to
            // wait_ms until one arrives that is not in `seen`. `waiting` lists every waiting id.
            let seen: HashSet<&str> = r.seen.iter().map(String::as_str).collect();
            let deadline = tokio::time::Instant::now() + Duration::from_millis(r.wait_ms.min(60_000));
            loop {
                let notified = hub.arrived.notified();
                let (new, waiting): (Vec<Spooled>, Vec<String>) = {
                    let s = hub.spool.lock().unwrap();
                    let mine: Vec<&Spooled> = s.msgs.iter().filter(|m| m.target == me_as).collect();
                    (mine.iter().filter(|m| !seen.contains(m.id.as_str())).map(|m| (*m).clone()).collect(), mine.iter().map(|m| m.id.clone()).collect())
                };
                if !new.is_empty() || tokio::time::Instant::now() >= deadline {
                    return Ok(json!({ "ok": true, "messages": new, "waiting": waiting }));
                }
                let _ = tokio::time::timeout_at(deadline, notified).await;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(from: &str, kind: &str) -> Spooled {
        Spooled { id: "id1".into(), from: from.into(), kind: kind.into(), target: "t".into(), text: "body".into(), ts: 0, reply_to: Some("q1".into()) }
    }

    #[test]
    fn headers_name_the_sender_once() {
        assert_eq!(render(&msg("laptop/tester", "send"), "laptop", None), "[letagents · laptop/tester]\n\nbody");
        assert_eq!(render(&msg("devbox/planner", "send"), "laptop", None), "[letagents · verified devbox/planner]\n\nbody");
        assert_eq!(render(&msg("devbox/planner", "ask"), "laptop", None), "[letagents · verified devbox/planner] ASK id1\n\nbody");
        assert_eq!(render(&msg("laptop/tester", "reply"), "laptop", None), "[letagents · laptop/tester] REPLY q1\n\nbody");
        for kind in ["send", "ask", "reply"] {
            assert_eq!(render(&msg("devbox/planner", kind), "laptop", None).matches("devbox/planner").count(), 1);
        }
    }

    #[test]
    fn the_note_is_in_the_label_of_remote_senders_only() {
        assert_eq!(render(&msg("x/alice", "send"), "laptop", Some("external engineer")), "[letagents · verified x/alice · note: external engineer]\n\nbody");
        assert_eq!(render(&msg("x/alice", "ask"), "laptop", Some("n")), "[letagents · verified x/alice · note: n] ASK id1\n\nbody");
        assert_eq!(render(&msg("laptop/tester", "send"), "laptop", Some("n")), "[letagents · laptop/tester]\n\nbody");
    }

    #[test]
    fn presence_skips_silent_contacts_and_coalesces() {
        let key = |n: &str| vec![(n.to_string(), "pi".to_string())];
        let now = 10 * ms(SILENT);
        // Never heard: one push, then hold until it writes.
        let mut p = PeerState::default();
        assert_eq!(p.push_due(&key("a"), now), Some(true));
        p.pushed = Some(key("a"));
        p.pushed_at = now;
        p.held = true;
        assert_eq!(p.push_due(&key("b"), now + ms(PRESENCE_GAP) * 5), None, "silent and held");
        // It wrote: pushes resume, at most one per PRESENCE_GAP, last state wins.
        p.heard = now + 1000;
        p.held = false;
        assert_eq!(p.push_due(&key("b"), now + 2000), None, "within the gap");
        assert_eq!(p.push_due(&key("b"), now + ms(PRESENCE_GAP)), Some(false));
        assert_eq!(p.push_due(&key("a"), now + ms(PRESENCE_GAP)), None, "unchanged");
    }

    #[test]
    fn lists_are_bounded_unless_needed() {
        let now = crate::crypto::MAX_AGE_MS;
        let mut p = PeerState::default();
        assert!(p.list_due(Why::Reach, now) && p.list_due(Why::Refresh, now));
        p.listed = now; // sent, not answered
        assert!(!p.list_due(Why::Reach, now + ms(REACH_LIST_GAP)) && !p.list_due(Why::Refresh, now + 3_600_000));
        assert!(p.list_due(Why::Now, now), "a new contact or code always gets one");
        p.got = now + 10; // answered
        assert!(!p.list_due(Why::Reach, now + 20), "asked a moment ago");
        assert!(p.list_due(Why::Reach, now + ms(REACH_LIST_GAP)) && p.list_due(Why::Refresh, now + 20));
        assert!(p.list_due(Why::Refresh, now + crate::crypto::MAX_AGE_MS + 1), "the old list left the mailbox");
    }

    fn bound(all: &mut store::Contacts, name: &str, agent: &str, exclusive: bool) {
        all.insert(name.into(), store::Contact { bind: Some(store::Bind { agent: agent.into(), exclusive }), rules: store::bind_rules(agent), ..Default::default() });
    }

    #[test]
    fn a_bound_agent_writes_only_to_its_contact() {
        let mut all = store::Contacts::new();
        bound(&mut all, "x", "guide", false);
        all.insert("y".into(), store::Contact::default());
        assert!(may_write(&all, "guide", Some("x")).is_ok());
        assert!(may_write(&all, "guide", Some("y")).unwrap_err().contains("bound to contact x"));
        assert!(may_write(&all, "guide", None).unwrap_err().contains("not to agents on this machine"));
        // Not exclusive: other local agents still reach x, and anything else.
        assert!(may_write(&all, "planner", Some("x")).is_ok());
        assert!(may_write(&all, "planner", Some("y")).is_ok());
        assert!(may_write(&all, "planner", None).is_ok());
        bound(&mut all, "x", "guide", true);
        assert!(may_write(&all, "planner", Some("x")).unwrap_err().contains("exclusive"));
        assert!(may_write(&all, "inbox", Some("x")).is_err());
        assert!(may_write(&all, "guide", Some("x")).is_ok());
    }

    #[test]
    fn other_contacts_do_not_see_a_bound_agent() {
        let mut all = store::Contacts::new();
        bound(&mut all, "x", "guide", false);
        let a = |n: &str| Agent { name: n.into(), harness: "mcp".into(), status: String::new(), r#where: String::new() };
        let roster = vec![a("guide"), a("planner")];
        assert_eq!(roster_for(&roster, "y", &all).len(), 1);
        assert_eq!(roster_for(&roster, "x", &all).len(), 2);
    }
}
