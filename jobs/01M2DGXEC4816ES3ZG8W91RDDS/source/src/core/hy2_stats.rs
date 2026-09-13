//! Client for Hysteria2's *other* real API -- the Traffic Stats API
//! (https://v2.hysteria.network/docs/advanced/Traffic-Stats-API/), separate
//! from the auth backend in `hy2_auth.rs`. Enabled via the `trafficStats`
//! block in config.yaml (see `hy2.rs::write_config`). This gives us:
//!
//! - `GET /traffic?clear=1` -- per-user upload/download since the last poll
//!   (the `clear` flag resets counters server-side, so polling on a timer
//!   and summing gives an additive total -- matching the control server's
//!   `recordUserTraffic`).
//! - `POST /kick` -- genuinely closes a user's *current* connection. Unlike
//!   Xray (whose API only edits the user table, not live sessions), this
//!   means `user.kick` for hysteria2 no longer needs a full restart.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;

/// Fixed loopback port for Hysteria2's own Traffic Stats API.
pub const HY2_STATS_PORT: u16 = 38216;
/// Loopback-only, so a fixed shared secret is fine (nothing but this agent
/// and hysteria2 itself ever see this port).
pub const HY2_STATS_SECRET: &str = "exliatycl-local-trafficstats";

#[derive(Debug, Deserialize)]
struct RawTraffic {
    tx: i64,
    rx: i64,
}

#[derive(Debug, Clone)]
pub struct Hy2UserTraffic {
    pub id: String,
    pub tx: u64,
    pub rx: u64,
}

fn base_url() -> String {
    format!("http://127.0.0.1:{HY2_STATS_PORT}")
}

/// Poll + reset per-user traffic. Returns [] (not an error) if the endpoint
/// isn't up yet (e.g. hysteria2 still starting) so ALIVE reporting never
/// fails just because a poll landed a beat too early.
pub async fn poll_traffic(http: &reqwest::Client) -> Vec<Hy2UserTraffic> {
    let url = format!("{}/traffic?clear=1", base_url());
    let resp = match http
        .get(&url)
        .header("Authorization", HY2_STATS_SECRET)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let map: HashMap<String, RawTraffic> = match resp.json().await {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    map.into_iter()
        .filter(|(_, t)| t.tx > 0 || t.rx > 0)
        .map(|(id, t)| Hy2UserTraffic {
            id,
            tx: t.tx.max(0) as u64,
            rx: t.rx.max(0) as u64,
        })
        .collect()
}

/// Forcibly disconnect a user's *current* session (they may reconnect if
/// still allowed by the auth backend -- pair with a ban for a real kick).
pub async fn kick(http: &reqwest::Client, ids: &[String]) -> Result<()> {
    let url = format!("{}/kick", base_url());
    http.post(&url)
        .header("Authorization", HY2_STATS_SECRET)
        .json(ids)
        .send()
        .await
        .context("hy2 traffic-stats API: POST /kick")?
        .error_for_status()
        .context("hy2 traffic-stats API: /kick returned an error status")?;
    Ok(())
}
