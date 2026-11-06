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
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(home())?;
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
#[derive(Serialize, Deserialize, Clone)]
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
}

/// An invite code is valid this long, for one use.
pub const INVITE_TTL_MS: i64 = 7 * 24 * 3600 * 1000;

/// invites.json: base64url(SHA-256(code)) -> expiry (ms). Only the hash is kept.
type Invites = BTreeMap<String, i64>;

fn invite_key(code: &str) -> String {
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

/// Use an invite code: true once if it exists and has not expired; it is deleted either way.
pub fn redeem_invite(code: &str) -> bool {
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
pub fn accept_invited(sender: &Peer, claimed_name: &str) -> Result<String> {
    let base: String = claimed_name.chars().filter(|c| c.is_ascii_alphanumeric() || "-_".contains(*c)).take(32).collect();
    let base = if base.is_empty() { "agent".to_string() } else { base };
    let mut all = contacts();
    let mut name = base.clone();
    let mut n = 2;
    while all.contains_key(&name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    let allow = RULE_KINDS.iter().map(|k| k.to_string()).collect();
    all.insert(name.clone(), Contact { address: sender.address(true), allow, session: None, added: now_ms(), rules: vec![], invite: None });
    write("contacts.json", &all)?;
    Ok(name)
}

/// A rule for one pair of agents. `*` matches any name; `prefix*` matches by prefix.
#[derive(Serialize, Deserialize, Clone, Debug)]
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
pub fn record_request(sender: &Peer, claimed_name: &str, msg_id: &str, ts: i64) -> (String, Request) {
    let name: String = claimed_name.chars().filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).take(32).collect();
    let req = Request { address: sender.address(true), name, safety: sender.safety_number(), ts };
    let rid: String = msg_id.chars().take(8).collect();
    let mut all = requests();
    all.insert(rid.clone(), req.clone());
    let _ = write("requests.json", &all);
    (rid, req)
}
