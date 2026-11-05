//! A minimal pi-intercom client: the local `.sock` broker protocol (4-byte big-endian length, then JSON).
//! Each letagents contact registers as its own local session, so local agents just `send`/`ask` it.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

const MAX_FRAME: usize = 1024 * 1024;

pub fn broker_socket() -> PathBuf {
    let base = std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".pi/agent"));
    base.join("intercom/broker.sock")
}

/// A message a local agent sent to one of our proxy sessions.
#[derive(Debug)]
pub struct Local {
    pub proxy: String,
    pub from_id: String,
    pub from_name: String,
    pub id: String,
    pub reply_to: Option<String>,
    pub expects_reply: bool,
    pub text: String,
}

pub struct Sent {
    pub id: String,
    pub delivered: bool,
    pub reason: Option<String>,
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<(bool, Option<String>)>>>>;

pub struct Proxy {
    out: mpsc::UnboundedSender<Value>,
    pending: Pending,
}

async fn read_frame(r: &mut (impl AsyncReadExt + Unpin)) -> Result<Value> {
    let len = r.read_u32().await? as usize;
    if len > MAX_FRAME {
        bail!("frame too large");
    }
    let mut buf = vec![0; len];
    r.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

fn frame(v: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(v).expect("json");
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend(body);
    out
}

impl Proxy {
    /// Register `name` on the local broker with a stable session id. Local messages go to `inbound`;
    /// `closed` fires when the broker connection ends.
    pub async fn connect(name: &str, session_id: &str, inbound: mpsc::UnboundedSender<Local>, closed: mpsc::UnboundedSender<String>) -> Result<Arc<Proxy>> {
        let sock = broker_socket();
        let stream = UnixStream::connect(&sock).await.with_context(|| format!("no pi-intercom broker at {}", sock.display()))?;
        let (mut rd, mut wr) = stream.into_split();
        let now = crate::crypto::now_ms();
        wr.write_all(&frame(&json!({
            "type": "register",
            "sessionId": session_id,
            "session": {
                "cwd": std::env::current_dir().unwrap_or_default(),
                "model": "letagents",
                "pid": std::process::id(),
                "startedAt": now,
                "lastActivity": now,
                "name": name,
                "status": "idle",
            },
        })))
        .await?;
        let reg = read_frame(&mut rd).await?;
        if reg["type"] != "registered" {
            bail!("broker refused registration: {reg}");
        }

        let (out, mut out_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            while let Some(v) = out_rx.recv().await {
                if wr.write_all(&frame(&v)).await.is_err() {
                    break;
                }
            }
        });
        let pending: Pending = Arc::default();
        let (p2, n2) = (pending.clone(), name.to_string());
        tokio::spawn(async move {
            while let Ok(v) = read_frame(&mut rd).await {
                match v["type"].as_str() {
                    Some("message") => {
                        let (f, m) = (&v["from"], &v["message"]);
                        let _ = inbound.send(Local {
                            proxy: n2.clone(),
                            from_id: f["id"].as_str().unwrap_or_default().into(),
                            from_name: f["name"].as_str().unwrap_or_default().into(),
                            id: m["id"].as_str().unwrap_or_default().into(),
                            reply_to: m["replyTo"].as_str().map(Into::into),
                            expects_reply: m["expectsReply"].as_bool().unwrap_or(false),
                            text: m["content"]["text"].as_str().unwrap_or_default().into(),
                        });
                    }
                    Some(t @ ("delivered" | "delivery_failed")) => {
                        let id = v["messageId"].as_str().unwrap_or_default();
                        if let Some(tx) = p2.lock().unwrap().remove(id) {
                            let _ = tx.send((t == "delivered", v["reason"].as_str().map(Into::into)));
                        }
                    }
                    _ => {}
                }
            }
            let _ = closed.send(n2);
        });
        Ok(Arc::new(Proxy { out, pending }))
    }

    pub async fn send(&self, to: &str, text: &str, reply_to: Option<&str>, expects_reply: bool) -> Result<Sent> {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        let mut message = json!({ "id": id, "timestamp": crate::crypto::now_ms(), "content": { "text": text } });
        if let Some(r) = reply_to {
            message["replyTo"] = json!(r);
        }
        if expects_reply {
            message["expectsReply"] = json!(true);
        }
        self.out.send(json!({ "type": "send", "to": to, "message": message }))?;
        let (delivered, reason) = tokio::time::timeout(std::time::Duration::from_secs(10), rx).await.context("broker send timeout")??;
        Ok(Sent { id, delivered, reason })
    }
}

/// Stable UUID-shaped session id derived from a label, so asks survive restarts.
pub fn stable_session_id(label: &str) -> String {
    let h = data_encoding::HEXLOWER.encode(&crate::crypto::sha256(&[b"letagents-proxy:", label.as_bytes()]));
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}
