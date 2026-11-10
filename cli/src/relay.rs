//! Talking to the relay over HTTPS (send, one-shot fetch, ack) and WSS (live inbox).

use crate::crypto::{b64, now_ms, Identity, Peer};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::Message;

use std::{path::Path, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

/// Add explicit PEM bundles in addition to built-in and platform trust roots.
/// Returns how many certificates were added.
fn add_pem(roots: &mut rustls::RootCertStore, path: &Path) -> Result<usize> {
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut count = 0;
    for cert in rustls_pemfile::certs(&mut reader) {
        roots.add(cert?).with_context(|| format!("invalid CA in {}", path.display()))?;
        count += 1;
    }
    Ok(count)
}

fn add_extra_roots(roots: &mut rustls::RootCertStore, file: Option<&std::ffi::OsStr>, dir: Option<&std::ffi::OsStr>) -> Result<()> {
    if let Some(file) = file {
        // A named bundle that parses to zero certificates is a misconfiguration, not an empty
        // bundle: fail loudly instead of silently trusting nothing extra.
        let path = Path::new(file);
        if add_pem(roots, path).context("loading SSL_CERT_FILE")? == 0 {
            bail!("no PEM certificates found in {}", path.display());
        }
    }
    if let Some(dirs) = dir {
        for dir in std::env::split_paths(dirs) {
            for entry in std::fs::read_dir(&dir).context("reading SSL_CERT_DIR")? {
                let path = entry?.path();
                if path.is_file() {
                    // Directories may contain non-certificate files (zero certs: skip silently);
                    // a malformed certificate block still errors.
                    let _ = add_pem(roots, &path).with_context(|| format!("loading {}", path.display()))?;
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn tls_config() -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let native = rustls_native_certs::load_native_certs();
    // Some platforms have no native store. Built-in roots remain available.
    for cert in native.certs {
        let _ = roots.add(cert);
    }
    add_extra_roots(&mut roots, std::env::var_os("SSL_CERT_FILE").as_deref(), std::env::var_os("SSL_CERT_DIR").as_deref())?;
    Ok(rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth())
}

fn http_client() -> Result<reqwest::Client> {
    // Snapshot and validate before installing the infallible reqwest callback. Use the
    // exact WSS policy, including HTTP_PROXY fallback and both NO_PROXY spellings.
    let env: std::collections::HashMap<String, String> = [
        "HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy",
        "NO_PROXY", "no_proxy",
    ].into_iter().filter_map(|key| std::env::var(key).ok().map(|v| (key.to_owned(), v))).collect();
    for (key, value) in &env {
        if !key.eq_ignore_ascii_case("no_proxy") && !value.trim().is_empty() {
            parse_proxy(value)?;
        }
    }
    let proxy = reqwest::Proxy::custom(move |url| {
        proxy_for(url, |key| env.get(key).cloned()).expect("proxy URLs already validated")
    });
    Ok(reqwest::Client::builder().no_proxy().proxy(proxy).use_preconfigured_tls(tls_config()?)
        .timeout(Duration::from_secs(30)).build()?)
}

fn poll_interval(value: Option<&str>) -> Duration {
    Duration::from_secs(value.and_then(|s| s.parse::<u64>().ok()).filter(|s| *s > 0).unwrap_or(10))
}

fn bypass_proxy(url: &reqwest::Url, no_proxy: &str) -> bool {
    let host = url.host_str().unwrap_or("").trim_matches(['[', ']']).to_ascii_lowercase();
    no_proxy.split(',').map(str::trim).filter(|s| !s.is_empty()).any(|entry| {
        if entry == "*" { return true; }
        let entry = entry.to_ascii_lowercase();
        let (name, port) = if entry.starts_with('[') {
            match entry.find(']') {
                Some(end) => (&entry[1..end], entry[end + 1..].strip_prefix(':')),
                None => (entry.as_str(), None),
            }
        } else if entry.matches(':').count() == 1 {
            let (name, port) = entry.rsplit_once(':').unwrap();
            (name, Some(port))
        } else { (entry.as_str(), None) };
        if port.is_some_and(|p| p.parse::<u16>().ok() != url.port_or_known_default()) { return false; }
        let name = name.trim_start_matches('.');
        host == name || host.ends_with(&format!(".{name}"))
    })
}

/// Injectable environment lookup keeps parsing tests isolated from process-global state.
fn proxy_for(url: &reqwest::Url, env: impl Fn(&str) -> Option<String>) -> Result<Option<reqwest::Url>> {
    if ["NO_PROXY", "no_proxy"].iter().filter_map(|key| env(key)).any(|v| bypass_proxy(url, &v)) {
        return Ok(None);
    }
    let keys: &[&str] = if matches!(url.scheme(), "wss" | "https") {
        &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy"]
    } else { &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] };
    let Some(value) = keys.iter().filter_map(|key| env(key)).find(|v| !v.trim().is_empty()) else { return Ok(None); };
    Ok(Some(parse_proxy(&value)?))
}

fn parse_proxy(value: &str) -> Result<reqwest::Url> {
    let proxy = reqwest::Url::parse(value.trim()).context("invalid proxy URL")?;
    if !matches!(proxy.scheme(), "http" | "socks5" | "socks5h") || proxy.host_str().is_none() {
        bail!("unsupported proxy URL (expected http, socks5 or socks5h)");
    }
    Ok(proxy)
}

async fn proxy_tunnel(proxy: &reqwest::Url, target: &reqwest::Url) -> Result<TcpStream> {
    let proxy_host = proxy.host_str().context("proxy has no host")?.trim_matches(['[', ']']);
    let mut stream = TcpStream::connect((proxy_host, proxy.port().unwrap_or(if proxy.scheme() == "http" { 80 } else { 1080 }))).await?;
    let host = target.host_str().context("relay has no host")?.trim_matches(['[', ']']);
    let port = target.port_or_known_default().context("relay has no port")?;
    if proxy.scheme() == "http" {
        let authority = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
        let auth = if !proxy.username().is_empty() {
            let (user, pass) = proxy_credentials(proxy)?;
            format!("Proxy-Authorization: Basic {}\r\n", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}")))
        } else { String::new() };
        stream.write_all(format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n{auth}\r\n").as_bytes()).await?;
        // Read exactly through the header terminator; leave tunnel bytes untouched.
        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            if response.len() >= 16384 { bail!("proxy CONNECT header too large"); }
            response.push(stream.read_u8().await?);
        }
        let status = std::str::from_utf8(&response)?.lines().next().unwrap_or("").split_whitespace().nth(1).unwrap_or("");
        if status != "200" { bail!("proxy CONNECT refused: {status}"); }
    } else {
        let credentials = !proxy.username().is_empty();
        stream.write_all(if credentials { &[5, 2, 0, 2] } else { &[5, 1, 0] }).await?;
        let mut choice = [0; 2];
        stream.read_exact(&mut choice).await?;
        if choice[0] != 5 { bail!("invalid SOCKS version"); }
        match choice[1] {
            0 => {},
            2 if credentials => {
                let (user, pass) = proxy_credentials(proxy)?;
                if user.len() > 255 || pass.len() > 255 { bail!("SOCKS credentials too long"); }
                let mut auth = vec![1, user.len() as u8];
                auth.extend(user.as_bytes()); auth.push(pass.len() as u8); auth.extend(pass.as_bytes());
                stream.write_all(&auth).await?;
                stream.read_exact(&mut choice).await?;
                if choice != [1, 0] { bail!("SOCKS authentication refused"); }
            },
            _ => bail!("SOCKS proxy has no supported authentication method"),
        }
        let mut request = vec![5, 1, 0];
        let ip = if let Ok(ip) = host.parse::<std::net::IpAddr>() { Some(ip) }
            else if proxy.scheme() == "socks5" { Some(tokio::net::lookup_host((host, port)).await?.next().context("relay DNS returned no addresses")?.ip()) }
            else { None };
        match ip {
            Some(std::net::IpAddr::V4(ip)) => { request.push(1); request.extend(ip.octets()); },
            Some(std::net::IpAddr::V6(ip)) => { request.push(4); request.extend(ip.octets()); },
            None => { if host.len() > 255 { bail!("SOCKS hostname too long"); } request.extend([3, host.len() as u8]); request.extend(host.as_bytes()); },
        }
        request.extend(port.to_be_bytes());
        stream.write_all(&request).await?;
        let mut reply = [0; 4]; stream.read_exact(&mut reply).await?;
        if reply[0] != 5 || reply[1] != 0 || reply[2] != 0 { bail!("SOCKS CONNECT refused (code {})", reply[1]); }
        let size = match reply[3] { 1 => 4, 4 => 16, 3 => stream.read_u8().await? as usize, _ => bail!("invalid SOCKS address type") };
        let mut bound = vec![0; size + 2]; stream.read_exact(&mut bound).await?;
    }
    Ok(stream)
}

fn proxy_credentials(proxy: &reqwest::Url) -> Result<(String, String)> {
    // URL form decoding provides percent decoding without treating a literal '+' as space.
    fn decode(s: &str) -> Result<String> {
        Ok(reqwest::Url::parse(&format!("http://localhost/?v={}", s.replace('+', "%2B")))?
            .query_pairs().next().context("invalid proxy credentials")?.1.into_owned())
    }
    Ok((decode(proxy.username())?, decode(proxy.password().unwrap_or(""))?))
}

async fn connect_ws(url: &str) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let target = reqwest::Url::parse(url)?;
    let proxy = proxy_for(&target, |key| std::env::var(key).ok())?;
    let connector = Some(Connector::Rustls(Arc::new(tls_config()?)));
    let (ws, _) = if let Some(proxy) = proxy {
        let stream = proxy_tunnel(&proxy, &target).await?;
        tokio_tungstenite::client_async_tls_with_config(url, stream, None, connector).await?
    } else {
        tokio_tungstenite::connect_async_tls_with_config(url, None, false, connector).await?
    };
    Ok(ws)
}

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
    let r = http_client()?
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
    let r = http_client()?.get(format!("{}/v1/inbox/{}?{}", relay_url(), me.id(), inbox_query(me))).send().await?;
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
    let r = http_client()?
        .post(format!("{}/v1/ack/{}?ts={ts}&seqs={list}&sig={sig}", relay_url(), me.id()))
        .send()
        .await?;
    if !r.status().is_success() {
        bail!("ack refused: {}", r.status());
    }
    Ok(())
}

/// While polling over HTTPS, an empty inbox doubles the wait up to this.
const POLL_MAX: Duration = Duration::from_secs(120);
/// While polling over HTTPS, WSS is tried again this often.
const WSS_RETRY: Duration = Duration::from_secs(300);

/// The next poll wait: back to `base` when mail came, else doubled up to `POLL_MAX`.
fn next_poll(cur: Duration, base: Duration, got_mail: bool) -> Duration {
    if got_mail {
        base
    } else {
        (cur * 2).clamp(base, POLL_MAX.max(base))
    }
}

/// Live inbox over WSS. Calls `on_msg` for each stored or new envelope and acks it afterwards.
/// A connected WSS costs the relay nothing while idle (hibernation). When WSS is unavailable, it fetches
/// over HTTPS with backoff (an empty inbox doubles the wait, up to 2 minutes) and tries WSS every 5 minutes.
pub async fn live<F, Fut>(me: &Identity, mut on_msg: F) -> !
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let poll = poll_interval(std::env::var("LETAGENTS_POLL_SECS").ok().as_deref());
    let mut wait = poll;
    let mut wss_at = tokio::time::Instant::now();
    loop {
        if tokio::time::Instant::now() < wss_at {
            // Polling mode: WSS failed recently.
            let got = poll_once(me, &mut on_msg).await;
            wait = next_poll(wait, poll, got);
            tokio::time::sleep(wait).await;
            continue;
        }
        let started = tokio::time::Instant::now();
        let url = format!("{}/v1/inbox/{}?{}", relay_url().replacen("http", "ws", 1), me.id(), inbox_query(me));
        match tokio::time::timeout(Duration::from_secs(30), connect_ws(&url)).await {
            Ok(Ok(ws)) => {
                eprintln!("inbox: connected (wss)");
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
                                ("ready", _, _) => {},
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
                // A connection that lived a while reconnects at once (stored mail comes on connect);
                // one that dropped quickly waits, so a flapping link cannot hammer the relay.
                tokio::time::sleep(if started.elapsed() > Duration::from_secs(60) { Duration::from_secs(1) } else { poll }).await;
                continue;
            }
            Ok(Err(e)) => eprintln!("inbox: wss connect failed: {e}"),
            Err(_) => eprintln!("inbox: wss connect timed out"),
        }
        eprintln!("inbox: polling via https (wss again in {} s)", WSS_RETRY.as_secs());
        wss_at = tokio::time::Instant::now() + WSS_RETRY;
        wait = poll;
        poll_once(me, &mut on_msg).await;
        tokio::time::sleep(wait).await;
    }
}

/// One HTTPS fetch + ack. True when mail came.
async fn poll_once<F, Fut>(me: &Identity, on_msg: &mut F) -> bool
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    match fetch(me).await {
        Ok(messages) => {
            let mut seqs = Vec::new();
            for message in messages {
                if let Ok(bytes) = message.bytes() {
                    on_msg(bytes).await;
                    seqs.push(message.seq);
                }
            }
            if let Err(e) = ack(me, &seqs).await {
                eprintln!("inbox: https ack failed: {e}");
            }
            !seqs.is_empty()
        }
        Err(e) => {
            eprintln!("inbox: https fetch failed: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Env-var tests share process-global state; serialize them.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).unwrap()
    }

    fn env_map<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string())
    }

    #[test]
    fn proxy_for_prefers_https_proxy_for_wss() {
        let u = url("wss://letagents.talk/v1/inbox/x");
        let p = proxy_for(&u, env_map(&[("HTTPS_PROXY", "http://proxy:8080"), ("ALL_PROXY", "socks5://other:1080")])).unwrap().unwrap();
        assert_eq!(p.as_str(), "http://proxy:8080/");
        // Lower-case keys work too.
        let p = proxy_for(&u, env_map(&[("https_proxy", "http://proxy:8080")])).unwrap().unwrap();
        assert_eq!(p.as_str(), "http://proxy:8080/");
    }

    #[test]
    fn proxy_for_falls_back_to_all_proxy_and_accepts_socks() {
        let u = url("wss://letagents.talk/v1/inbox/x");
        let p = proxy_for(&u, env_map(&[("ALL_PROXY", "socks5h://127.0.0.1:11080")])).unwrap().unwrap();
        assert_eq!(p.scheme(), "socks5h");
        let p = proxy_for(&u, env_map(&[("all_proxy", "socks5://127.0.0.1:1080")])).unwrap().unwrap();
        assert_eq!(p.scheme(), "socks5");
        // No proxy env at all: direct.
        assert!(proxy_for(&u, env_map(&[])).unwrap().is_none());
    }

    #[test]
    fn proxy_for_rejects_unsupported_schemes() {
        let u = url("wss://letagents.talk/v1/inbox/x");
        assert!(proxy_for(&u, env_map(&[("HTTPS_PROXY", "ftp://proxy:21")])).is_err());
        assert!(proxy_for(&u, env_map(&[("HTTPS_PROXY", "not a url")])).is_err());
    }

    #[test]
    fn https_and_wss_share_proxy_policy() {
        for key in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy"] {
            for bypass in ["", "letagents.talk", "other.example"] {
                let pairs = [(key, "http://proxy:8080"), ("NO_PROXY", bypass)];
                let https = proxy_for(&url("https://letagents.talk/inbox"), env_map(&pairs)).unwrap();
                let wss = proxy_for(&url("wss://letagents.talk/inbox"), env_map(&pairs)).unwrap();
                assert_eq!(https, wss, "{key}, NO_PROXY={bypass}");
                assert_eq!(https.is_none(), bypass == "letagents.talk");
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires loopback socket permission; run with --ignored outside the sandbox"]
    async fn http_connect_leaves_target_dns_to_proxy() {
        // .invalid must never be resolved locally when using HTTP CONNECT.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = url(&format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
                assert!(request.len() < 16384);
            }
            assert!(request.starts_with(b"CONNECT relay.invalid:443 HTTP/1.1\r\n"));
            stream.write_all(b"HTTP/1.1 200 Connection established\r\n\r\ntunnel").await.unwrap();
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut stream = proxy_tunnel(&proxy, &url("wss://relay.invalid/inbox")).await.unwrap();
            let mut bytes = [0; 6];
            stream.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"tunnel");
            server.await.unwrap();
        }).await.unwrap();
    }

    #[test]
    fn no_proxy_bypasses_localhost_and_matching_hosts() {
        let u = url("wss://letagents.talk/v1/inbox/x");
        // localhost is bypassed even with a proxy set.
        let local = url("ws://localhost:8787/v1/inbox/x");
        assert!(proxy_for(&local, env_map(&[("ALL_PROXY", "http://proxy:8080"), ("NO_PROXY", "localhost,127.0.0.1")])).unwrap().is_none());
        assert!(proxy_for(&local, env_map(&[("ALL_PROXY", "http://proxy:8080"), ("no_proxy", "localhost")])).unwrap().is_none());
        // A matching NO_PROXY entry bypasses the relay host too.
        assert!(proxy_for(&u, env_map(&[("HTTPS_PROXY", "http://proxy:8080"), ("NO_PROXY", "letagents.talk")])).unwrap().is_none());
        // A non-matching entry does not bypass.
        assert!(proxy_for(&u, env_map(&[("HTTPS_PROXY", "http://proxy:8080"), ("NO_PROXY", "example.com")])).unwrap().is_some());
    }

    #[test]
    fn bypass_proxy_matches_hosts_subdomains_and_ports() {
        let u = url("https://example.com:443/x");
        assert!(bypass_proxy(&u, "example.com"));
        assert!(bypass_proxy(&u, ".example.com"));
        assert!(bypass_proxy(&u, "example.com, other.org"));
        // A subdomain entry matches that subdomain, not its parent.
        assert!(!bypass_proxy(&u, "sub.example.com"));
        assert!(bypass_proxy(&url("https://sub.example.com/x"), "sub.example.com"));
        assert!(!bypass_proxy(&u, "badexample.com"));
        assert!(!bypass_proxy(&u, "example.org"));
        assert!(bypass_proxy(&u, "*"));
        // Port-qualified entries only match that port.
        assert!(bypass_proxy(&u, "example.com:443"));
        assert!(!bypass_proxy(&u, "example.com:8443"));
    }

    #[test]
    fn poll_backs_off_when_empty_and_resets_on_mail() {
        let base = Duration::from_secs(10);
        let mut w = base;
        for want in [20, 40, 80, 120, 120] {
            w = next_poll(w, base, false);
            assert_eq!(w, Duration::from_secs(want));
        }
        assert_eq!(next_poll(w, base, true), base);
        // A base above the cap is kept.
        assert_eq!(next_poll(Duration::from_secs(300), Duration::from_secs(300), false), Duration::from_secs(300));
    }

    #[test]
    fn poll_interval_defaults_and_parses() {
        assert_eq!(poll_interval(None), Duration::from_secs(10));
        assert_eq!(poll_interval(Some("30")), Duration::from_secs(30));
        assert_eq!(poll_interval(Some("1")), Duration::from_secs(1));
        assert_eq!(poll_interval(Some("0")), Duration::from_secs(10));
        assert_eq!(poll_interval(Some("not-a-number")), Duration::from_secs(10));
        assert_eq!(poll_interval(Some("")), Duration::from_secs(10));
    }

    /// A self-signed test CA, generated with openssl for this test only.
    const TEST_CA_PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIDBTCCAe2gAwIBAgIUO2y7PwS931lItEDUDnHa1qSHASQwDQYJKoZIhvcNAQEL\nBQAwEjEQMA4GA1UEAwwHdGVzdC1jYTAeFw0yNjExMDUwNzA0MTNaFw0yNzExMDUw\nNzA0MTNaMBIxEDAOBgNVBAMMB3Rlc3QtY2EwggEiMA0GCSqGSIb3DQEBAQUAA4IB\nDwAwggEKAoIBAQC8WImK2LxwaAR0CalwOy1Ip3Y70rX1Z2YoVtR8MElfthUzmbLJ\nY1G8BhWFfOBX7VzUWfVlQ1X9fF9/qU1mbI0//Ur2Q1XJUjOxYuVh6fUY520e/nFR\nlZ9w8uuEXhcIjFXKoNKIhzp4u/UFvT1QK/7WARnr0jXvnxHY/ors3V9pbmQ0fiUh\n6Hu4pbQQKFM8jhfBXxWsRHuHb4EfRq/I4h3rPy1uT0vQler4n7wjO5O4gnu7ycWP\nN5Aj2H28/XwM50XeSbA+jxSbT/9DVCfO55FRYt7NboDqns2XSxQ2Ibzv1ornVrSY\nZ1aqekkU7c2llM5LDFZMEfZW9O37vXspkrcvAgMBAAGjUzBRMB0GA1UdDgQWBBQS\nGp/Imze1tRKlFcpprYl1ak+x4jAfBgNVHSMEGDAWgBQSGp/Imze1tRKlFcpprYl1\nak+x4jAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3DQEBCwUAA4IBAQBWLPAs+vRN\njOBB2iUrIow8zQqwtUks/xzOm+1/zDfRKWJ3feZgBQFbJRhc30Cd2eVhK4ZuM532\njoDdiGdQ6bE8dEHKYwXqQia+o37CIY8t6sNNE/+yqyz2ARf06UiXd2zxI1djgEaZ\nXE8GK5qy2ADJ2fslf9rQPO9ytimQlTkb8MAJERcbVWh4bjKCCBoxTJzVhkLXnDJE\nvgNuo15LU81Pph0ECoqOke+vuQY2Rkp5YSRssNW7w80/A1rI4TS7qGqvTAhCGphu\n09cuaGmAQyrEkE4ZhzHH2jClyue5jJ0ym2H4KPuFgs+aT0fuH6KCUfN6RXyuO1kh\nUt9Jhx28B+x6\n-----END CERTIFICATE-----\n";

    #[test]
    fn tls_config_loads_ssl_cert_file_and_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("letagents-test-certs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("ca.pem");
        std::fs::write(&file, TEST_CA_PEM).unwrap();
        let old_file = std::env::var_os("SSL_CERT_FILE");
        let old_dir = std::env::var_os("SSL_CERT_DIR");
        std::env::set_var("SSL_CERT_FILE", &file);
        std::env::remove_var("SSL_CERT_DIR");
        let from_file = tls_config();
        std::env::remove_var("SSL_CERT_FILE");
        std::env::set_var("SSL_CERT_DIR", &dir);
        let from_dir = tls_config();
        match old_file {
            Some(v) => std::env::set_var("SSL_CERT_FILE", v),
            None => std::env::remove_var("SSL_CERT_FILE"),
        }
        match old_dir {
            Some(v) => std::env::set_var("SSL_CERT_DIR", v),
            None => std::env::remove_var("SSL_CERT_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(from_file.is_ok(), "tls_config with SSL_CERT_FILE: {:?}", from_file.err());
        assert!(from_dir.is_ok(), "tls_config with SSL_CERT_DIR: {:?}", from_dir.err());
    }

    #[test]
    fn tls_config_rejects_bad_ssl_cert_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("letagents-test-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("bad.pem");
        std::fs::write(&file, "not a certificate\n").unwrap();
        let old = std::env::var_os("SSL_CERT_FILE");
        let old_dir = std::env::var_os("SSL_CERT_DIR");
        std::env::set_var("SSL_CERT_FILE", &file);
        std::env::remove_var("SSL_CERT_DIR");
        let r = tls_config();
        match old {
            Some(v) => std::env::set_var("SSL_CERT_FILE", v),
            None => std::env::remove_var("SSL_CERT_FILE"),
        }
        match old_dir {
            Some(v) => std::env::set_var("SSL_CERT_DIR", v),
            None => std::env::remove_var("SSL_CERT_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(r.is_err(), "tls_config should fail on a bad SSL_CERT_FILE");
    }
}
