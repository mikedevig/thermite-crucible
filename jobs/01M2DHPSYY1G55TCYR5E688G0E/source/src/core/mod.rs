use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};
use tokio::sync::RwLock;

use crate::paths::Exliatycld;
use crate::protocol::{CoreKind, SetupConfig};

pub mod hy2_auth;
mod hy2;
pub mod hy2_stats;
mod xray;
pub mod xray_api;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VpnUser {
    pub id: String,
    pub credential: String,
}

pub struct CoreRuntime {
    pub setup: Option<SetupConfig>,
    pub users: Vec<VpnUser>,
    child: Option<Child>,
    /// Live credential table for hysteria2's HTTP-auth backend (hy2_auth.rs).
    /// Mutating this is how hy2 users get added/removed with zero restart.
    hy2_auth: hy2_auth::SharedAuthMap,
}

impl CoreRuntime {
    pub fn new() -> Self {
        let hy2_auth: hy2_auth::SharedAuthMap = Arc::new(RwLock::new(HashMap::new()));
        // Always-on: harmless to bind even before a hy2 server exists, and
        // avoids a chicken/egg problem where the backend must be listening
        // before hysteria2's first config (pointing at it) is written.
        hy2_auth::spawn(hy2_auth.clone());
        Self {
            setup: None,
            users: Vec::new(),
            child: None,
            hy2_auth,
        }
    }

    /// Stop the running core process (hy2/xray), if any. Used for
    /// `server.teardown` - the dashboard already deleted its DB rows, this
    /// just needs to make sure nothing keeps listening on this box.
    pub async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    pub async fn load_users(&mut self, exliatycld: &Exliatycld) -> Result<()> {
        let p = exliatycld.users_file();
        if p.is_file() {
            let raw = tokio::fs::read_to_string(&p).await?;
            self.users = serde_json::from_str(&raw).unwrap_or_default();
        }
        self.sync_hy2_auth().await;
        Ok(())
    }

    async fn save_users(&self, exliatycld: &Exliatycld) -> Result<()> {
        tokio::fs::write(exliatycld.users_file(), serde_json::to_vec_pretty(&self.users)?).await?;
        Ok(())
    }

    /// Rebuild the in-memory credential->id table hysteria2's auth backend
    /// reads from, from the current `self.users`.
    async fn sync_hy2_auth(&self) {
        let mut map = self.hy2_auth.write().await;
        map.clear();
        for u in &self.users {
            map.insert(u.credential.clone(), u.id.clone());
        }
    }

    pub async fn apply_setup(
        &mut self,
        exliatycld: &Exliatycld,
        setup: SetupConfig,
        public_host: &str,
    ) -> Result<String> {
        if self.users.is_empty() && !setup.auth.is_empty() {
            self.users.push(VpnUser {
                id: "owner".into(),
                credential: setup.auth.clone(),
            });
        }
        self.setup = Some(setup.clone());
        self.save_users(exliatycld).await?;
        self.sync_hy2_auth().await;
        // First bring-up always needs an actual process start.
        self.write_and_restart(exliatycld).await?;
        Ok(self.share_url(public_host, &setup.auth)?)
    }

    /// Server-level settings changed (port, obfs, tls, protocol, bandwidth...).
    /// This genuinely requires a restart on both cores -- neither Hysteria2
    /// nor Xray can rebind a listening port or change protocol/TLS live.
    pub async fn apply_update(&mut self, exliatycld: &Exliatycld, setup: SetupConfig) -> Result<()> {
        if let Some(old) = &self.setup {
            if old.auth != setup.auth && !setup.auth.is_empty() {
                if let Some(u) = self.users.iter_mut().find(|u| u.id == "owner") {
                    u.credential = setup.auth.clone();
                }
            }
        }
        self.setup = Some(setup);
        self.save_users(exliatycld).await?;
        self.sync_hy2_auth().await;
        self.write_and_restart(exliatycld).await
    }

    /// Add a VPN user *live*: Hysteria2 via its HTTP-auth table (in-memory,
    /// instant), Xray via the real `HandlerService.AlterInbound` gRPC API
    /// (instant). No config rewrite, no killing the running process, no
    /// impact on any other connected user. Falls back to a full
    /// rewrite+restart only if the live API call itself fails (e.g. Xray's
    /// gRPC endpoint isn't up yet for some reason) so user creation still
    /// succeeds either way.
    pub async fn create_user(
        &mut self,
        exliatycld: &Exliatycld,
        credential: String,
        public_host: &str,
    ) -> Result<(String, String)> {
        let id = uuid::Uuid::new_v4().to_string();
        self.users.push(VpnUser {
            id: id.clone(),
            credential: credential.clone(),
        });
        self.save_users(exliatycld).await?;

        let core = self
            .setup
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("core not configured"))?
            .core;
        match core {
            CoreKind::Hy2 => {
                self.sync_hy2_auth().await;
            }
            CoreKind::Xray => {
                let protocol = self
                    .setup
                    .as_ref()
                    .and_then(|s| s.protocol.clone())
                    .unwrap_or_else(|| "vless".into());
                if let Err(e) = xray_api::add_user(&protocol, &id, &credential).await {
                    tracing::warn!(error = %e, "xray live add_user failed, falling back to full rewrite+restart");
                    self.write_and_restart(exliatycld).await?;
                }
            }
        }

        let link = self.share_url(public_host, &credential)?;
        Ok((id, link))
    }

    /// Ban (fully remove) a VPN user live, same no-restart paths as create_user.
    pub async fn ban_user(&mut self, exliatycld: &Exliatycld, ident: &str) -> Result<()> {
        let removed = self
            .users
            .iter()
            .find(|u| u.id == ident || u.credential == ident)
            .cloned();
        let Some(removed) = removed else {
            anyhow::bail!("user {ident} not found on node");
        };
        self.users
            .retain(|u| u.id != removed.id || u.credential != removed.credential);
        self.save_users(exliatycld).await?;

        let core = self
            .setup
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("core not configured"))?
            .core;
        match core {
            CoreKind::Hy2 => {
                self.sync_hy2_auth().await;
            }
            CoreKind::Xray => {
                if let Err(e) = xray_api::remove_user(&removed.id).await {
                    tracing::warn!(error = %e, "xray live remove_user failed, falling back to full rewrite+restart");
                    self.write_and_restart(exliatycld).await?;
                }
            }
        }
        Ok(())
    }

    /// Kick (drop the *current* session) live where the core actually
    /// supports it:
    ///  - Hysteria2's Traffic Stats API has a real `/kick` that closes the
    ///    active connection immediately (`hy2_stats.rs`) -- no restart.
    ///  - Xray's API only edits the user table (AlterInbound), it has no
    ///    "close this open connection" call, so we fall back to a restart
    ///    there, which affects every user on that server momentarily.
    pub async fn kick_user(
        &mut self,
        exliatycld: &Exliatycld,
        http: &reqwest::Client,
        ident: &str,
    ) -> Result<()> {
        if !self
            .users
            .iter()
            .any(|u| u.id == ident || u.credential == ident)
        {
            anyhow::bail!("user {ident} not found on node");
        }
        let core = self
            .setup
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("core not configured"))?
            .core;
        match core {
            CoreKind::Hy2 => {
                hy2_stats::kick(http, &[ident.to_string()]).await?;
                Ok(())
            }
            CoreKind::Xray => self.write_and_restart(exliatycld).await,
        }
    }

    /// Poll each core's real traffic API for per-user usage since the last
    /// call and shape it into the `xray.users` / `hy2.users` fragment the
    /// control server's `parseUserTraffic` expects (see clientHub.js).
    /// Returns an empty object if there's nothing new or no core is set up
    /// yet -- callers just merge this into the outgoing ALIVE payload.
    pub async fn collect_traffic(&self, http: &reqwest::Client) -> serde_json::Value {
        use serde_json::json;
        match self.setup.as_ref().map(|s| s.core) {
            Some(CoreKind::Xray) => match xray_api::get_users_traffic().await {
                Ok(list) if !list.is_empty() => {
                    let arr: Vec<serde_json::Value> = list
                        .into_iter()
                        .map(|u| json!({ "email": u.email, "ul": u.uplink, "dl": u.downlink, "ip": u.ip }))
                        .collect();
                    json!({ "xray.users": arr })
                }
                Ok(_) => json!({}),
                Err(e) => {
                    tracing::debug!(error = %e, "xray traffic poll failed (ok if xray just restarted)");
                    json!({})
                }
            },
            Some(CoreKind::Hy2) => {
                let list = hy2_stats::poll_traffic(http).await;
                if list.is_empty() {
                    json!({})
                } else {
                    let arr: Vec<serde_json::Value> = list
                        .into_iter()
                        .map(|u| json!({ "id": u.id, "tx": u.tx, "rx": u.rx }))
                        .collect();
                    json!({ "hy2.users": arr })
                }
            }
            None => json!({}),
        }
    }

    pub fn share_url(&self, public_host: &str, credential: &str) -> Result<String> {
        let setup = self
            .setup
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("core not configured"))?;
        let user = self
            .users
            .iter()
            .find(|u| u.credential == credential)
            .map(|u| u.id.as_str())
            .unwrap_or("owner");
        match setup.core {
            CoreKind::Hy2 => hy2::share_url(setup, public_host, user, credential),
            CoreKind::Xray => xray::share_url(setup, public_host, credential),
        }
    }

    async fn write_and_restart(&mut self, exliatycld: &Exliatycld) -> Result<()> {
        let setup = self
            .setup
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("core not configured"))?;
        match setup.core {
            CoreKind::Hy2 => hy2::write_config(exliatycld, setup)?,
            CoreKind::Xray => xray::write_config(exliatycld, setup, &self.users)?,
        }
        self.restart(exliatycld, setup.core).await
    }

    async fn restart(&mut self, exliatycld: &Exliatycld, kind: CoreKind) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        let mut cmd = match kind {
            CoreKind::Hy2 => {
                let mut c = Command::new(exliatycld.hy2_bin());
                c.arg("server").arg("-c").arg(exliatycld.hy2_config());
                c
            }
            CoreKind::Xray => {
                let mut c = Command::new(exliatycld.xray_bin());
                c.arg("run").arg("-c").arg(exliatycld.xray_config());
                c
            }
        };
        cmd.kill_on_drop(true);
        tracing::info!(?kind, "starting core process");
        self.child = Some(cmd.spawn().with_context(|| format!("spawn {kind:?}"))?);
        Ok(())
    }
}
