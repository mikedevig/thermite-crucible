mod banner;
mod certs;
mod core;
mod download;
mod handler;
mod paths;
mod protocol;
mod state;
mod stats;
mod ticket;
mod ws;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use crate::core::CoreRuntime;
use crate::handler::Handler;
use crate::paths::Exliatycld;

#[derive(Debug, Parser)]
#[command(name = "exliatycl-client", about = "Exliatycl node agent", version)]
struct Cli {
    /// Control server origin, e.g. https://vpn.example.com or http://127.0.0.1:3000
    #[arg(long, env = "EXLIATYCLD_SERVER", default_value = "https://cliapi.exliatycl.online")]
    server: String,

    /// Hostname/IP put into share URLs (hy2/vless/…). If omitted, autodetected from the
    /// machine's public-facing IP.
    #[arg(long, env = "EXLIATYCLD_PUBLIC_HOST")]
    public_host: Option<String>,

    /// Directory for downloaded cores, certs, configs, and node state.
    #[arg(long, env = "EXLIATYCLD_DIR", default_value = "./exliatycld")]
    exliatycld: PathBuf,

    /// Re-download Hysteria2 / Xray-core even if the binary already exists.
    #[arg(long)]
    force_download: bool,
}

/// Ask a couple of plain-text "what's my IP" endpoints, in order, and return the first
/// answer that looks like a valid IPv4/IPv6 address.
async fn detect_public_host(http: &reqwest::Client) -> Result<String> {
    const ENDPOINTS: &[&str] = &[
        "https://api.ipify.org",
        "https://ifconfig.me/ip",
        "https://icanhazip.com",
    ];

    let mut last_err = None;
    for endpoint in ENDPOINTS {
        match http.get(*endpoint).send().await {
            Ok(resp) => match resp.error_for_status() {
                Ok(resp) => match resp.text().await {
                    Ok(body) => {
                        let ip = body.trim().to_string();
                        if !ip.is_empty() && ip.parse::<std::net::IpAddr>().is_ok() {
                            return Ok(ip);
                        }
                        last_err = Some(anyhow::anyhow!("{endpoint} returned unparsable body"));
                    }
                    Err(e) => last_err = Some(e.into()),
                },
                Err(e) => last_err = Some(e.into()),
            },
            Err(e) => last_err = Some(e.into()),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no public-host endpoint reachable")))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let exliatycld = Exliatycld::new(cli.exliatycld.clone());
    exliatycld.ensure_dirs().await.context("create exliatycld/")?;

    let http = reqwest::Client::builder()
        .user_agent("exliatycl-client/1.0")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .build()?;

    let existing_state = state::load(&exliatycld.state_file()).await?;
    let first_run = !matches!(&existing_state, Some(s) if s.control_base == cli.server);

    if first_run {
        banner::title("Exliatycl");
        banner::step("Checking IP..........");
    }
    let public_host = match cli.public_host.clone() {
        Some(h) => h,
        None => detect_public_host(&http)
            .await
            .context("autodetect public host (pass --public-host to skip)")?,
    };
    if first_run {
        banner::step_done();
    }
    if !first_run {
        tracing::info!(%public_host, "public host");
    }

    let node = match existing_state {
        Some(existing) if existing.control_base == cli.server => existing,
        _ => {
            if first_run {
                banner::step("Registering...........");
            }
            let ticket = ticket::mint_ticket(&http, &cli.server).await?;
            if first_run {
                banner::step_done();
                banner::token(&ticket.setup_code);
            }
            if !first_run {
                tracing::info!(code = %ticket.setup_code, "enter this PIN on the dashboard");
            }
            let node = state::NodeState {
                control_base: cli.server.clone(),
                setup_code: ticket.setup_code,
                token: ticket.token,
            };
            state::save(&exliatycld.state_file(), &node).await?;
            node
        }
    };

    if !first_run {
        println!("Exliatycl setup code: {}", node.setup_code);
        println!(
            "Waiting for dashboard (token kept in {})",
            exliatycld.state_file().display()
        );
    }

    let (stage_tx, mut stage_rx) = crate::handler::stage_channel();
    let mirror_base = format!("{}/clientapi/fetch-bin", cli.server.trim_end_matches('/'));
    let mut handler = Handler {
        exliatycld: exliatycld.clone(),
        http,
        public_host,
        force_download: cli.force_download,
        mirror_base,
        runtime: CoreRuntime::new(),
        announce_link: first_run,
        stage_tx,
    };
    handler.runtime.load_users(&handler.exliatycld).await?;
    let handler = Arc::new(Mutex::new(handler));

    let mut backoff = Duration::from_secs(1);
    let mut show_setup_banner = first_run;
    loop {
        let banner_this_iter = show_setup_banner;
        if banner_this_iter {
            banner::step("Connecting..........");
            show_setup_banner = false;
        }
        let cb: Option<Box<dyn FnOnce() + Send>> = if banner_this_iter {
            Some(Box::new(|| {
                banner::step_done();
                println!("Waiting for you to enter the PIN on the dashboard...");
            }))
        } else {
            None
        };
        match ws::run_session(
            handler.clone(),
            &mut stage_rx,
            &node.control_base,
            &node.token,
            cb,
            banner_this_iter,
        )
        .await
        {
            Ok(()) => {}
            Err(e) => {
                if banner_this_iter {
                    banner::step_failed();
                }
                tracing::warn!(error = %e, "control session ended");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}
