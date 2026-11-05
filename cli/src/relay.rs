//! Talking to the relay over HTTPS (send, one-shot fetch, ack) and WSS (live inbox).

use crate::crypto::{b64, now_ms, Identity, Peer};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::Message;

pub fn relay_url() -> String {
    std::env::var("LETAGENTS_RELAY").unwrap_or_else(|_| "https://letagents.talk".into())
}

/// The Host the relay sees. `wrangler dev` rewrites Host to the route, so tests can override it.
fn auth_host() -> String {
    std::env::var("LETAGENTS_AUTH_HOST").unwrap_or_else(|_| {
        reqwest::Url::parse(&relay_url()).ok().and_then(|u| u.host_str().map(|h| match u.port() {
            Some(p) => format!("{h}:{p}"),
            None => h.to_string(),
        })).unwrap_or_default()
    })
}

/// Drop a sealed envelope into `to`'s mailbox. Anonymous: only the drop secret is presented.
pub async fn deliver(to: &Peer, envelope: Vec<u8>) -> Result<()> {
    let drop = to.drop.context("contact address has no drop secret; ask them for their full address")?;
    let r = reqwest::Client::new()
        .post(format!("{}/v1/send/{}", relay_url(), to.id()))
        .header("x-la-drop", b64(&drop))
        .header("content-type", "application/octet-stream")
        .body(envelope)
        .send()
        .await?;
    if r.status().as_u16() != 202 {
        bail!("relay refused: {} {}", r.status(), r.text().await.unwrap_or_default());
    }
    Ok(())
}

fn inbox_query(me: &Identity) -> String {
    let ts = now_ms().to_string();
    let dh = me.drop_hash();
    let sig = me.sign(format!("letagents-inbox-v1\n{}\n{}\n{ts}\n{dh}", auth_host(), me.id()).as_bytes());
    format!("ts={ts}&drop_hash={dh}&sig={sig}")
}

#[derive(Deserialize)]
pub struct Stored {
    pub seq: i64,
    pub ct: String,
}

impl Stored {
    pub fn bytes(&self) -> Result<Vec<u8>> {
        Ok(B64.decode(&self.ct)?)
    }
}

/// One-shot HTTPS fetch of everything waiting (does not delete; call `ack`).
pub async fn fetch(me: &Identity) -> Result<Vec<Stored>> {
    #[derive(Deserialize)]
    struct R {
        msgs: Vec<Stored>,
    }
    let r = reqwest::get(format!("{}/v1/inbox/{}?{}", relay_url(), me.id(), inbox_query(me))).await?;
    if !r.status().is_success() {
        bail!("relay refused: {} {}", r.status(), r.text().await.unwrap_or_default());
    }
    Ok(serde_json::from_slice::<R>(&r.bytes().await?)?.msgs)
}

pub async fn ack(me: &Identity, seqs: &[i64]) -> Result<()> {
    if seqs.is_empty() {
        return Ok(());
    }
    let ts = now_ms().to_string();
    let list = seqs.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let sig = me.sign(format!("letagents-ack-v1\n{}\n{}\n{ts}\n{list}", auth_host(), me.id()).as_bytes());
    let r = reqwest::Client::new()
        .post(format!("{}/v1/ack/{}?ts={ts}&seqs={list}&sig={sig}", relay_url(), me.id()))
        .send()
        .await?;
    if !r.status().is_success() {
        bail!("ack refused: {}", r.status());
    }
    Ok(())
}

/// Live inbox over WSS. Calls `on_msg` for each stored or new envelope and acks it afterwards.
/// Reconnects forever with capped, jittered backoff.
pub async fn live<F, Fut>(me: &Identity, mut on_msg: F) -> !
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut backoff = 1u64;
    loop {
        let url = format!("{}/v1/inbox/{}?{}", relay_url().replacen("http", "ws", 1), me.id(), inbox_query(me));
        match tokio_tungstenite::connect_async(url).await {
            Ok((ws, _)) => {
                backoff = 1;
                let (mut tx, mut rx) = ws.split();
                let mut ping = tokio::time::interval(std::time::Duration::from_secs(45));
                loop {
                    tokio::select! {
                        _ = ping.tick() => { if tx.send(Message::text(r#"{"t":"ping"}"#)).await.is_err() { break; } }
                        m = rx.next() => {
                            let Some(Ok(m)) = m else { break };
                            let Message::Text(t) = m else { continue };
                            #[derive(Deserialize)]
                            struct F { t: String, seq: Option<i64>, ct: Option<String> }
                            let Ok(f) = serde_json::from_str::<F>(t.as_str()) else { continue };
                            match (f.t.as_str(), f.seq, f.ct) {
                                ("ready", _, _) => eprintln!("inbox: connected"),
                                ("msg", Some(seq), Some(ct)) => {
                                    if let Ok(b) = B64.decode(ct) { on_msg(b).await; }
                                    let _ = tx.send(Message::text(format!(r#"{{"t":"ack","seq":{seq}}}"#))).await;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                eprintln!("inbox: disconnected");
            }
            Err(e) => eprintln!("inbox: connect failed: {e}"),
        }
        let jitter = (now_ms() as u64) % 1000;
        tokio::time::sleep(std::time::Duration::from_millis(backoff * 1000 + jitter)).await;
        backoff = (backoff * 2).min(60);
    }
}
