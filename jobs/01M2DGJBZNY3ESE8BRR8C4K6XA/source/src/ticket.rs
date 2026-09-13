use anyhow::{Context, Result};
use url::Url;

use crate::protocol::RegTicket;

pub async fn mint_ticket(http: &reqwest::Client, control_base: &str) -> Result<RegTicket> {
    let url = Url::parse(control_base)
        .context("control server URL")?
        .join("/api/regticket")
        .context("regticket URL")?;
    let ticket: RegTicket = http
        .get(url)
        .send()
        .await
        .context("GET /api/regticket")?
        .error_for_status()
        .context("regticket HTTP status")?
        .json()
        .await
        .context("regticket JSON")?;
    if ticket.status.to_uppercase() != "OKAY" {
        anyhow::bail!("regticket status was {}", ticket.status);
    }
    if ticket.token.is_empty() || ticket.setup_code.is_empty() {
        anyhow::bail!("regticket missing token or setup_code");
    }
    Ok(ticket)
}

pub fn clientws_url(control_base: &str, token: &str) -> Result<Url> {
    let mut u = Url::parse(control_base).context("control server URL")?;
    match u.scheme() {
        "https" => {
            u.set_scheme("wss").ok();
        }
        "http" => {
            u.set_scheme("ws").ok();
        }
        "wss" | "ws" => {}
        other => anyhow::bail!("unsupported control scheme {other}"),
    }
    u.set_path("/clientws");
    u.set_query(None);
    u.query_pairs_mut().append_pair("token", token);
    Ok(u)
}
