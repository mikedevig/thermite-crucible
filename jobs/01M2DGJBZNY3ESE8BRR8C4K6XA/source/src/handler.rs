use anyhow::Result;
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::certs;
use crate::core::CoreRuntime;
use crate::download;
use crate::paths::Exliatycld;
use crate::protocol::{self, CoreKind, SetupConfig};

pub struct Handler {
    pub exliatycld: Exliatycld,
    pub http: reqwest::Client,
    pub public_host: String,
    pub force_download: bool,
    /// `{control server origin}/clientapi/fetch-bin` - derived from
    /// `--server`, not hardcoded, so self-hosters pointing at their own
    /// control server get their own binary mirror for free.
    pub mirror_base: String,
    pub runtime: CoreRuntime,
    /// True until the dashboard actually sends `first.setup.config` for the first
    /// time, at which point we print the "linked successfully" banner once and
    /// flip this off for good.
    pub announce_link: bool,
    /// Fires interim `setup.stage` events (downloading binaries, generating
    /// certs, starting up…) out over the control channel while a
    /// `first.setup.config` push is being applied. `ws.rs` forwards anything
    /// sent here straight to the dashboard, which relays it to the browser
    /// over SSE. Sending is best-effort: a full/closed channel is ignored.
    pub stage_tx: UnboundedSender<Value>,
}

/// Convenience constructor for the stage channel; call this once in
/// `main.rs` and pass the sender/receiver into `Handler { .. }`.
pub fn stage_channel() -> (UnboundedSender<Value>, UnboundedReceiver<Value>) {
    tokio::sync::mpsc::unbounded_channel()
}

impl Handler {
    fn emit_stage(&self, stage: &str, detail: Option<&str>, req_id: Option<&str>) {
        let _ = self.stage_tx.send(protocol::stage(stage, detail, req_id));
    }
}

impl Handler {
    pub fn current_core(&self) -> CoreKind {
        self.runtime
            .setup
            .as_ref()
            .map(|s| s.core)
            .unwrap_or(CoreKind::Hy2)
    }

    pub async fn on_message(&mut self, msg: Value) -> Result<Option<Value>> {
        let action = protocol::incoming_action(&msg).unwrap_or("");
        let req_id = protocol::incoming_req_id(&msg).map(str::to_owned);
        match action {
            "first.setup.config" => {
                let setup = SetupConfig::from_server_json(&msg)?;
                let rid = req_id.as_deref();
                self.prepare_core(setup.core, rid).await?;
                self.emit_stage("generating_config", None, rid);
                let url = self
                    .runtime
                    .apply_setup(&self.exliatycld, setup.clone(), &self.public_host)
                    .await?;
                self.emit_stage("starting_service", None, rid);
                if self.announce_link {
                    self.announce_link = false;
                    crate::banner::linked();
                }
                Ok(Some(protocol::setup_ok(
                    setup.core,
                    &url,
                    req_id.as_deref(),
                )))
            }
            "server.config.update" => {
                let setup = SetupConfig::from_server_json(&msg)?;
                self.prepare_core(setup.core, req_id.as_deref()).await?;
                match self.runtime.apply_update(&self.exliatycld, setup).await {
                    Ok(()) => Ok(Some(protocol::ack(
                        "server.config.update.ack",
                        req_id.as_deref(),
                        serde_json::json!({}),
                    ))),
                    Err(e) => Ok(Some(protocol::error_ack(
                        "server.config.update.ack",
                        req_id.as_deref(),
                        &e.to_string(),
                    ))),
                }
            }
            "user.createnew" => {
                let cred = protocol::incoming_auth(&msg).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                match self
                    .runtime
                    .create_user(&self.exliatycld, cred, &self.public_host)
                    .await
                {
                    Ok((user, link)) => Ok(Some(protocol::ack(
                        "user.createnew.ack",
                        req_id.as_deref(),
                        serde_json::json!({ "user": user, "link": link }),
                    ))),
                    Err(e) => Ok(Some(protocol::error_ack(
                        "user.createnew.ack",
                        req_id.as_deref(),
                        &e.to_string(),
                    ))),
                }
            }
            "user.ban" => {
                let user = protocol::incoming_user(&msg).unwrap_or("").to_string();
                match self.runtime.ban_user(&self.exliatycld, &user).await {
                    Ok(()) => Ok(Some(protocol::ack(
                        "user.ban.ack",
                        req_id.as_deref(),
                        serde_json::json!({ "user": user }),
                    ))),
                    Err(e) => Ok(Some(protocol::error_ack(
                        "user.ban.ack",
                        req_id.as_deref(),
                        &e.to_string(),
                    ))),
                }
            }
            "user.kick" => {
                let user = protocol::incoming_user(&msg).unwrap_or("").to_string();
                match self.runtime.kick_user(&self.exliatycld, &self.http, &user).await {
                    Ok(()) => Ok(Some(protocol::ack(
                        "user.kick.ack",
                        req_id.as_deref(),
                        serde_json::json!({ "user": user }),
                    ))),
                    Err(e) => Ok(Some(protocol::error_ack(
                        "user.kick.ack",
                        req_id.as_deref(),
                        &e.to_string(),
                    ))),
                }
            }
            "server.teardown" => {
                self.emit_stage("stopping_service", None, req_id.as_deref());
                self.runtime.stop().await;
                tracing::warn!("received server.teardown - stopping core and exiting");
                // Fire-and-forget from the dashboard's side (it already
                // dropped its DB rows), so no ACK is required. Give the
                // event loop a beat to flush anything still in-flight, then
                // exit the whole process so a fresh run prints a brand-new
                // setup PIN if this box is ever reattached to a server.
                tokio::spawn(async {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    std::process::exit(0);
                });
                Ok(None)
            }
            "" => Ok(None),
            other => {
                tracing::warn!(action = other, "unhandled control action");
                Ok(None)
            }
        }
    }

    async fn prepare_core(&self, kind: CoreKind, req_id: Option<&str>) -> Result<()> {
        self.emit_stage("downloading_binary", Some(kind.prefix()), req_id);
        download::ensure_core(&self.http, &self.exliatycld, kind, self.force_download, &self.mirror_base).await?;
        self.emit_stage("generating_certs", None, req_id);
        certs::ensure_self_signed(&self.exliatycld.cert_pem(), &self.exliatycld.key_pem(), &self.public_host)?;
        Ok(())
    }
}
