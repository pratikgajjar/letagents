//! Local state in ~/.letagents (or $LETAGENTS_HOME): identity, contacts, pending requests, seen ids.

use crate::crypto::{now_ms, Identity, Peer, MAX_AGE_MS};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub added: i64,
}

pub type Contacts = BTreeMap<String, Contact>;

pub fn contacts() -> Contacts {
    read("contacts.json")
}

/// Returns the contact name when this verified sender may send this kind, else the reason.
pub fn authorize(sender: &Peer, kind: &str) -> std::result::Result<(String, Contact), String> {
    for (name, c) in contacts() {
        let Ok(p) = Peer::parse(&c.address) else { continue };
        if p.sig != sender.sig {
            continue;
        }
        if p.rcv != sender.rcv {
            return Err(format!("contact {name} presented a different key"));
        }
        if !c.allow.iter().any(|k| k == kind) {
            return Err(format!("contact {name} is not allowed to {kind}"));
        }
        return Ok((name, c));
    }
    Err(format!("unknown sender {}", sender.safety_number()))
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
