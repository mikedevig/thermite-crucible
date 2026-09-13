//! Implements the backend side of Hysteria2's `auth: {type: http}` contract:
//!
//! Hysteria2 POSTs `{"addr": "...", "auth": "...", "tx": ...}` to us for every
//! connecting client and expects `{"ok": true, "id": "..."}` with HTTP 200 on
//! success (see https://v2.hysteria.network/docs/advanced/Full-Server-Config/#authentication).
//!
//! Running our own tiny loopback backend (instead of `type: userpass`, which
//! bakes the whole user table into hysteria2's config file) means adding,
//! banning, or unbanning a VPN user is a single in-memory map update -- no
//! config rewrite, no killing/restarting the hysteria2 process, and existing
//! sessions of *other* users are never interrupted.
//!
//! This is a hand-rolled, minimal HTTP/1.1 server (no new HTTP-server
//! dependency) since the contract is a single small POST/JSON exchange on a
//! loopback-only port that nothing but hysteria2 itself ever talks to.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

/// Fixed loopback port for the Hysteria2 HTTP-auth backend. Never exposed
/// publicly -- only referenced from hysteria2's own config.yaml on this host.
pub const HY2_AUTH_PORT: u16 = 38214;

/// credential -> vpn user id, shared with CoreRuntime so create/ban/unban can
/// mutate it directly without touching hysteria2 at all.
pub type SharedAuthMap = Arc<RwLock<HashMap<String, String>>>;

#[derive(Deserialize)]
struct AuthReq {
    #[allow(dead_code)]
    addr: String,
    auth: String,
    #[allow(dead_code)]
    tx: Option<u64>,
}

/// Spawn the auth backend once per process lifetime. Safe to call repeatedly;
/// only the first call actually binds the listener.
pub fn spawn(users: SharedAuthMap) {
    tokio::spawn(async move {
        loop {
            match TcpListener::bind(("127.0.0.1", HY2_AUTH_PORT)).await {
                Ok(listener) => {
                    tracing::info!(port = HY2_AUTH_PORT, "hy2 http-auth backend listening");
                    accept_loop(listener, users.clone()).await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "hy2 http-auth backend failed to bind, retrying in 2s");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
    });
}

async fn accept_loop(listener: TcpListener, users: SharedAuthMap) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, "hy2 http-auth accept failed");
                continue;
            }
        };
        let users = users.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, users).await {
                tracing::debug!(error = %e, "hy2 http-auth connection error");
            }
        });
    }
}

async fn handle_conn(mut stream: tokio::net::TcpStream, users: SharedAuthMap) -> Result<()> {
    // Read until we have the full header block, then use Content-Length to
    // read exactly the body. hysteria2 sends a single small JSON POST per
    // request and does not pipeline, so this simple approach is sufficient.
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            anyhow::bail!("connection closed before headers complete");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 16 * 1024 {
            anyhow::bail!("headers too large");
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]);
    let content_length: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.trim().eq_ignore_ascii_case("content-length") {
                v.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);

    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    let resp = match serde_json::from_slice::<AuthReq>(&body) {
        Ok(req) => {
            // Hysteria2 forwards the client's raw auth string verbatim. Our
            // share URLs encode it as "<id>:<credential>" (see hy2.rs), so
            // split on the first colon and match by credential -- this
            // keeps existing share links working unchanged.
            let cred = req
                .auth
                .split_once(':')
                .map(|(_, c)| c)
                .unwrap_or(&req.auth);
            let map = users.read().await;
            match map.get(cred) {
                Some(id) => serde_json::json!({ "ok": true, "id": id }),
                None => serde_json::json!({ "ok": false }),
            }
        }
        Err(_) => serde_json::json!({ "ok": false }),
    };

    let body = serde_json::to_vec(&resp)?;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.shutdown().await?;
    Ok(())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
