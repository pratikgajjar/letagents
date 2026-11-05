//! The daemon: joins the local pi-intercom broker as one session per contact, and the relay as this
//! device's mailbox. Local sends/asks to a contact become sealed envelopes; verified inbound mail is
//! checked against the contact's rules and delivered to a local session. Remote asks keep their
//! ask/reply link, so the asker's `ask` closes when the reply arrives.

use crate::crypto::{open, Identity, Peer};
use crate::intercom::{stable_session_id, Local, Proxy};
use crate::store::{self, Seen};
use crate::{compose_and_send, relay};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// The local session that receives notices (contact requests, rejections).
pub const SYSTEM_SESSION: &str = "letagents";

#[derive(Default)]
struct Links {
    /// local message id (delivered to a local agent as an ask) -> (contact, remote envelope id)
    remote_ask_by_local: HashMap<String, (String, String)>,
    /// our envelope id (an ask we sealed) -> (proxy, local asker id, local message id)
    local_ask_by_remote: HashMap<String, (String, String, String)>,
}

fn log(s: impl AsRef<str>) {
    let t = crate::crypto::now_ms() / 1000 % 86400;
    println!("{:02}:{:02}:{:02} {}", t / 3600, t / 60 % 60, t % 60, s.as_ref());
}

pub async fn run(me: Identity, default_target: String) -> Result<()> {
    let me = Arc::new(me);
    let contacts = store::contacts();
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Local>();
    let (closed_tx, mut closed_rx) = mpsc::unbounded_channel::<String>();
    let mut proxies: HashMap<String, Arc<Proxy>> = HashMap::new();

    // Wait for a broker: pi sessions start it. Retrying here keeps the service from crash-looping.
    let system = loop {
        match Proxy::connect(SYSTEM_SESSION, &stable_session_id(&format!("system:{}", me.id())), in_tx.clone(), closed_tx.clone()).await {
            Ok(p) => break p,
            Err(e) => {
                log(format!("waiting for pi-intercom broker: {e}"));
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        }
    };
    for (name, c) in &contacts {
        let p = Proxy::connect(name, &stable_session_id(&c.address), in_tx.clone(), closed_tx.clone()).await?;
        log(format!("contact \"{name}\" is local session \"{name}\" (allow: {})", c.allow.join(",")));
        proxies.insert(name.clone(), p);
    }
    let proxies = Arc::new(proxies);
    let links = Arc::new(Mutex::new(Links::default()));

    // Local agents -> relay.
    {
        let (me, proxies, links) = (me.clone(), proxies.clone(), links.clone());
        tokio::spawn(async move {
            while let Some(m) = in_rx.recv().await {
                if m.proxy == SYSTEM_SESSION {
                    continue;
                }
                let Some(proxy) = proxies.get(&m.proxy).cloned() else { continue };
                let reply_link = m.reply_to.as_ref().and_then(|r| links.lock().unwrap().remote_ask_by_local.remove(r));
                let result = if let Some((contact, remote_id)) = reply_link.filter(|(c, _)| *c == m.proxy) {
                    compose_and_send(&me, &contact, "reply", &m.text, Some(remote_id), None).await.map(|_| "reply".to_string())
                } else {
                    let (session, body) = match m.text.trim_start().strip_prefix("to:").and_then(|r| r.split_once('\n')) {
                        Some((s, b)) => (Some(s.trim().to_string()), b.trim().to_string()),
                        None => (None, m.text.clone()),
                    };
                    let kind = if m.expects_reply { "ask" } else { "send" };
                    let body = format!("[from {}/{}]\n{body}", me.name, if m.from_name.is_empty() { &m.from_id } else { &m.from_name });
                    match compose_and_send(&me, &m.proxy, kind, &body, None, session).await {
                        Ok(id) => {
                            if kind == "ask" {
                                links.lock().unwrap().local_ask_by_remote.insert(id, (m.proxy.clone(), m.from_id.clone(), m.id.clone()));
                            }
                            Ok(kind.to_string())
                        }
                        Err(e) => Err(e),
                    }
                };
                match result {
                    Ok(kind) => log(format!("out {kind} -> {}", m.proxy)),
                    Err(e) => {
                        log(format!("out -> {} FAILED: {e}", m.proxy));
                        let _ = proxy.send(&m.from_id, &format!("[letagents] delivery to {} failed: {e}", m.proxy), None, false).await;
                    }
                }
            }
        });
    }

    // Relay -> local agents.
    {
        let (me2, proxies, links, system) = (me.clone(), proxies.clone(), links.clone(), system.clone());
        tokio::spawn(async move {
            let mut seen: Seen = store::read("seen.json");
            relay::live(&me2, |env| {
                let (me2, proxies, links, system, default_target) = (me2.clone(), proxies.clone(), links.clone(), system.clone(), default_target.clone());
                let fresh = match open(&me2, &env) {
                    Ok((inner, sender)) => {
                        let new = seen.insert(&inner.id, inner.ts);
                        let _ = store::write("seen.json", &seen);
                        if new { Some((inner, sender)) } else { log(format!("dropped replay {}", inner.id)); None }
                    }
                    Err(e) => { log(format!("dropped envelope: {e}")); None }
                };
                async move {
                    let Some((inner, sender)) = fresh else { return };
                    deliver_inbound(&proxies, &links, &system, &default_target, inner, sender).await;
                }
            })
            .await
        });
    }

    let gone = closed_rx.recv().await.unwrap_or_default();
    log(format!("local broker connection for \"{gone}\" closed; exiting so the service manager restarts us"));
    std::process::exit(4);
}

async fn deliver_inbound(
    proxies: &HashMap<String, Arc<Proxy>>,
    links: &Mutex<Links>,
    system: &Proxy,
    default_target: &str,
    inner: crate::crypto::Inner,
    sender: Peer,
) {
    match store::authorize(&sender, &inner.kind) {
        Err(reason) if inner.kind == "hello" && reason.starts_with("unknown sender") => {
            // A contact request. Store it; tell the local target only the safe metadata.
            let (rid, req) = store::record_request(&sender, &inner.body, &inner.id, inner.ts);
            let name = req.name.clone();
            log(format!("contact request {rid} from \"{name}\" ({})", req.safety));
            let note = format!(
                "[letagents] contact request {rid} from an agent calling itself \"{name}\", safety number {}.\nAsk your human to confirm this safety number with them, then run: letagents contact accept {rid} <name> --allow send,ask,reply",
                req.safety
            );
            let _ = system.send(default_target, &note, None, false).await;
        }
        Err(reason) => log(format!("REJECTED {} {}: {reason}", inner.kind, inner.id)),
        Ok((name, contact)) => {
            let Some(proxy) = proxies.get(&name) else {
                return log(format!("in from {name}: restart the daemon to load new contacts"));
            };
            if inner.kind == "hello" {
                return log(format!("hello from known contact {name}"));
            }
            if inner.kind == "reply" {
                let link = inner.reply_to.as_ref().and_then(|r| links.lock().unwrap().local_ask_by_remote.remove(r));
                let Some((_, local_from, local_msg)) = link else { return log(format!("reply from {name} matches no open ask")) };
                let r = proxy.send(&local_from, &inner.body, Some(&local_msg), false).await;
                return log(format!("in reply from {name}: {}", outcome(&r)));
            }
            let target = inner.session.clone().or(contact.session.clone()).unwrap_or_else(|| default_target.to_string());
            let ask = inner.kind == "ask";
            let header = if ask {
                format!("[letagents · verified {name}] ASK: reply to this message to answer it")
            } else {
                format!("[letagents · verified {name}] to answer, send to \"{name}\" (first line \"to: <their session>\" to pick one)")
            };
            let r = proxy.send(&target, &format!("{header}\n\n{}", inner.body), None, ask).await;
            if let (true, Ok(s)) = (ask, &r) {
                links.lock().unwrap().remote_ask_by_local.insert(s.id.clone(), (name.clone(), inner.id.clone()));
            }
            log(format!("in {} from {name} -> {target}: {}", inner.kind, outcome(&r)));
        }
    }
}

fn outcome(r: &Result<crate::intercom::Sent>) -> String {
    match r {
        Ok(s) if s.delivered => "delivered".into(),
        Ok(s) => format!("not delivered ({})", s.reason.clone().unwrap_or_default()),
        Err(e) => format!("error ({e})"),
    }
}
