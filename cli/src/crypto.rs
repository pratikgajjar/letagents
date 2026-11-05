//! Keys, addresses and sealed envelopes. Everything here runs on the device; see PROTOCOL.md.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as XPublic, StaticSecret};

/// Message kinds. `send`/`ask`/`reply` carry agent text; `list`/`roster` share which agents are online;
/// `receipt` reports delivery or refusal; `hello` requests contact.
pub const KINDS: [&str; 7] = ["send", "ask", "reply", "list", "roster", "receipt", "hello"];
/// Kinds a contact rule can allow.
pub const RULE_KINDS: [&str; 4] = ["send", "ask", "reply", "list"];
const MSG_DOMAIN: &[u8] = b"letagents-msg-v1\n";
const HKDF_INFO: &[u8] = b"letagents-v1 aes-256-gcm";
pub const MAX_AGE_MS: i64 = 48 * 3600 * 1000 + 300_000;

pub fn b64(b: &[u8]) -> String {
    B64.encode(b)
}
pub fn unb64<const N: usize>(s: &str) -> Result<[u8; N]> {
    B64.decode(s)?.try_into().map_err(|_| anyhow!("expected {N} bytes"))
}
pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

/// This device's secrets. Stored at ~/.letagents/identity.json with mode 0600.
#[derive(Serialize, Deserialize, Clone)]
pub struct Identity {
    pub name: String,
    sig: String,
    rcv: String,
    pub drop: String,
}

/// Someone's public address: `la1.<sig pub>.<box pub>[.<drop>]`.
#[derive(Clone, PartialEq, Eq)]
pub struct Peer {
    pub sig: [u8; 32],
    pub rcv: [u8; 32],
    pub drop: Option<[u8; 16]>,
}

impl Identity {
    pub fn generate(name: &str) -> Self {
        let mut drop = [0u8; 16];
        OsRng.fill_bytes(&mut drop);
        Identity {
            name: name.into(),
            sig: b64(&SigningKey::generate(&mut OsRng).to_bytes()),
            rcv: b64(&StaticSecret::random_from_rng(OsRng).to_bytes()),
            drop: b64(&drop),
        }
    }
    pub fn signing(&self) -> SigningKey {
        SigningKey::from_bytes(&unb64::<32>(&self.sig).expect("identity sig key"))
    }
    fn receiving(&self) -> StaticSecret {
        StaticSecret::from(unb64::<32>(&self.rcv).expect("identity box key"))
    }
    pub fn peer(&self) -> Peer {
        Peer {
            sig: self.signing().verifying_key().to_bytes(),
            rcv: XPublic::from(&self.receiving()).to_bytes(),
            drop: Some(unb64::<16>(&self.drop).expect("identity drop")),
        }
    }
    pub fn id(&self) -> String {
        b64(&self.peer().sig)
    }
    pub fn drop_hash(&self) -> String {
        b64(&sha256(&[&unb64::<16>(&self.drop).unwrap()]))
    }
    pub fn sign(&self, msg: &[u8]) -> String {
        b64(&self.signing().sign(msg).to_bytes())
    }
}

impl Peer {
    pub fn parse(addr: &str) -> Result<Self> {
        let p: Vec<&str> = addr.trim().split('.').collect();
        if p.first() != Some(&"la1") || !(p.len() == 3 || p.len() == 4) {
            bail!("not a letagents address (la1.<sig>.<box>[.<drop>])");
        }
        Ok(Peer {
            sig: unb64(p[1]).context("address sig key")?,
            rcv: unb64(p[2]).context("address box key")?,
            drop: if p.len() == 4 { Some(unb64(p[3]).context("address drop")?) } else { None },
        })
    }
    pub fn address(&self, with_drop: bool) -> String {
        let mut a = format!("la1.{}.{}", b64(&self.sig), b64(&self.rcv));
        if let (true, Some(d)) = (with_drop, self.drop) {
            a.push('.');
            a.push_str(&b64(&d));
        }
        a
    }
    pub fn id(&self) -> String {
        b64(&self.sig)
    }
    /// Short fingerprint humans compare over a channel they already trust.
    pub fn safety_number(&self) -> String {
        let h = sha256(&[&self.sig, &self.rcv]);
        let s = data_encoding::BASE32_NOPAD.encode(&h[..20]);
        s.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join(" ")
    }
    pub fn same_keys(&self, o: &Peer) -> bool {
        self.sig == o.sig && self.rcv == o.rcv
    }
}

/// The signed, encrypted content of a message.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Inner {
    pub v: u8,
    pub from: String,
    pub to: String,
    pub ts: i64,
    pub id: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// Sending agent on the sender's machine (vouched for by the sender's device key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_agent: Option<String>,
    /// Target agent on the recipient's machine.
    #[serde(default, alias = "session", skip_serializing_if = "Option::is_none")]
    pub to_agent: Option<String>,
    pub body: String,
}

#[derive(Serialize, Deserialize)]
struct Plain {
    inner: String,
    sig: String,
}

fn derive_key(shared: &[u8; 32], eph: &[u8; 32], rcv: &[u8; 32]) -> [u8; 32] {
    let mut salt = [0u8; 64];
    salt[..32].copy_from_slice(eph);
    salt[32..].copy_from_slice(rcv);
    let mut key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&salt), shared).expand(HKDF_INFO, &mut key).expect("hkdf length");
    key
}
fn aad(rcpt_id: &str) -> Vec<u8> {
    [b"letagents-v1".as_slice(), rcpt_id.as_bytes()].concat()
}

/// Sign `inner` with our key, then encrypt it to `to` with a fresh one-time X25519 key.
pub fn seal(me: &Identity, to: &Peer, inner: &Inner) -> Result<Vec<u8>> {
    let eph = StaticSecret::random_from_rng(OsRng);
    let eph_pub = XPublic::from(&eph).to_bytes();
    let inner_json = serde_json::to_string(inner)?;
    let signed = [MSG_DOMAIN, &eph_pub, b"\n", inner_json.as_bytes()].concat();
    let plain = serde_json::to_vec(&Plain { inner: inner_json, sig: me.sign(&signed) })?;
    let mut padded = plain;
    let pad = (1024 - padded.len() % 1024) % 1024;
    padded.extend(std::iter::repeat_n(b' ', pad));

    let shared = eph.diffie_hellman(&XPublic::from(to.rcv));
    let key = derive_key(shared.as_bytes(), &eph_pub, &to.rcv);
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ct = Aes256Gcm::new_from_slice(&key)?
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: &padded, aad: &aad(&to.id()) })
        .map_err(|_| anyhow!("encrypt failed"))?;
    Ok([b"LA1".as_slice(), &eph_pub, &nonce, &ct].concat())
}

/// Decrypt an envelope addressed to us and verify the sender's signature.
pub fn open(me: &Identity, env: &[u8]) -> Result<(Inner, Peer)> {
    if env.len() < 63 || &env[..3] != b"LA1" {
        bail!("bad envelope");
    }
    let eph_pub: [u8; 32] = env[3..35].try_into()?;
    let nonce = &env[35..47];
    let my = me.peer();
    let shared = me.receiving().diffie_hellman(&XPublic::from(eph_pub));
    let key = derive_key(shared.as_bytes(), &eph_pub, &my.rcv);
    let padded = Aes256Gcm::new_from_slice(&key)?
        .decrypt(Nonce::from_slice(nonce), Payload { msg: &env[47..], aad: &aad(&my.id()) })
        .map_err(|_| anyhow!("cannot decrypt (tampered, or not for us)"))?;
    let plain: Plain = serde_json::from_slice(padded.trim_ascii_end())?;
    let inner: Inner = serde_json::from_str(&plain.inner)?;
    let sender = Peer::parse(&inner.from)?;
    let signed = [MSG_DOMAIN, &eph_pub, b"\n", plain.inner.as_bytes()].concat();
    VerifyingKey::from_bytes(&sender.sig)?
        .verify_strict(&signed, &Signature::from_bytes(&unb64::<64>(&plain.sig)?))
        .map_err(|_| anyhow!("bad signature"))?;
    if inner.v != 1 || inner.to != my.id() {
        bail!("not addressed to us");
    }
    if !KINDS.contains(&inner.kind.as_str()) {
        bail!("unknown kind");
    }
    if (now_ms() - inner.ts).abs() > MAX_AGE_MS {
        bail!("stale message");
    }
    Ok((inner, sender))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(from: &Identity, to: &Identity, body: &str) -> Inner {
        Inner {
            v: 1,
            from: from.peer().address(false),
            to: to.id(),
            ts: now_ms(),
            id: uuid::Uuid::new_v4().to_string(),
            kind: "send".into(),
            reply_to: None,
            from_agent: None,
            to_agent: None,
            body: body.into(),
        }
    }

    #[test]
    fn seal_open_tamper_and_misroute() {
        let (a, b, c) = (Identity::generate("a"), Identity::generate("b"), Identity::generate("c"));
        let env = seal(&a, &b.peer(), &msg(&a, &b, "hi")).unwrap();
        assert_eq!(env.len() % 1024, 3 + 32 + 12 + 16);
        let (inner, sender) = open(&b, &env).unwrap();
        assert_eq!(inner.body, "hi");
        assert!(sender.same_keys(&a.peer()));
        let mut bad = env.clone();
        bad[100] ^= 1;
        assert!(open(&b, &bad).is_err());
        assert!(open(&c, &env).is_err());
    }
}
