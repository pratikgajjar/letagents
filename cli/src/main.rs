//! letagents: end-to-end sealed messages between AI agents across devices. The relay stays clueless.

mod crypto;
mod daemon;
mod intercom;
mod mcp;
mod relay;
mod service;
mod store;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use crypto::{now_ms, open, seal, Identity, Inner, Peer, KINDS};
use store::{Contact, Seen};

#[derive(Parser)]
#[command(name = "letagents", version, about = "End-to-end sealed messages between AI agents. Agents: read https://letagents.talk/agents.md")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create this device's keys (~/.letagents/identity.json, mode 0600)
    Init { name: String },
    /// Show this device's address and safety number
    Whoami,
    /// Print a short invite to paste to another agent or human
    Invite,
    /// Manage trusted contacts and their rules
    Contact {
        #[command(subcommand)]
        cmd: ContactCmd,
    },
    /// List pending contact requests (hellos from unknown agents)
    Requests,
    /// Send our address to a contact so they can add us
    Hello { contact: String },
    /// Ask a contact and wait for the reply (needs the daemon)
    Ask {
        contact: String,
        text: String,
        #[arg(long)]
        session: Option<String>,
        /// Your local session name (replies that arrive late go to its inbox)
        #[arg(long = "as")]
        as_: Option<String>,
        #[arg(long, default_value_t = 50)]
        timeout: u64,
    },
    /// Read and remove messages waiting for a local agent (needs the daemon)
    Inbox {
        #[arg(long = "as")]
        as_: Option<String>,
        /// Wait up to this many seconds for a message
        #[arg(long, default_value_t = 0)]
        wait: u64,
        /// Print a plain-text block for a harness hook (nothing when empty)
        #[arg(long)]
        hook: bool,
    },
    /// Answer an ask from the inbox by its id (needs the daemon)
    Reply { id: String, text: String },
    /// Show daemon status
    Status,
    /// MCP server over stdio for Claude Code, Codex, Cursor, Gemini CLI, OpenCode
    Mcp {
        /// This agent's session name; messages for it land in its inbox
        #[arg(long = "as", default_value = "planner")]
        as_: String,
    },
    /// Send a sealed message
    Send {
        contact: String,
        text: String,
        /// Remote local session to deliver to (e.g. planner)
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value = "send")]
        kind: String,
    },
    /// Fetch waiting messages once over plain HTTPS, print them as JSON lines, and ack them
    Check,
    /// Stay connected and print verified messages as JSON lines
    Listen,
    /// Bridge the local pi-intercom broker and the relay (run as a service)
    Daemon {
        /// Local session that receives messages with no explicit session
        #[arg(long, default_value = "planner")]
        target: String,
    },
    /// Install, remove or show the background service (launchd on macOS, systemd --user on Linux)
    Service {
        #[arg(value_parser = ["install", "uninstall", "status"])]
        action: String,
        #[arg(long, default_value = "planner")]
        target: String,
    },
}

#[derive(Subcommand)]
enum ContactCmd {
    /// Trust an agent. Compare its safety number with its human first.
    Add {
        name: String,
        address: String,
        #[arg(long, default_value = "send,ask,reply")]
        allow: String,
        /// Default local session for its messages
        #[arg(long)]
        session: Option<String>,
    },
    /// Accept a pending contact request by id
    Accept {
        request: String,
        name: String,
        #[arg(long, default_value = "send,ask,reply")]
        allow: String,
    },
    Ls,
    Rm { name: String },
}

/// Seal `body` for `contact` and deliver it. Returns the message id.
pub async fn compose_and_send(me: &Identity, contact: &str, kind: &str, body: &str, reply_to: Option<String>, session: Option<String>) -> Result<String> {
    let c = store::contacts().get(contact).cloned().with_context(|| format!("no contact {contact}"))?;
    let to = Peer::parse(&c.address)?;
    let inner = Inner {
        v: 1,
        from: me.peer().address(false),
        to: to.id(),
        ts: now_ms(),
        id: uuid::Uuid::new_v4().to_string(),
        kind: kind.into(),
        reply_to,
        session,
        body: body.into(),
    };
    relay::deliver(&to, seal(me, &to, &inner)?).await?;
    Ok(inner.id)
}

fn parse_allow(s: &str) -> Result<Vec<String>> {
    let v: Vec<String> = s.split(',').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect();
    if let Some(bad) = v.iter().find(|k| !KINDS[..3].contains(&k.as_str())) {
        bail!("--allow takes send,ask,reply (got {bad})");
    }
    Ok(v)
}

fn add_contact(name: &str, address: &str, allow: &str, session: Option<String>) -> Result<Peer> {
    let p = Peer::parse(address)?;
    if p.drop.is_none() {
        bail!("this address has no drop secret (4th part); ask for the full address");
    }
    let mut all = store::contacts();
    all.insert(name.into(), Contact { address: address.trim().into(), allow: parse_allow(allow)?, session, added: now_ms() });
    store::write("contacts.json", &all)?;
    Ok(p)
}

fn print_msg(me: &Identity, env: &[u8], seen: &mut Seen) {
    match open(me, env) {
        Ok((inner, sender)) => {
            if !seen.insert(&inner.id, inner.ts) {
                return eprintln!("dropped replay {}", inner.id);
            }
            let _ = store::write("seen.json", seen);
            match store::authorize(&sender, &inner.kind) {
                Ok((name, _)) => println!(
                    "{}",
                    serde_json::json!({ "from": name, "kind": inner.kind, "id": inner.id, "reply_to": inner.reply_to, "session": inner.session, "ts": inner.ts, "body": inner.body })
                ),
                Err(r) if inner.kind == "hello" && r.starts_with("unknown sender") => {
                    let (rid, req) = store::record_request(&sender, &inner.body, &inner.id, inner.ts);
                    eprintln!("contact request {rid} from \"{}\" ({}); accept with your human's approval: letagents contact accept {rid} <name>", req.name, req.safety);
                }
                Err(r) => eprintln!("REJECTED {} {}: {r}", inner.kind, inner.id),
            }
        }
        Err(e) => eprintln!("dropped envelope: {e}"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Init { name } => {
            store::create_identity(&name)?;
            whoami()
        }
        Cmd::Whoami => whoami(),
        Cmd::Invite => {
            let me = store::identity()?;
            println!(
                "letagents invite from \"{}\"\naddress: {}\nsafety number: {}\nAgent setup: read https://letagents.talk/agents.md, then run\n  letagents contact add {} {} && letagents hello {}",
                me.name,
                me.peer().address(true),
                me.peer().safety_number(),
                me.name,
                me.peer().address(true),
                me.name
            );
            Ok(())
        }
        Cmd::Contact { cmd } => match cmd {
            ContactCmd::Add { name, address, allow, session } => {
                let p = add_contact(&name, &address, &allow, session)?;
                println!("added {name}; safety number {} (confirm it with their human over a channel you trust)", p.safety_number());
                Ok(())
            }
            ContactCmd::Accept { request, name, allow } => {
                let mut reqs = store::requests();
                let r = reqs.remove(&request).with_context(|| format!("no request {request}; see `letagents requests`"))?;
                bail_if_no_drop(&r.address)?;
                add_contact(&name, &r.address, &allow, None)?;
                store::write("requests.json", &reqs)?;
                println!("accepted {name} ({}); restart the daemon to load it", r.safety);
                Ok(())
            }
            ContactCmd::Ls => {
                for (n, c) in store::contacts() {
                    let p = Peer::parse(&c.address)?;
                    println!("{n:<16} {}  allow={}{}", p.safety_number(), c.allow.join(","), c.session.map(|s| format!(" session={s}")).unwrap_or_default());
                }
                Ok(())
            }
            ContactCmd::Rm { name } => {
                let mut all = store::contacts();
                all.remove(&name);
                store::write("contacts.json", &all)?;
                println!("removed {name}");
                Ok(())
            }
        },
        Cmd::Requests => {
            for (id, r) in store::requests() {
                println!("{id}  \"{}\"  {}  {}", r.name, r.safety, r.address);
            }
            Ok(())
        }
        Cmd::Hello { contact } => {
            let me = store::identity()?;
            // A hello carries our full address (with drop) so the other side can write back.
            let to = Peer::parse(&store::contacts().get(&contact).with_context(|| format!("no contact {contact}"))?.address)?;
            let inner = Inner { v: 1, from: me.peer().address(true), to: to.id(), ts: now_ms(), id: uuid::Uuid::new_v4().to_string(), kind: "hello".into(), reply_to: None, session: None, body: me.name.clone() };
            relay::deliver(&to, seal(&me, &to, &inner)?).await?;
            println!("hello sent to {contact}; they must accept it before your messages reach their agents");
            Ok(())
        }
        Cmd::Send { contact, text, session, kind } => {
            if !KINDS[..3].contains(&kind.as_str()) {
                bail!("--kind takes send,ask,reply");
            }
            let id = compose_and_send(&store::identity()?, &contact, &kind, &text, None, session).await?;
            println!("sent {id}");
            Ok(())
        }
        Cmd::Check => {
            let me = store::identity()?;
            let mut seen: Seen = store::read("seen.json");
            let msgs = relay::fetch(&me).await?;
            for m in &msgs {
                print_msg(&me, &m.bytes()?, &mut seen);
            }
            relay::ack(&me, &msgs.iter().map(|m| m.seq).collect::<Vec<_>>()).await
        }
        Cmd::Listen => {
            let me = store::identity()?;
            let mut seen: Seen = store::read("seen.json");
            let me2 = store::identity()?;
            relay::live(&me2, |env| {
                print_msg(&me, &env, &mut seen);
                async {}
            })
            .await
        }
        Cmd::Daemon { target } => daemon::run(store::identity()?, target).await,
        Cmd::Mcp { as_ } => mcp::run(as_).await,
        Cmd::Status => {
            let v = daemon::call(serde_json::json!({ "op": "status" })).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        Cmd::Ask { contact, text, session, as_, timeout } => {
            let v = daemon::call(serde_json::json!({ "op": "ask", "to": contact, "text": text, "session": session, "as": as_, "timeout_ms": timeout * 1000 })).await?;
            match v["reply"]["text"].as_str() {
                Some(t) => println!("{t}"),
                None => {
                    eprintln!("no reply within {timeout}s; it will arrive in the inbox (msg {})", v["msg_id"].as_str().unwrap_or_default());
                    std::process::exit(2);
                }
            }
            Ok(())
        }
        Cmd::Inbox { as_, wait, hook } => {
            let v = daemon::call(serde_json::json!({ "op": "inbox", "as": as_, "wait_ms": wait * 1000 })).await?;
            let msgs = v["messages"].as_array().cloned().unwrap_or_default();
            if hook {
                for m in &msgs {
                    let s = |k: &str| m[k].as_str().unwrap_or_default().to_string();
                    let how = if s("kind") == "ask" { format!("ASK, answer with: letagents reply {} \"<text>\"", s("id")) } else { s("kind") };
                    println!("[letagents · verified {}] {how}\n{}\n", s("from"), s("text"));
                }
            } else {
                for m in &msgs {
                    println!("{m}");
                }
            }
            Ok(())
        }
        Cmd::Reply { id, text } => {
            let v = daemon::call(serde_json::json!({ "op": "reply", "msg": id, "text": text })).await?;
            println!("sent {}", v["msg_id"].as_str().unwrap_or_default());
            Ok(())
        }
        Cmd::Service { action, target } => service::run(&action, &target),
    }
}

fn bail_if_no_drop(addr: &str) -> Result<()> {
    if Peer::parse(addr)?.drop.is_none() {
        bail!("request carries no drop secret");
    }
    Ok(())
}

fn whoami() -> Result<()> {
    let me = store::identity()?;
    println!(
        "name:          {}\naddress:       {}\nsafety number: {}\nrelay:         {}",
        me.name,
        me.peer().address(true),
        me.peer().safety_number(),
        relay::relay_url()
    );
    Ok(())
}
