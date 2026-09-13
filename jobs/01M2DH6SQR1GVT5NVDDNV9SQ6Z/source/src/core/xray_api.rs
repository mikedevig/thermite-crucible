//! Talks to Xray-core's real management API: a gRPC `HandlerService` exposed
//! by Xray itself on a loopback port when the config's `api` block is set
//! (see https://xtls.github.io/en/config/api.html). We call `AlterInbound`
//! with an `AddUserOperation` / `RemoveUserOperation`, which is exactly what
//! Xray-core's own ecosystem (3x-ui, Marzban, Remnawave, etc.) uses to
//! add/remove users on a running inbound *without restarting the process*.
//!
//! The .proto stubs under `proto/` are trimmed to only the messages we send
//! (see comments there) but are wire-compatible with upstream Xray-core:
//! gRPC only cares about the service/method name and the encoded field
//! numbers actually on the wire, not the full sibling message set.

use anyhow::{Context, Result};
use tonic::transport::{Channel, Endpoint};

pub mod pb {
    pub mod command {
        tonic::include_proto!("xray.app.proxyman.command");
    }
    pub mod stats {
        tonic::include_proto!("xray.app.stats.command");
    }
    pub mod protocol {
        tonic::include_proto!("xray.common.protocol");
    }
    pub mod serial {
        tonic::include_proto!("xray.common.serial");
    }
    pub mod vless {
        tonic::include_proto!("xray.proxy.vless");
    }
    pub mod vmess {
        tonic::include_proto!("xray.proxy.vmess");
    }
    pub mod trojan {
        tonic::include_proto!("xray.proxy.trojan");
    }
    pub mod shadowsocks {
        tonic::include_proto!("xray.proxy.shadowsocks");
    }
}

use pb::command::handler_service_client::HandlerServiceClient;
use pb::command::{AddUserOperation, AlterInboundRequest, RemoveUserOperation};
use pb::protocol::User;
use pb::serial::TypedMessage;
use pb::stats::stats_service_client::StatsServiceClient;
use pb::stats::GetUsersStatsRequest;

/// Fixed loopback port for the local Xray API. Never exposed publicly --
/// only used by this process to talk to its own Xray-core child.
pub const XRAY_API_PORT: u16 = 38215;
/// The tag of the single client-facing inbound written in xray.rs.
pub const INBOUND_TAG: &str = "in";

fn typed_account(protocol: &str, credential: &str) -> Result<TypedMessage> {
    use prost::Message;
    let (type_name, bytes) = match protocol {
        "vmess" => (
            "xray.proxy.vmess.Account",
            pb::vmess::Account {
                id: credential.to_string(),
            }
            .encode_to_vec(),
        ),
        "vless" => (
            "xray.proxy.vless.Account",
            pb::vless::Account {
                id: credential.to_string(),
                flow: String::new(),
                encryption: "none".to_string(),
            }
            .encode_to_vec(),
        ),
        "trojan" => (
            "xray.proxy.trojan.Account",
            pb::trojan::Account {
                password: credential.to_string(),
            }
            .encode_to_vec(),
        ),
        "ss" | "shadowsocks" => (
            "xray.proxy.shadowsocks.Account",
            pb::shadowsocks::Account {
                password: credential.to_string(),
                cipher_type: pb::shadowsocks::CipherType::Aes256Gcm as i32,
            }
            .encode_to_vec(),
        ),
        other => anyhow::bail!("xray api: unsupported protocol {other}"),
    };
    Ok(TypedMessage {
        r#type: type_name.to_string(),
        value: bytes,
    })
}

async fn channel() -> Result<Channel> {
    let ep = Endpoint::from_shared(format!("http://127.0.0.1:{XRAY_API_PORT}"))
        .context("build xray api endpoint")?
        .connect_timeout(std::time::Duration::from_secs(3));
    ep.connect().await.context(
        "connect to Xray's local gRPC API (is the api{} block in xray config.json + is xray running?)",
    )
}

async fn client() -> Result<HandlerServiceClient<Channel>> {
    Ok(HandlerServiceClient::new(channel().await?))
}

/// One user's traffic delta + currently-seen IPs since the last poll
/// (StatsService.GetUsersStats is called with reset=true, so uplink/downlink
/// here are deltas, matching the control server's additive accounting).
#[derive(Debug, Clone)]
pub struct UserTraffic {
    pub email: String,
    pub uplink: u64,
    pub downlink: u64,
    pub ip: Option<String>,
}

/// Poll Xray's real per-user traffic counters live. Requires the config's
/// `policy.levels.0.statsUserUplink/Downlink` + `stats{}` + `api.services`
/// including `StatsService` (see xray.rs::write_config).
pub async fn get_users_traffic() -> Result<Vec<UserTraffic>> {
    let mut c = StatsServiceClient::new(channel().await?);
    let resp = c
        .get_users_stats(GetUsersStatsRequest {
            include_traffic: true,
            reset: true,
        })
        .await
        .context("xray api GetUsersStats")?
        .into_inner();

    Ok(resp
        .users
        .into_iter()
        .filter_map(|u| {
            let traffic = u.traffic?;
            if traffic.uplink <= 0 && traffic.downlink <= 0 {
                return None;
            }
            Some(UserTraffic {
                email: u.email,
                uplink: traffic.uplink.max(0) as u64,
                downlink: traffic.downlink.max(0) as u64,
                ip: u.ips.into_iter().next().map(|e| e.ip),
            })
        })
        .collect())
}

/// Add a user to the running Xray inbound live -- no config rewrite, no restart.
pub async fn add_user(protocol: &str, email: &str, credential: &str) -> Result<()> {
    let account = typed_account(protocol, credential)?;
    let mut c = client().await?;
    use prost::Message;
    let op = AddUserOperation {
        user: Some(User {
            level: 0,
            email: email.to_string(),
            account: Some(account),
        }),
    };
    c.alter_inbound(AlterInboundRequest {
        tag: INBOUND_TAG.to_string(),
        operation: Some(TypedMessage {
            r#type: "xray.app.proxyman.command.AddUserOperation".to_string(),
            value: op.encode_to_vec(),
        }),
    })
    .await
    .context("xray api AlterInbound(AddUserOperation)")?;
    Ok(())
}

/// Remove a user from the running Xray inbound live -- no config rewrite, no restart.
pub async fn remove_user(email: &str) -> Result<()> {
    let mut c = client().await?;
    use prost::Message;
    let op = RemoveUserOperation {
        email: email.to_string(),
    };
    c.alter_inbound(AlterInboundRequest {
        tag: INBOUND_TAG.to_string(),
        operation: Some(TypedMessage {
            r#type: "xray.app.proxyman.command.RemoveUserOperation".to_string(),
            value: op.encode_to_vec(),
        }),
    })
    .await
    .context("xray api AlterInbound(RemoveUserOperation)")?;
    Ok(())
}
