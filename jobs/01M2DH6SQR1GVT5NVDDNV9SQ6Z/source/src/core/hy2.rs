use anyhow::Result;
use serde_json::{json, Map, Value};

use crate::core::hy2_auth::HY2_AUTH_PORT;
use crate::core::hy2_stats::{HY2_STATS_PORT, HY2_STATS_SECRET};
use crate::paths::Exliatycld;
use crate::protocol::SetupConfig;

/// Writes hysteria2's config. Auth is delegated to our own local HTTP
/// backend (see `hy2_auth.rs`) instead of baking the whole `userpass` table
/// into this file -- so adding/banning a VPN user never touches this file
/// or the hysteria2 process again after the first write. `trafficStats`
/// exposes Hysteria2's own real per-user traffic + kick API
/// (see `hy2_stats.rs`), loopback-only.
pub fn write_config(exliatycld: &Exliatycld, setup: &SetupConfig) -> Result<()> {
    let mut root = Map::new();
    root.insert("listen".into(), json!(format!(":{}", setup.port)));
    root.insert(
        "tls".into(),
        json!({
            "cert": exliatycld.cert_pem().to_string_lossy(),
            "key": exliatycld.key_pem().to_string_lossy(),
        }),
    );
    root.insert(
        "auth".into(),
        json!({
            "type": "http",
            "http": {
                "url": format!("http://127.0.0.1:{HY2_AUTH_PORT}/auth"),
                "insecure": false,
            },
        }),
    );
    root.insert(
        "trafficStats".into(),
        json!({
            "listen": format!("127.0.0.1:{HY2_STATS_PORT}"),
            "secret": HY2_STATS_SECRET,
        }),
    );
    if let Some(obfs) = setup.obfs.as_ref().filter(|s| !s.is_empty()) {
        root.insert(
            "obfs".into(),
            json!({
                "type": "salamander",
                "salamander": { "password": obfs },
            }),
        );
    }
    if !setup.bw_ul.is_empty() || !setup.bw_dl.is_empty() {
        let mut bw = Map::new();
        if !setup.bw_ul.is_empty() {
            bw.insert("up".into(), json!(setup.bw_ul));
        }
        if !setup.bw_dl.is_empty() {
            bw.insert("down".into(), json!(setup.bw_dl));
        }
        root.insert("bandwidth".into(), Value::Object(bw));
    }

    let yaml = serde_yaml::to_string(&Value::Object(root))?;
    std::fs::write(exliatycld.hy2_config(), yaml)?;
    Ok(())
}

pub fn share_url(setup: &SetupConfig, host: &str, user: &str, credential: &str) -> Result<String> {
    let mut url = format!(
        "hysteria2://{user}:{cred}@{host}:{port}/",
        user = urlencoding(user),
        cred = urlencoding(credential),
        host = host,
        port = setup.port,
    );
    let mut q = Vec::new();
    if let Some(obfs) = setup.obfs.as_ref().filter(|s| !s.is_empty()) {
        q.push(format!("obfs=salamander"));
        q.push(format!("obfs-password={}", urlencoding(obfs)));
    }
    if setup.allow_insecure {
        q.push("insecure=1".into());
    }
    if !q.is_empty() {
        url.push('?');
        url.push_str(&q.join("&"));
    }
    Ok(url)
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
