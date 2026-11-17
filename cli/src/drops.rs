//! Per-contact drop codes (PROTOCOL.md, "Drop codes"; docs/DECISIONS.md #12).
//!
//! Every contact gets its own drop code from us, sent inside sealed messages (`drop`) until it confirms it
//! (`drop_ok`). The relay keeps only SHA-256 of each code, at most 64, and never learns which contact a code
//! belongs to. Removing a contact deletes its code at the relay, so the relay refuses its sends.
//!
//! The shared code (identity `drop`, in the `whoami` address) stays: contacts on older versions write with it,
//! and a new contact added by a bare address uses it until it gets its own code. Rule: removing a contact also
//! replaces the shared code, at once if every remaining contact writes with its own code, else as soon as the
//! last one does (or is removed).
//!
//! State in drops.json: open invites' drop codes, the pending replacement, and what the relay holds.

use crate::crypto::{b64, now_ms, sha256, unb64, Identity, Inner, Peer};
use crate::store::{self, Contact, Contacts};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Most drop codes per mailbox (contacts and open invites). The relay enforces the same bound.
pub const MAX_CODES: usize = 64;

/// The body of `POST /v1/drops/<id>`: the shared code's hash and every live code's hash, sorted.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Default, Debug)]
pub struct Table {
    pub shared: String,
    pub drops: Vec<Code>,
}

/// One code's SHA-256 and its expiry (ms; 0 = until removed).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Code {
    pub h: String,
    #[serde(default)]
    pub exp: i64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct InviteDrop {
    pub drop: String,
    pub exp: i64,
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    /// Open invites: SHA-256(invite code) -> the drop code in that invite's address.
    #[serde(default)]
    invites: BTreeMap<String, InviteDrop>,
    /// A removal asked for a new shared code; done when no contact writes with the shared code.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    rotate: bool,
    /// What the relay holds after our last successful update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    relay: Option<Table>,
}

/// drops.json is changed by the daemon and by CLI commands; this serializes it inside one process.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn update<T>(f: impl FnOnce(&mut State) -> T) -> T {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut s: State = store::read("drops.json");
    let out = f(&mut s);
    let _ = store::write("drops.json", &s);
    out
}

fn state() -> State {
    store::read("drops.json")
}

/// base64url(SHA-256(code)), as the relay computes it from the `x-la-drop` header.
pub fn hash(code: &str) -> Option<String> {
    unb64::<16>(code).ok().map(|b| b64(&sha256(&[&b])))
}

fn new_code() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b64(&b)
}

fn live(c: &Code) -> bool {
    c.exp == 0 || c.exp > now_ms()
}

/// Is `code` in the relay's table (as of our last update)? A code the relay does not hold is never handed out.
fn registered(s: &State, code: &str) -> bool {
    let Some(h) = hash(code) else { return false };
    s.relay.as_ref().is_some_and(|t| t.drops.iter().any(|c| c.h == h && live(c)))
}

/// Keep only invites that are still open in invites.json.
fn prune_invites(s: &mut State) {
    let open: BTreeMap<String, i64> = store::read("invites.json");
    let now = now_ms();
    s.invites.retain(|k, v| v.exp > now && open.get(k).is_some_and(|e| *e > now));
}

fn in_use(s: &State, all: &Contacts) -> usize {
    all.values().filter(|c| c.our_drop.is_some()).count() + s.invites.len()
}

/// What the relay should hold now.
fn wanted(me: &Identity, s: &State) -> Table {
    let mut drops: Vec<Code> = store::contacts().values().filter_map(|c| c.our_drop.as_deref().and_then(hash)).map(|h| Code { h, exp: 0 }).collect();
    drops.extend(s.invites.values().filter_map(|i| hash(&i.drop).map(|h| Code { h, exp: i.exp })));
    // Sorted by hash: the order says nothing about which contact a code belongs to.
    drops.sort();
    drops.dedup_by(|a, b| a.h == b.h);
    drops.truncate(MAX_CODES);
    Table { shared: me.drop_hash(), drops }
}

/// Give every contact that has no drop code its own, while fewer than MAX_CODES are in use. True if any.
pub fn issue_missing() -> bool {
    let mut s = state();
    prune_invites(&mut s);
    store::update_contacts(|all| {
        let mut n = in_use(&s, all);
        let mut changed = false;
        for c in all.values_mut().filter(|c| c.our_drop.is_none()) {
            if n >= MAX_CODES {
                break;
            }
            c.our_drop = Some(new_code());
            c.our_drop_ok = false;
            n += 1;
            changed = true;
        }
        changed
    })
}

/// A drop code for a new invite. It goes in the invite's address and becomes the joiner's own code.
pub fn invite_drop(invite_code: &str, exp: i64) -> Result<String> {
    let all = store::contacts();
    update(|s| {
        prune_invites(s);
        if in_use(s, &all) >= MAX_CODES {
            bail!("{MAX_CODES} drop codes in use (contacts and open invites): remove a contact or wait for invites to expire");
        }
        let d = new_code();
        s.invites.insert(store::invite_key(invite_code), InviteDrop { drop: d.clone(), exp });
        Ok(d)
    })
}

/// Forget an invite's drop code (its invite could not be registered at the relay).
pub fn drop_invite(invite_code: &str) {
    update(|s| s.invites.remove(&store::invite_key(invite_code)));
}

/// The drop code of an invite that was just redeemed: it becomes the new contact's code.
pub fn take_invite(invite_code: &str) -> Option<String> {
    update(|s| s.invites.remove(&store::invite_key(invite_code)).map(|i| i.drop))
}

/// Add the drop fields to a message for contact `c`: `drop_ok` (we write to it with this code) and, until it
/// confirms, `drop` (write to us with this one).
pub fn stamp(inner: &mut Inner, c: &Contact) {
    inner.drop_ok = Peer::parse(&c.address).ok().and_then(|p| p.drop).map(|d| b64(&sha256(&[&d])));
    if !c.our_drop_ok {
        let s = state();
        inner.drop = c.our_drop.clone().filter(|d| registered(&s, d));
    }
}

/// Our full address for a hello to `c`: with its own code when the relay holds it, else the shared code.
pub fn hello_from(me: &Identity, c: &Contact) -> String {
    let mut p = me.peer();
    if let Some(d) = c.our_drop.as_deref().filter(|d| registered(&state(), d)).and_then(|d| unb64::<16>(d).ok()) {
        p.drop = Some(d);
    }
    p.address(true)
}

#[derive(Default)]
pub struct Learned {
    /// The contact gave us a new code to write to it with.
    pub new_code: bool,
    /// The shared code was replaced: the relay needs an update.
    pub rotated: bool,
}

/// Apply the drop fields of a verified message from a known contact. Only that contact's own entry changes:
/// a contact can choose how we reach it, nothing else.
pub fn learn(sender: &Peer, inner: &Inner) -> Learned {
    let mut out = Learned::default();
    if inner.drop.is_none() && inner.drop_ok.is_none() {
        return out;
    }
    let mut confirmed = false;
    store::update_contacts(|all| {
        let Some(c) = all.values_mut().find(|c| Peer::parse(&c.address).is_ok_and(|p| p.same_keys(sender))) else { return false };
        let mut changed = false;
        if let (Some(d), Ok(mut p)) = (inner.drop.as_deref().and_then(|d| unb64::<16>(d).ok()), Peer::parse(&c.address)) {
            if p.drop != Some(d) {
                p.drop = Some(d);
                c.address = p.address(true);
                out.new_code = true;
                changed = true;
            }
        }
        if let Some(ok) = &inner.drop_ok {
            let now_ok = c.our_drop.as_deref().and_then(hash).is_some_and(|h| &h == ok);
            if now_ok != c.our_drop_ok {
                c.our_drop_ok = now_ok;
                confirmed = now_ok;
                changed = true;
            }
        }
        changed
    });
    if confirmed {
        out.rotated = rotate_if_due();
    }
    out
}

/// Contacts that do not yet write to us with their own code (they may use the shared code).
pub fn waiting() -> Vec<String> {
    store::contacts().into_iter().filter(|(_, c)| !(c.our_drop.is_some() && c.our_drop_ok)).map(|(n, _)| n).collect()
}

/// After a contact is removed: replace the shared code now if no contact needs it, else when none does.
/// Returns the contacts that hold the replacement up.
pub fn after_remove() -> Vec<String> {
    update(|s| s.rotate = true);
    rotate_if_due();
    waiting()
}

/// Replace the shared code if a removal asked for it and every contact writes with its own code.
fn rotate_if_due() -> bool {
    if !state().rotate || !waiting().is_empty() {
        return false;
    }
    let Ok(me) = store::identity() else { return false };
    if store::write("identity.json", &me.with_new_drop()).is_err() {
        return false;
    }
    update(|s| s.rotate = false);
    true
}

/// Serializes relay updates inside one process (each must carry a newer `ts`).
static SYNC: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Bring the relay's table in line with this machine's contacts, open invites and shared code. Without
/// `force`, nothing is sent when our last update already matches. Ok(true): the relay was updated.
pub async fn sync(force: bool) -> Result<bool> {
    let _g = SYNC.lock().await;
    let me = store::identity()?;
    let mut s = state();
    prune_invites(&mut s);
    let want = wanted(&me, &s);
    if !force && s.relay.as_ref() == Some(&want) {
        return Ok(false);
    }
    crate::relay::set_drops(&me, &serde_json::to_string(&want)?).await?;
    update(|s| {
        prune_invites(s);
        s.relay = Some(want);
    });
    Ok(true)
}

/// The relay reported how many live codes it holds (at login). A different number than our last update
/// (the relay erased an idle mailbox, or an update was lost): send the table again.
pub fn relay_count(n: i64) -> bool {
    let mine = state().relay.map(|t| t.drops.iter().filter(|c| live(c)).count()).unwrap_or(0);
    mine as i64 != n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_learn_round_trip() {
        let dir = std::env::temp_dir().join(format!("letagents-drops-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("LETAGENTS_HOME", &dir);
        let (a, b) = (Identity::generate("a"), Identity::generate("b"));
        store::write("identity.json", &a).unwrap();
        let mut all = Contacts::new();
        all.insert("b".into(), Contact { address: b.peer().address(true), allow: vec![], session: None, added: 0, rules: vec![], invite: None, our_drop: None, our_drop_ok: false, ..Default::default() });
        store::write("contacts.json", &all).unwrap();
        assert!(issue_missing());
        assert!(!issue_missing());
        let c = store::contacts()["b"].clone();
        let code = c.our_drop.clone().unwrap();
        // Not at the relay yet: never handed out, and a hello carries the shared code.
        let mut m = Inner::default();
        stamp(&mut m, &c);
        assert!(m.drop.is_none());
        assert_eq!(m.drop_ok, Some(b.drop_hash()));
        assert_eq!(hello_from(&a, &c), a.peer().address(true));
        update(|s| s.relay = Some(wanted(&a, s)));
        stamp(&mut m, &c);
        assert_eq!(m.drop.as_deref(), Some(code.as_str()));
        assert!(hello_from(&a, &c).ends_with(&code));
        // b confirms with the hash of our code, and gives us its own code.
        let theirs = new_code();
        let reply = Inner { drop: Some(theirs.clone()), drop_ok: hash(&code), ..Default::default() };
        let l = learn(&b.peer(), &reply);
        assert!(l.new_code && !l.rotated);
        let c = store::contacts()["b"].clone();
        assert!(c.our_drop_ok && c.address.ends_with(&theirs));
        assert!(waiting().is_empty());
        // Removing a contact when nobody else needs the shared code replaces it at once.
        let before = store::identity().unwrap().drop_hash();
        store::write("contacts.json", &Contacts::new()).unwrap();
        assert!(after_remove().is_empty());
        assert_ne!(store::identity().unwrap().drop_hash(), before);
        assert_eq!(store::identity().unwrap().id(), a.id());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
