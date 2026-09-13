use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::process::Command;

use super::VpnUser;
use crate::core::xray_api::{INBOUND_TAG, XRAY_API_PORT};
use crate::paths::Exliatycld;
use crate::protocol::SetupConfig;

#[derive(Serialize, Deserialize)]
struct RealityKeys {
    private_key: String,
    public_key: String,
    short_id: String,
}

pub fn write_config(exliatycld: &Exliatycld, setup: &SetupConfig, users: &[VpnUser]) -> Result<()> {
    let proto = setup
        .protocol
        .as_deref()
        .unwrap_or("vless")
        .to_lowercase();
    let transport = setup.transport.as_deref().unwrap_or("tcp").to_lowercase();
    let tls = setup.tls.as_deref().unwrap_or("none").to_lowercase();

    let clients: Vec<Value> = users
        .iter()
        .map(|u| match proto.as_str() {
            "vmess" | "vless" => json!({
                "id": u.credential,
                "email": u.id,
            }),
            "trojan" => json!({
                "password": u.credential,
                "email": u.id,
            }),
            "ss" | "shadowsocks" => json!({
                "password": u.credential,
                "method": "aes-256-gcm",
                "email": u.id,
            }),
            _ => json!({ "id": u.credential, "email": u.id }),
        })
        .collect();

    let mut stream = json!({ "network": transport });
    match transport.as_str() {
        "ws" => {
            stream["wsSettings"] = json!({ "path": "/wagon" });
        }
        "grpc" => {
            stream["grpcSettings"] = json!({ "serviceName": "wagon" });
        }
        _ => {}
    }

    match tls.as_str() {
        "tls" => {
            stream["security"] = json!("tls");
            stream["tlsSettings"] = json!({
                "certificates": [{
                    "certificateFile": exliatycld.cert_pem().to_string_lossy(),
                    "keyFile": exliatycld.key_pem().to_string_lossy(),
                }]
            });
        }
        "reality" => {
            let keys = ensure_reality(exliatycld)?;
            stream["security"] = json!("reality");
            stream["realitySettings"] = json!({
                "dest": "www.microsoft.com:443",
                "serverNames": ["www.microsoft.com"],
                "privateKey": keys.private_key,
                "shortIds": [keys.short_id],
            });
        }
        _ => {
            stream["security"] = json!("none");
        }
    }

    let inbound = match proto.as_str() {
            "ss" | "shadowsocks" => json!({
                "tag": INBOUND_TAG,
                "port": setup.port,
                "protocol": "shadowsocks",
                "settings": { "clients": clients, "network": "tcp,udp" },
                "streamSettings": stream,
            }),
            "vless" => json!({
                "tag": INBOUND_TAG,
                "port": setup.port,
                "protocol": "vless",
                "settings": { "clients": clients, "decryption": "none" },
                "streamSettings": stream,
            }),
            other => json!({
                "tag": INBOUND_TAG,
                "port": setup.port,
                "protocol": other,
                "settings": { "clients": clients },
                "streamSettings": stream,
            }),
        };

    let cfg = json!({
        "log": { "loglevel": "warning" },
        // Real Xray-core management API (gRPC), loopback-only:
        //  - HandlerService: AlterInbound(AddUserOperation/RemoveUserOperation)
        //    for live, no-restart user add/remove (src/core/xray_api.rs).
        //  - StatsService: GetUsersStats for live per-user traffic accounting,
        //    polled every ALIVE tick and reported to the control server.
        // `policy` + `stats` below are what actually turn per-user counters on;
        // without them Xray never populates traffic for GetUsersStats.
        "api": {
            "tag": "api",
            "listen": format!("127.0.0.1:{XRAY_API_PORT}"),
            "services": ["HandlerService", "StatsService"],
        },
        "policy": {
            "levels": {
                "0": { "statsUserUplink": true, "statsUserDownlink": true },
            },
        },
        "stats": {},
        "inbounds": [inbound],
        "outbounds": [{ "protocol": "freedom", "tag": "direct" }],
    });
    std::fs::write(exliatycld.xray_config(), serde_json::to_vec_pretty(&cfg)?)?;
    Ok(())
}

fn ensure_reality(exliatycld: &Exliatycld) -> Result<RealityKeys> {
    let p = exliatycld.reality_file();
    if p.is_file() {
        let raw = std::fs::read_to_string(&p)?;
        return Ok(serde_json::from_str(&raw)?);
    }
    let out = Command::new(exliatycld.xray_bin())
        .arg("x25519")
        .output()
        .context("xray x25519")?;
    if !out.status.success() {
        anyhow::bail!(
            "xray x25519 failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut private_key = String::new();
    let mut public_key = String::new();
    for line in text.lines() {
        let l = line.trim();
        let lower = l.to_lowercase();
        if lower.starts_with("private") {
            if let Some((_, v)) = l.split_once(':') {
                private_key = v.trim().to_string();
            }
        } else if lower.starts_with("public") || lower.starts_with("password") {
            if let Some((_, v)) = l.split_once(':') {
                public_key = v.trim().to_string();
            }
        }
    }
    if private_key.is_empty() || public_key.is_empty() {
        anyhow::bail!("could not parse xray x25519 output:\n{text}");
    }
    let keys = RealityKeys {
        private_key,
        public_key,
        short_id: format!("{:08x}", rand_u32()),
    };
    std::fs::write(&p, serde_json::to_vec_pretty(&keys)?)?;
    Ok(keys)
}

fn rand_u32() -> u32 {
    let n = uuid::Uuid::new_v4();
    u32::from_le_bytes(n.as_bytes()[0..4].try_into().unwrap())
}

pub fn share_url(setup: &SetupConfig, host: &str, credential: &str) -> Result<String> {
    let proto = setup.protocol.as_deref().unwrap_or("vless").to_lowercase();
    let transport = setup.transport.as_deref().unwrap_or("tcp").to_lowercase();
    let tls = setup.tls.as_deref().unwrap_or("none").to_lowercase();
    let security = match tls.as_str() {
        "tls" => "tls",
        "reality" => "reality",
        _ => "none",
    };
    let insecure = if setup.allow_insecure { "&allowInsecure=1" } else { "" };

    let url = match proto.as_str() {
        "vmess" => {
            let obj = json!({
                "v": "2",
                "ps": "wagon",
                "add": host,
                "port": setup.port.to_string(),
                "id": credential,
                "aid": "0",
                "net": transport,
                "type": "none",
                "tls": if security == "none" { "" } else { security },
            });
            let b64 = base64_nopad(obj.to_string().as_bytes());
            format!("vmess://{b64}")
        }
        "trojan" => format!(
            "trojan://{credential}@{host}:{port}?type={transport}&security={security}{insecure}",
            port = setup.port
        ),
        "ss" | "shadowsocks" => {
            let userinfo = base64_nopad(format!("aes-256-gcm:{credential}").as_bytes());
            format!("ss://{userinfo}@{host}:{port}#wagon", port = setup.port)
        }
        _ => format!(
            "vless://{credential}@{host}:{port}?encryption=none&type={transport}&security={security}{insecure}",
            port = setup.port
        ),
    };
    Ok(url)
}

fn base64_nopad(bytes: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let b1 = if i + 1 < bytes.len() { bytes[i + 1] } else { 0 };
        let b2 = if i + 2 < bytes.len() { bytes[i + 2] } else { 0 };
        let triple = ((b0 as u32) << 16) | ((b1 as u32) << 8) | b2 as u32;
        out.push(T[((triple >> 18) & 63) as usize] as char);
        out.push(T[((triple >> 12) & 63) as usize] as char);
        if i + 1 < bytes.len() {
            out.push(T[((triple >> 6) & 63) as usize] as char);
        }
        if i + 2 < bytes.len() {
            out.push(T[(triple & 63) as usize] as char);
        }
        i += 3;
    }
    out
}
