//! Codex push: the shared Codex app-server daemon (`codex app-server daemon`, which interactive `codex`
//! uses unless started with `--no-daemon`) speaks JSON-RPC over a WebSocket on a local Unix socket,
//! `$CODEX_HOME/app-server-control/app-server-control.sock`. `turn/start` on a loaded thread starts a turn
//! with our text as the user message (or adds it to the running turn), so an idle Codex session wakes up.
//! Everything stays on this machine. One short connection per call.

use anyhow::{anyhow, bail, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;

const TIMEOUT: Duration = Duration::from_secs(5);

/// A Codex thread loaded in the shared app-server, i.e. a session that is open now.
#[derive(Clone, Debug)]
pub struct Thread {
    pub id: String,
    pub status: String,
    pub cwd: String,
}

/// The letagents name of a Codex thread: `codex-` and the last 8 hex digits of its id.
pub fn name_for(thread_id: &str) -> String {
    let hex: String = thread_id.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    format!("codex-{}", &hex[hex.len().saturating_sub(8)..])
}

fn socket() -> PathBuf {
    let home = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".codex"));
    home.join("app-server-control").join("app-server-control.sock")
}

struct Conn {
    ws: tokio_tungstenite::WebSocketStream<UnixStream>,
    next: u64,
}

impl Conn {
    async fn open() -> Result<Conn> {
        let stream = UnixStream::connect(socket()).await?;
        let (ws, _) = tokio_tungstenite::client_async("ws://localhost/", stream).await?;
        let mut c = Conn { ws, next: 0 };
        c.call("initialize", json!({ "clientInfo": { "name": "letagents", "version": env!("CARGO_PKG_VERSION") }, "capabilities": {} })).await?;
        c.ws.send(Message::text(json!({ "jsonrpc": "2.0", "method": "initialized" }).to_string())).await?;
        Ok(c)
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        let id = self.next;
        self.ws.send(Message::text(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string())).await?;
        loop {
            let m = self.ws.next().await.ok_or_else(|| anyhow!("codex app-server closed"))??;
            let Message::Text(t) = m else { continue };
            let v: Value = serde_json::from_str(&t)?;
            // Skip notifications and server requests; answer only our id.
            if v.get("method").is_some() || v["id"] != json!(id) {
                continue;
            }
            if let Some(e) = v.get("error") {
                bail!("{method}: {}", e["message"].as_str().unwrap_or("error"));
            }
            return Ok(v["result"].clone());
        }
    }
}

/// Run `f` on a fresh connection, all within TIMEOUT.
async fn timed<T>(f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(TIMEOUT, f).await.map_err(|_| anyhow!("codex app-server timed out"))?
}

/// Interactive top-level threads that are open now. Empty when no shared Codex daemon runs.
pub async fn threads() -> Vec<Thread> {
    if !socket().exists() {
        return vec![];
    }
    timed(async {
        let mut c = Conn::open().await?;
        let ids = c.call("thread/loaded/list", json!({})).await?;
        let mut out = vec![];
        for id in ids["data"].as_array().cloned().unwrap_or_default() {
            let Some(id) = id.as_str() else { continue };
            let Ok(r) = c.call("thread/read", json!({ "threadId": id })).await else { continue };
            let t = &r["thread"];
            // Skip one-off threads (ephemeral) and sub-agents (they have a parent and refuse direct input).
            if t["ephemeral"] == json!(true) || !t["parentThreadId"].is_null() || t["canAcceptDirectInput"] == json!(false) {
                continue;
            }
            let status = match t["status"]["type"].as_str().unwrap_or("idle") {
                "active" => "working",
                "systemError" => "error",
                s => s,
            };
            out.push(Thread { id: id.to_string(), status: status.to_string(), cwd: t["cwd"].as_str().unwrap_or_default().to_string() });
        }
        let _ = c.ws.close(None).await;
        Ok(out)
    })
    .await
    .unwrap_or_default()
}

/// Start a turn on an open thread with `text` as the user message. If a turn is running, Codex adds the
/// text to it. Refuses threads that are not open, so no turn ever runs where nobody watches.
pub async fn start_turn(thread_id: &str, text: &str) -> Result<()> {
    timed(async {
        let mut c = Conn::open().await?;
        let ids = c.call("thread/loaded/list", json!({})).await?;
        if !ids["data"].as_array().is_some_and(|a| a.iter().any(|x| x.as_str() == Some(thread_id))) {
            bail!("codex thread is not open");
        }
        c.call("turn/start", json!({ "threadId": thread_id, "input": [{ "type": "text", "text": text }] })).await?;
        let _ = c.ws.close(None).await;
        Ok(())
    })
    .await
}
