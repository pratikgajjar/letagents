//! letagents: end-to-end sealed messages between AI agents across devices. The relay stays clueless.

mod codex;
mod crypto;
mod daemon;
mod intercom;
mod mcp;
mod relay;
mod service;
mod store;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use crypto::{now_ms, open, seal, Identity, Inner, Peer, RULE_KINDS};
use serde_json::json;
use store::{AgentRule, Contact, Seen};

#[derive(Parser)]
#[command(name = "letagents", version, about = "End-to-end sealed messages between AI agents. Agents: read https://letagents.talk/agents.md")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create this machine's keys (~/.letagents/identity.json, mode 0600)
    Init { name: String },
    /// Show this machine's address and safety number
    Whoami,
    /// Print an invite with a one-time code (valid 7 days): whoever uses it is added as a contact at once.
    /// Creates this machine's keys and starts the daemon first if needed.
    Invite {
        /// This machine's name if it has no keys yet (default: host name)
        #[arg(long)]
        me: Option<String>,
        /// Do not install the background service
        #[arg(long)]
        no_service: bool,
    },
    /// Join with an invite: create keys if needed, add the inviter, send hello with its code, start the daemon
    Join {
        /// Name for the inviting machine
        name: String,
        invite: String,
        /// This machine's name if it has no keys yet (default: host name)
        #[arg(long)]
        me: Option<String>,
        /// Do not install the background service
        #[arg(long)]
        no_service: bool,
    },
    /// Manage trusted contacts (machines)
    Contact {
        #[command(subcommand)]
        cmd: ContactCmd,
    },
    /// Per-agent rules for a contact
    Rule {
        #[command(subcommand)]
        cmd: RuleCmd,
    },
    /// Named groups of agents (fan-out sends)
    Group {
        #[command(subcommand)]
        cmd: GroupCmd,
    },
    /// List pending contact requests
    Requests,
    /// Send our address to a contact so they can add us
    Hello { contact: String },
    /// List every agent you can reach: local agents and agents on each contact's machine (needs the daemon)
    List {
        /// Only this contact
        contact: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Send a message to <machine>/<agent> or @<group>
    Send {
        to: String,
        text: String,
        /// Your local agent name
        #[arg(long = "as")]
        as_: Option<String>,
    },
    /// Ask and wait for the reply: <machine>/<agent> or @<group>
    Ask {
        to: String,
        text: String,
        #[arg(long = "as")]
        as_: Option<String>,
        #[arg(long, default_value_t = 50)]
        timeout: u64,
    },
    /// Read and remove messages waiting for a local agent
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
    /// Answer an ask from the inbox by its id
    Reply {
        id: String,
        text: String,
        #[arg(long = "as")]
        as_: Option<String>,
    },
    /// Show daemon status
    Status,
    /// Fetch waiting mail once over HTTPS without a daemon; prints JSON lines
    Check,
    /// Run the daemon (normally via `letagents service install`)
    Daemon {
        /// Local agent told about contact requests (they also wait in `letagents inbox`).
        /// Never gets other agents' mail: senders must name an agent.
        #[arg(long, alias = "target")]
        notify: Option<String>,
    },
    /// Install, remove or show the background service (launchd on macOS, systemd --user on Linux)
    Service {
        #[arg(value_parser = ["install", "uninstall", "status"])]
        action: String,
        /// Local agent told about contact requests
        #[arg(long, alias = "target")]
        notify: Option<String>,
    },
    /// MCP server over stdio for Claude Code, Codex, Cursor, Gemini CLI, OpenCode
    Mcp {
        /// Agent name; default derives from the harness session (e.g. claude-6bf79665)
        #[arg(long = "as")]
        as_: Option<String>,
    },
}

#[derive(Subcommand)]
enum ContactCmd {
    /// Trust a machine. Compare its safety number with its human first.
    Add {
        name: String,
        address: String,
        /// What its agents may do: send, ask, reply, list
        #[arg(long, default_value = "send,ask,reply,list")]
        allow: String,
        /// Default local agent for its messages
        #[arg(long)]
        session: Option<String>,
    },
    /// Accept a pending contact request by id
    Accept {
        request: String,
        name: String,
        #[arg(long, default_value = "send,ask,reply,list")]
        allow: String,
    },
    Ls,
    Rm { name: String },
}

#[derive(Subcommand)]
enum RuleCmd {
    /// Allow kinds between their agent (--from) and our agent (--to); `*` and `prefix*` match
    Add {
        contact: String,
        #[arg(long, default_value = "*")]
        from: String,
        #[arg(long, default_value = "*")]
        to: String,
        #[arg(long)]
        allow: String,
    },
    Ls,
    /// Remove rule number N (from `rule ls`). Rules are checked in order; the first match decides.
    Rm { contact: String, n: usize },
}

#[derive(Subcommand)]
enum GroupCmd {
    /// Create or replace a group: members are <machine>/<agent> or <machine>
    Set { name: String, members: Vec<String> },
    Ls,
    Rm { name: String },
}

/// Seal `body` for `contact` and deliver it. Returns the message id.
pub async fn compose_and_send(
    me: &Identity,
    contact: &str,
    kind: &str,
    body: &str,
    reply_to: Option<String>,
    from_agent: Option<String>,
    to_agent: Option<String>,
) -> Result<String> {
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
        from_agent,
        to_agent,
        invite: None,
        body: body.into(),
    };
    relay::deliver(&to, seal(me, &to, &inner)?).await?;
    Ok(inner.id)
}

fn parse_allow(s: &str) -> Result<Vec<String>> {
    let v: Vec<String> = s.split(',').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect();
    if let Some(bad) = v.iter().find(|k| !RULE_KINDS.contains(&k.as_str())) {
        bail!("allow takes {} (got {bad})", RULE_KINDS.join(","));
    }
    Ok(v)
}

fn add_contact(name: &str, address: &str, allow: &str, session: Option<String>) -> Result<Peer> {
    if name.contains('@') || name.contains('/') {
        bail!("contact names cannot contain @ or /");
    }
    let (address, invite) = store::split_invite(address);
    let p = Peer::parse(&address)?;
    if p.drop.is_none() {
        bail!("this address has no drop secret (4th part); ask for the full address");
    }
    let mut all = store::contacts();
    let rules = all.get(name).map(|c| c.rules.clone()).unwrap_or_default();
    all.insert(name.into(), Contact { address, allow: parse_allow(allow)?, session, added: now_ms(), rules, invite });
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
            let from_agent = inner.from_agent.clone().unwrap_or_else(|| "agent".into());
            let to = inner.to_agent.clone().unwrap_or_else(|| "*".into());
            match store::authorize(&sender, &inner.kind, &from_agent, &to) {
                Ok((name, _)) => println!(
                    "{}",
                    json!({ "from": format!("{name}/{from_agent}"), "kind": inner.kind, "id": inner.id, "reply_to": inner.reply_to, "to": inner.to_agent, "ts": inner.ts, "body": inner.body })
                ),
                Err(r) if inner.kind == "hello" && r.starts_with("unknown sender") && inner.invite.as_deref().is_some_and(store::redeem_invite) => match store::accept_invited(&sender, &inner.body) {
                    Ok(name) => eprintln!("added contact {name} ({}): it used your invite code", sender.safety_number()),
                    Err(e) => eprintln!("invite from {}: {e}", sender.safety_number()),
                },
                Err(r) if inner.kind == "hello" && r.starts_with("unknown sender") => {
                    let (rid, req) = store::record_request(&sender, &inner.body, &inner.id, inner.ts);
                    eprintln!("contact request {rid} from \"{}\" ({}); accept with your human's approval: letagents contact accept {rid} <name>", req.name, req.safety);
                }
                Err(r) => eprintln!("refused {} {}: {r}", inner.kind, inner.id),
            }
        }
        Err(e) => eprintln!("dropped envelope: {e}"),
    }
}

fn print_json(v: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Init { name } => {
            store::create_identity(&name)?;
            whoami()
        }
        Cmd::Whoami => whoami(),
        Cmd::Invite { me, no_service } => {
            let me = ensure_identity(me).await?;
            ensure_daemon(no_service).await;
            connect_harnesses(no_service);
            let a = format!("{}.{}", me.peer().address(true), store::new_invite()?);
            println!(
                "letagents invite from \"{n}\" (one use, valid 7 days; give it only to the agent you want)\naddress: {a}\nsafety number: {}\n\nOn the other machine, paste one line:\n  curl -fsSL https://letagents.talk/install.sh | sh -s -- join {n} {a}\nNo install (Node 20+):\n  curl -fsSL https://letagents.talk/la.mjs | node - join {n} {a}",
                me.peer().safety_number(),
                n = me.name
            );
            Ok(())
        }
        Cmd::Join { name, invite, me, no_service } => {
            if store::split_invite(&invite).1.is_none() {
                bail!("this is an address without an invite code; ask for a new invite (letagents invite), or use: letagents contact add {name} <address>");
            }
            let me = ensure_identity(me).await?;
            let p = add_contact(&name, &invite, "send,ask,reply,list", None)?;
            let c = store::contacts().get(&name).cloned().context("contact")?;
            let inner = Inner { v: 1, from: me.peer().address(true), to: p.id(), ts: now_ms(), id: uuid::Uuid::new_v4().to_string(), kind: "hello".into(), reply_to: None, from_agent: None, to_agent: None, invite: c.invite.clone(), body: me.name.clone() };
            relay::deliver(&p, seal(&me, &p, &inner)?).await?;
            ensure_daemon(no_service).await;
            connect_harnesses(no_service);
            println!(
                "joined {name} as \"{}\": it adds you when its daemon reads the hello (code: one use, 7 days).\nsafety number of {name}: {}\nsend:  letagents send {name}/<agent> \"text\"      (who is there: letagents list {name})",
                me.name,
                p.safety_number()
            );
            Ok(())
        }
        Cmd::Contact { cmd } => match cmd {
            ContactCmd::Add { name, address, allow, session } => {
                let p = add_contact(&name, &address, &allow, session)?;
                let _ = daemon::call(json!({ "op": "reload" })).await;
                println!("added {name}; safety number {} (compare it with their human if the address came through a channel you do not trust)", p.safety_number());
                Ok(())
            }
            ContactCmd::Accept { request, name, allow } => {
                let mut reqs = store::requests();
                let r = reqs.remove(&request).with_context(|| format!("no request {request}; see `letagents requests`"))?;
                add_contact(&name, &r.address, &allow, None)?;
                store::write("requests.json", &reqs)?;
                let _ = daemon::call(json!({ "op": "reload" })).await;
                println!("accepted {name} ({})", r.safety);
                Ok(())
            }
            ContactCmd::Ls => {
                for (n, c) in store::contacts() {
                    let p = Peer::parse(&c.address)?;
                    println!("{n:<16} {}  allow={}  rules={}", p.safety_number(), c.allow.join(","), c.rules.len());
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
        Cmd::Rule { cmd } => {
            let mut all = store::contacts();
            match cmd {
                RuleCmd::Add { contact, from, to, allow } => {
                    let c = all.get_mut(&contact).with_context(|| format!("no contact {contact}"))?;
                    c.rules.push(AgentRule { from, to, allow: parse_allow(&allow)? });
                    store::write("contacts.json", &all)?;
                    println!("rule added; rules apply at once");
                }
                RuleCmd::Ls => {
                    for (n, c) in &all {
                        if c.rules.is_empty() {
                            println!("{n}: no rules (contact allow list applies to all agents: {})", c.allow.join(","));
                        }
                        for (i, r) in c.rules.iter().enumerate() {
                            println!("{n} #{i}: {n}/{} -> {} may {}", r.from, r.to, r.allow.join(","));
                        }
                    }
                }
                RuleCmd::Rm { contact, n } => {
                    let c = all.get_mut(&contact).with_context(|| format!("no contact {contact}"))?;
                    if n >= c.rules.len() {
                        bail!("no rule #{n}");
                    }
                    c.rules.remove(n);
                    store::write("contacts.json", &all)?;
                    println!("rule removed");
                }
            }
            Ok(())
        }
        Cmd::Group { cmd } => {
            let mut g = store::groups();
            match cmd {
                GroupCmd::Set { name, members } => {
                    if members.is_empty() {
                        bail!("a group needs members");
                    }
                    g.insert(name.trim_start_matches('@').into(), members);
                    store::write("groups.json", &g)?;
                    println!("group set");
                }
                GroupCmd::Ls => {
                    for (n, m) in &g {
                        println!("@{n}: {}", m.join(" "));
                    }
                }
                GroupCmd::Rm { name } => {
                    g.remove(name.trim_start_matches('@'));
                    store::write("groups.json", &g)?;
                    println!("group removed");
                }
            }
            Ok(())
        }
        Cmd::Requests => {
            for (id, r) in store::requests() {
                println!("{id}  \"{}\"  {}", r.name, r.safety);
            }
            Ok(())
        }
        Cmd::Hello { contact } => {
            let me = store::identity()?;
            // A hello carries our full address (with drop) so the other side can write back, and the invite
            // code we got with their address, if any.
            let c = store::contacts().get(&contact).cloned().with_context(|| format!("no contact {contact}"))?;
            let to = Peer::parse(&c.address)?;
            let inner = Inner { v: 1, from: me.peer().address(true), to: to.id(), ts: now_ms(), id: uuid::Uuid::new_v4().to_string(), kind: "hello".into(), reply_to: None, from_agent: None, to_agent: None, invite: c.invite.clone(), body: me.name.clone() };
            relay::deliver(&to, seal(&me, &to, &inner)?).await?;
            if c.invite.is_some() {
                println!("hello sent to {contact} with its invite code; it adds you at once if the code is unused and less than 7 days old");
            } else {
                println!("hello sent to {contact}; they must accept it before your messages reach their agents");
            }
            Ok(())
        }
        Cmd::List { contact, json: as_json } => {
            let v = daemon::call(json!({ "op": "list", "contact": contact })).await?;
            if as_json {
                return print_json(&v);
            }
            println!("on this machine ({}):", v["me"].as_str().unwrap_or_default());
            for a in v["local"].as_array().cloned().unwrap_or_default() {
                println!("  {:<32} {:<12} {:<10} {}", a["name"].as_str().unwrap_or_default(), a["harness"].as_str().unwrap_or_default(), a["status"].as_str().unwrap_or_default(), a["where"].as_str().unwrap_or_default());
            }
            println!("reachable:");
            for a in v["remote"].as_array().cloned().unwrap_or_default() {
                println!("  {:<32} {:<12} {:<10} {}", a["address"].as_str().unwrap_or_default(), a["harness"].as_str().unwrap_or_default(), a["status"].as_str().unwrap_or_default(), a["where"].as_str().unwrap_or_default());
            }
            for g in v["groups"].as_array().cloned().unwrap_or_default() {
                println!("  {}", g.as_str().unwrap_or_default());
            }
            Ok(())
        }
        Cmd::Send { to, text, as_ } => {
            let v = daemon::call(json!({ "op": "send", "to": to, "text": text, "as": as_.or_else(mcp::session_name) })).await?;
            for s in v["sent"].as_array().cloned().unwrap_or_default() {
                println!("{} -> {} {}", s["to"].as_str().unwrap_or_default(), s["receipt"]["status"].as_str().unwrap_or_default(), s["receipt"]["detail"].as_str().unwrap_or_default());
            }
            Ok(())
        }
        Cmd::Ask { to, text, as_, timeout } => {
            let v = daemon::call(json!({ "op": "ask", "to": to, "text": text, "as": as_.or_else(mcp::session_name), "timeout_ms": timeout * 1000 })).await?;
            let replies = v["replies"].as_array().cloned().unwrap_or_default();
            let mut missing = false;
            for r in &replies {
                match r["reply"]["text"].as_str() {
                    Some(t) if replies.len() == 1 => println!("{t}"),
                    Some(t) => println!("[{}] {t}", r["from"].as_str().unwrap_or_default()),
                    None => {
                        missing = true;
                        eprintln!("{}: no reply within {timeout}s; it will arrive in the inbox", r["from"].as_str().unwrap_or_default());
                    }
                }
            }
            if missing {
                std::process::exit(2);
            }
            Ok(())
        }
        Cmd::Inbox { as_, wait, hook } => {
            let v = daemon::call(json!({ "op": "inbox", "as": as_.or_else(mcp::session_name), "wait_ms": wait * 1000 })).await?;
            for m in v["messages"].as_array().cloned().unwrap_or_default() {
                if hook {
                    let s = |k: &str| m[k].as_str().unwrap_or_default().to_string();
                    let how = if s("kind") == "ask" { format!(" ASK {}", s("id")) } else { String::new() };
                    println!("[letagents · {}]{how}\n{}\n", s("from"), s("text"));
                } else {
                    println!("{m}");
                }
            }
            Ok(())
        }
        Cmd::Reply { id, text, as_ } => {
            let v = daemon::call(json!({ "op": "reply", "msg": id, "text": text, "as": as_.or_else(mcp::session_name) })).await?;
            println!("sent {}", v["msg_id"].as_str().unwrap_or_default());
            Ok(())
        }
        Cmd::Status => print_json(&daemon::call(json!({ "op": "status" })).await?),
        Cmd::Check => {
            let me = store::identity()?;
            let mut seen: Seen = store::read("seen.json");
            let msgs = relay::fetch(&me).await?;
            for m in &msgs {
                print_msg(&me, &m.bytes()?, &mut seen);
            }
            relay::ack(&me, &msgs.iter().map(|m| m.seq).collect::<Vec<_>>()).await
        }
        Cmd::Daemon { notify } => daemon::run(store::identity()?, notify).await,
        Cmd::Service { action, notify } => service::run(&action, notify.as_deref()),
        Cmd::Mcp { as_ } => mcp::run(as_).await,
    }
}

/// This machine's keys, created on first use (name: --me, else the host name). A new mailbox is opened
/// at once, so contacts can write before the daemon first runs.
async fn ensure_identity(name: Option<String>) -> Result<Identity> {
    if let Ok(me) = store::identity() {
        return Ok(me);
    }
    let name = name.unwrap_or_else(host_name);
    let me = store::create_identity(&name)?;
    if let Err(e) = relay::fetch(&me).await {
        eprintln!("warning: relay not reached ({e}); the daemon opens the mailbox when it starts");
    }
    eprintln!("created keys for \"{name}\" in {} (they never leave this machine)", store::home().display());
    Ok(me)
}

/// Short host name: lower case letters, digits and '-'.
fn host_name() -> String {
    let h = std::process::Command::new("hostname").arg("-s").output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    let h: String = h.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let h: String = h.trim_matches('-').chars().take(24).collect();
    if h.is_empty() { "machine".into() } else { h }
}

/// Start the background service unless it runs (or --no-service, or a custom LETAGENTS_HOME: the service
/// uses ~/.letagents). If it runs, it loads new contacts.
async fn ensure_daemon(no_service: bool) {
    if daemon::call(json!({ "op": "reload" })).await.is_ok() {
        return;
    }
    if no_service || std::env::var_os("LETAGENTS_HOME").is_some() || !(cfg!(target_os = "macos") || cfg!(target_os = "linux")) {
        return eprintln!("no daemon running: start it with `letagents service install` (or `letagents daemon`)");
    }
    if let Err(e) = service::run("install", None) {
        eprintln!("could not install the service ({e}); run: letagents daemon");
    }
}

/// Register `letagents mcp` with Claude Code and Codex when they are installed and do not have it yet,
/// so their sessions can send and get messages with no more setup. pi needs nothing.
fn connect_harnesses(skip: bool) {
    if skip || std::env::var_os("LETAGENTS_HOME").is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let exe = exe.to_string_lossy().to_string();
    let run = |cmd: &str, args: &[&str]| std::process::Command::new(cmd).args(args).stdin(std::process::Stdio::null()).output().map(|o| o.status.success()).unwrap_or(false);
    for (cli, label, add) in [
        ("claude", "Claude Code", vec!["mcp", "add", "--scope", "user", "letagents", "--", exe.as_str(), "mcp"]),
        ("codex", "Codex", vec!["mcp", "add", "letagents", "--", exe.as_str(), "mcp"]),
    ] {
        if !run(cli, &["--version"]) || run(cli, &["mcp", "get", "letagents"]) {
            continue;
        }
        if run(cli, &add) {
            eprintln!("{label}: added the letagents MCP server (new sessions can send and get messages)");
        } else {
            eprintln!("{label}: could not add the MCP server; run: {cli} mcp add letagents -- letagents mcp");
        }
    }
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
