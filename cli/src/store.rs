//! Local state in ~/.letagents (or $LETAGENTS_HOME): identity, contacts, pending requests, seen ids.

use crate::crypto::{b64, now_ms, sha256, Identity, Peer, MAX_AGE_MS, RULE_KINDS};
use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub fn home() -> PathBuf {
    std::env::var_os("LETAGENTS_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".letagents"))
}

fn path(name: &str) -> PathBuf {
    home().join(name)
}

pub fn read<T: DeserializeOwned + Default>(name: &str) -> T {
    std::fs::read(path(name)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn write<T: Serialize>(name: &str, v: &T) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(home())?;
    // A directory made before (by hand, or by another tool) may be open to other users: close it.
    if std::fs::metadata(home())?.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(home(), std::fs::Permissions::from_mode(0o700))?;
    }
    let tmp = path(&format!("{name}.tmp"));
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(v)?)?;
    }
    std::fs::rename(tmp, path(name))?;
    Ok(())
}

pub fn identity() -> Result<Identity> {
    let b = std::fs::read(path("identity.json")).with_context(|| format!("no identity in {}; run: letagents init <name>", home().display()))?;
    Ok(serde_json::from_slice(&b)?)
}

pub fn create_identity(name: &str) -> Result<Identity> {
    if path("identity.json").exists() {
        bail!("{} exists; refusing to overwrite keys", path("identity.json").display());
    }
    let id = Identity::generate(name);
    write("identity.json", &id)?;
    Ok(id)
}

/// A trusted peer and what it may do. Rules live only on this device.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Contact {
    pub address: String,
    pub allow: Vec<String>,
    /// Default local agent for its messages
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub added: i64,
    /// Per-agent rules. If any exist, a message must also match one: their agent (`from`), our agent (`to`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<AgentRule>,
    /// The one-time code from their invite; sent in our hello so they add us at once
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite: Option<String>,
    /// The drop code we gave this contact (drops.rs). The relay holds only its SHA-256.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub our_drop: Option<String>,
    /// It confirmed (`drop_ok`) that it writes to us with `our_drop`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub our_drop_ok: bool,
    /// The one local agent this contact is bound to (`--agent`): the daemon lets that agent write only to
    /// this contact. Local only, never sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<Bind>,
    /// The human's reminder about this contact (`--note`), shown in the label of its messages. Not a control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A contact bound to one local agent. See docs/DECISIONS.md #13.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Bind {
    pub agent: String,
    /// Only the bound agent may write to this contact (other local agents are refused).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exclusive: bool,
}

/// What `--agent`, `--exclusive` and `--note` ask for. Kept with an invite until the invite is used.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct BindSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exclusive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl BindSpec {
    pub fn is_empty(&self) -> bool {
        self.agent.is_none() && self.note.is_none()
    }
}

/// The inbound rules of a bound contact: its agents may send, ask and reply to `agent`, nothing else
/// (no other agent, no `list`).
pub fn bind_rules(agent: &str) -> Vec<AgentRule> {
    vec![
        AgentRule { from: "*".into(), to: agent.into(), allow: vec!["send".into(), "ask".into(), "reply".into()] },
        AgentRule { from: "*".into(), to: "*".into(), allow: vec![] },
    ]
}

/// A note as shown in a label: one line, at most 120 characters.
pub fn clean_note(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().chars().take(120).collect()
}

/// The contact a local agent is bound to, if any.
pub fn bound_to(agent: &str, all: &Contacts) -> Option<(String, Bind)> {
    all.iter().find_map(|(n, c)| c.bind.as_ref().filter(|b| b.agent == agent).map(|b| (n.clone(), b.clone())))
}

/// Can `agent` be bound to `contact` (None: a contact still to come, from an invite)?
pub fn check_bind_agent(agent: &str, contact: Option<&str>, all: &Contacts) -> Result<()> {
    let bad = agent.is_empty() || agent.len() > 80 || agent.contains(['/', '@', '*']) || agent.chars().any(char::is_control);
    if bad || agent == "inbox" || agent == "letagents-daemon" || all.contains_key(agent) {
        bail!("{agent:?} cannot be bound: use a plain local agent name (no / @ *, not a contact, not inbox)");
    }
    if let Some((other, _)) = bound_to(agent, all).filter(|(o, _)| Some(o.as_str()) != contact) {
        bail!("{agent} is already bound to contact {other}: one agent serves one contact (letagents contact unbind {other})");
    }
    Ok(())
}

/// Bind contact `name` to a local agent (writes its inbound rules) and/or set its note.
pub fn apply_bind(name: &str, spec: &BindSpec) -> Result<()> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = contacts();
    if let Some(agent) = &spec.agent {
        check_bind_agent(agent, Some(name), &all)?;
    }
    let c = all.get_mut(name).with_context(|| format!("no contact {name}"))?;
    if let Some(agent) = &spec.agent {
        c.rules = bind_rules(agent);
        c.bind = Some(Bind { agent: agent.clone(), exclusive: spec.exclusive });
    }
    if let Some(note) = &spec.note {
        let n = clean_note(note);
        c.note = if n.is_empty() { None } else { Some(n) };
    }
    write("contacts.json", &all)
}

/// Undo a binding: the contact may reach every agent again (its allow list applies). Returns the agent.
pub fn unbind(name: &str) -> Result<String> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = contacts();
    let c = all.get_mut(name).with_context(|| format!("no contact {name}"))?;
    let b = c.bind.take().with_context(|| format!("contact {name} is not bound"))?;
    if c.rules == bind_rules(&b.agent) {
        c.rules.clear();
    }
    write("contacts.json", &all)?;
    Ok(b.agent)
}

/// invite_binds.json: base64url(SHA-256(code)) -> the binding for whoever uses that invite.
type InviteBinds = BTreeMap<String, BindSpec>;

/// Keep a binding with an invite code; it is applied when the code is used.
pub fn save_invite_bind(code: &str, spec: &BindSpec) -> Result<()> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let live: Invites = read("invites.json");
    let mut all: InviteBinds = read("invite_binds.json");
    all.retain(|k, _| live.contains_key(k));
    all.insert(invite_key(code), spec.clone());
    write("invite_binds.json", &all)
}

/// May this contact see our whole roster (presence)? Only if it may `list` and no machine-wide rule says no.
pub fn may_see_roster(c: &Contact) -> bool {
    c.allow.iter().any(|k| k == "list") && (c.rules.is_empty() || c.rules.iter().find(|r| r.to == "*").is_some_and(|r| r.from == "*" && r.allow.iter().any(|k| k == "list")))
}

/// An invite code is valid this long, for one use.
pub const INVITE_TTL_MS: i64 = 7 * 24 * 3600 * 1000;

/// invites.json: base64url(SHA-256(code)) -> expiry (ms). Only the hash is kept.
type Invites = BTreeMap<String, i64>;

pub fn invite_key(code: &str) -> String {
    b64(&sha256(&[code.trim().as_bytes()]))
}

/// A new one-time invite code (16 random bytes, base64url), valid for 7 days.
pub fn new_invite() -> Result<String> {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    let code = b64(&b);
    let mut all: Invites = read("invites.json");
    all.retain(|_, exp| *exp > now_ms());
    all.insert(invite_key(&code), now_ms() + INVITE_TTL_MS);
    write("invites.json", &all)?;
    Ok(code)
}

/// Serializes read-modify-write of invites.json and contacts.json inside the daemon: two hellos that carry
/// one code and arrive together must not both redeem it.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Read-modify-write contacts.json under the lock; `f` returns true when it changed something.
pub fn update_contacts(f: impl FnOnce(&mut Contacts) -> bool) -> bool {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = contacts();
    let changed = f(&mut all);
    if changed {
        let _ = write("contacts.json", &all);
    }
    changed
}

/// Use an invite code: true once if it exists and has not expired; it is deleted either way.
pub fn redeem_invite(code: &str) -> bool {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut all: Invites = read("invites.json");
    let hit = all.remove(&invite_key(code)).is_some_and(|exp| exp > now_ms());
    all.retain(|_, exp| *exp > now_ms());
    let _ = write("invites.json", &all);
    hit
}

/// An invite is an address with a 5th part, the code: `la1.<sig>.<box>.<drop>.<code>`.
pub fn split_invite(addr: &str) -> (String, Option<String>) {
    let p: Vec<&str> = addr.trim().split('.').collect();
    if p.len() == 5 {
        (p[..4].join("."), Some(p[4].to_string()))
    } else {
        (addr.trim().to_string(), None)
    }
}

/// Add the sender of a hello that carried a valid invite code. Name: its claimed name, made unique.
/// `our_drop`: the invite's drop code, which the joiner already writes with; it becomes this contact's code.
/// A binding kept with the invite (`invite --agent`) is applied in the same write, so no message can come
/// in between. Returns the name and the bound agent.
pub fn accept_invited(sender: &Peer, claimed_name: &str, our_drop: Option<String>, code: &str) -> Result<(String, Option<String>)> {
    let base: String = claimed_name.chars().filter(|c| c.is_ascii_alphanumeric() || "-_".contains(*c)).take(32).collect();
    let base = if base.is_empty() { "agent".to_string() } else { base };
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = contacts();
    let mut name = base.clone();
    let mut n = 2;
    while all.contains_key(&name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    let allow = RULE_KINDS.iter().map(|k| k.to_string()).collect();
    let our_drop_ok = our_drop.is_some();
    let mut c = Contact { address: sender.address(true), allow, added: now_ms(), our_drop, our_drop_ok, ..Default::default() };
    let mut binds: InviteBinds = read("invite_binds.json");
    let mut bound = None;
    if let Some(spec) = binds.remove(&invite_key(code)) {
        let _ = write("invite_binds.json", &binds);
        c.note = spec.note.as_deref().map(clean_note).filter(|n| !n.is_empty());
        if let Some(agent) = spec.agent {
            if bound_to(&agent, &all).is_some() || all.contains_key(&agent) {
                // The agent was bound to someone else since the invite was made: fail closed, reach no one.
                c.rules = vec![AgentRule { from: "*".into(), to: "*".into(), allow: vec![] }];
            } else {
                c.rules = bind_rules(&agent);
                c.bind = Some(Bind { agent: agent.clone(), exclusive: spec.exclusive });
                bound = Some(agent);
            }
        }
    }
    all.insert(name.clone(), c);
    write("contacts.json", &all)?;
    Ok((name, bound))
}

/// A rule for one pair of agents. `*` matches any name; `prefix*` matches by prefix.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AgentRule {
    pub from: String,
    pub to: String,
    pub allow: Vec<String>,
}

pub fn glob(pat: &str, name: &str) -> bool {
    match pat.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pat == name,
    }
}

pub type Contacts = BTreeMap<String, Contact>;

pub fn contacts() -> Contacts {
    read("contacts.json")
}

/// Find the contact for a verified sender. Err if unknown or presenting a different key.
pub fn contact_of(sender: &Peer) -> std::result::Result<(String, Contact), String> {
    for (name, c) in contacts() {
        let Ok(p) = Peer::parse(&c.address) else { continue };
        if p.sig != sender.sig {
            continue;
        }
        if p.rcv != sender.rcv {
            return Err(format!("contact {name} presented a different key"));
        }
        return Ok((name, c));
    }
    Err(format!("unknown sender {}", sender.safety_number()))
}

/// May this contact's agent `from` send `kind` to our agent `to`? Returns the contact on success.
pub fn authorize(sender: &Peer, kind: &str, from: &str, to: &str) -> std::result::Result<(String, Contact), String> {
    let (name, c) = contact_of(sender)?;
    if !c.allow.iter().any(|k| k == kind) {
        return Err(format!("contact {name} is not allowed to {kind}"));
    }
    // Rules are checked in order; the first rule whose agents match decides (like a firewall).
    // `list` is about the whole machine: only machine-wide rules (`--to '*'`) decide it.
    if !c.rules.is_empty() {
        let hit = c.rules.iter().find(|r| glob(&r.from, from) && if kind == "list" { r.to == "*" } else { glob(&r.to, to) });
        match hit {
            Some(r) if r.allow.iter().any(|k| k == kind) => {}
            Some(_) => return Err(format!("rule: {name}/{from} may not {kind} {to}")),
            None => return Err(format!("no rule matches {name}/{from} -> {to}")),
        }
    }
    Ok((name, c))
}

/// A reply to an ask that our agent sent to this same contact: the ask invited it, so per-agent rules do not
/// apply to it (the contact's allow list does). The reply goes only to the agent that asked.
pub fn authorize_answer(sender: &Peer) -> std::result::Result<(String, Contact), String> {
    let (name, c) = contact_of(sender)?;
    if !c.allow.iter().any(|k| k == "reply") {
        return Err(format!("contact {name} is not allowed to reply"));
    }
    Ok((name, c))
}

/// Groups: a name -> member addresses (`contact/agent` or `contact`). Kept on this machine only.
pub fn groups() -> BTreeMap<String, Vec<String>> {
    read("groups.json")
}

/// A contact request from an unknown agent (a "hello"). Never delivered to agents as instructions.
#[derive(Serialize, Deserialize, Clone)]
pub struct Request {
    pub address: String,
    pub name: String,
    pub safety: String,
    pub ts: i64,
}

pub fn requests() -> BTreeMap<String, Request> {
    read("requests.json")
}

#[derive(Serialize, Deserialize, Default)]
pub struct Seen(pub BTreeMap<String, i64>);

impl Seen {
    /// Record a message id; false if it was already seen (a replay).
    pub fn insert(&mut self, id: &str, ts: i64) -> bool {
        let cut = now_ms() - MAX_AGE_MS;
        self.0.retain(|_, t| *t >= cut);
        self.0.insert(id.to_string(), ts).is_none()
    }
}

/// Queue a hello from an unknown agent as a pending contact request. Returns its short id.
/// The id is made here, never taken from the message: a sender must not be able to pick the id of
/// someone else's request and replace it. A new hello from the same keys keeps its id.
pub fn record_request(sender: &Peer, claimed_name: &str, ts: i64) -> (String, Request) {
    let name: String = claimed_name.chars().filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).take(32).collect();
    let req = Request { address: sender.address(true), name, safety: sender.safety_number(), ts };
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = requests();
    let same = |r: &Request| Peer::parse(&r.address).is_ok_and(|p| p.same_keys(sender));
    let rid = all.iter().find(|(_, r)| same(r)).map(|(k, _)| k.clone()).unwrap_or_else(|| loop {
        use rand::RngCore;
        let k = data_encoding::HEXLOWER.encode(&rand::rngs::OsRng.next_u32().to_be_bytes());
        if !all.contains_key(&k) {
            break k;
        }
    });
    all.insert(rid.clone(), req.clone());
    let _ = write("requests.json", &all);
    (rid, req)
}
