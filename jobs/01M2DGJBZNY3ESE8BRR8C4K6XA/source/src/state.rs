use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeState {
    pub control_base: String,
    pub setup_code: String,
    pub token: String,
}

pub async fn load(path: &Path) -> Result<Option<NodeState>> {
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        return Ok(None);
    }
    let raw = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    Ok(Some(serde_json::from_str(&raw).context("parse node state")?))
}

pub async fn save(path: &Path, state: &NodeState) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, serde_json::to_vec_pretty(state)?).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}
