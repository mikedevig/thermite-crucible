//! Architecture-specific downloads for Hysteria2 and Xray-core.
//!
//! Primary source is our own mirror (`cliapi.exliatycl.online`), since raw
//! GitHub / githubusercontent downloads are sometimes blocked on the
//! networks these nodes run on. GitHub releases are kept as a fallback for
//! resilience if the mirror itself is ever unreachable.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use crate::paths::Exliatycld;
use crate::protocol::CoreKind;

const UA: &str = "exliatycl-client/1.0 (+https://github.com/XTLS/Xray-core)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTarget {
    pub os: &'static str,
    pub arch: &'static str,
}

impl HostTarget {
    pub fn detect() -> Self {
        Self {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        }
    }

    /// `{ARCH}` tag used against our own mirror
    /// (`.../clientapi/fetch-bin/{hy2,xray}/{ARCH}`). One shared tag scheme
    /// across both cores - the mirror itself is responsible for mapping it
    /// to whichever upstream asset name that core actually uses.
    pub fn mirror_arch(self) -> Result<&'static str> {
        Ok(match (self.os, self.arch) {
            ("windows", "x86_64") => "windows-amd64",
            ("windows", "aarch64") => "windows-arm64",
            ("windows", "x86") => "windows-386",
            ("linux", "x86_64") => "linux-amd64",
            ("linux", "aarch64") => "linux-arm64",
            ("linux", "arm") => "linux-arm",
            ("linux", "x86") => "linux-386",
            ("linux", "riscv64") => "linux-riscv64",
            ("macos", "x86_64") => "darwin-amd64",
            ("macos", "aarch64") => "darwin-arm64",
            ("freebsd", "x86_64") => "freebsd-amd64",
            ("freebsd", "aarch64") => "freebsd-arm64",
            (os, arch) => bail!("no mirror build tag for {os}/{arch}"),
        })
    }

    /// Hysteria2 GitHub asset filename for this host (no `-avx` variant).
    pub fn hysteria_asset(self) -> Result<&'static str> {
        Ok(match (self.os, self.arch) {
            ("windows", "x86_64") => "hysteria-windows-amd64.exe",
            ("windows", "aarch64") => "hysteria-windows-arm64.exe",
            ("windows", "x86") => "hysteria-windows-386.exe",
            ("linux", "x86_64") => "hysteria-linux-amd64",
            ("linux", "aarch64") => "hysteria-linux-arm64",
            ("linux", "arm") => "hysteria-linux-arm",
            ("linux", "x86") => "hysteria-linux-386",
            ("linux", "riscv64") => "hysteria-linux-riscv64",
            ("macos", "x86_64") => "hysteria-darwin-amd64",
            ("macos", "aarch64") => "hysteria-darwin-arm64",
            ("freebsd", "x86_64") => "hysteria-freebsd-amd64",
            ("freebsd", "aarch64") => "hysteria-freebsd-arm64",
            (os, arch) => bail!("no Hysteria2 build for {os}/{arch}"),
        })
    }

    /// Xray-core zip asset name (`Xray-<friendly>.zip`).
    pub fn xray_zip(self) -> Result<&'static str> {
        Ok(match (self.os, self.arch) {
            ("windows", "x86_64") => "Xray-windows-64.zip",
            ("windows", "aarch64") => "Xray-windows-arm64-v8a.zip",
            ("windows", "x86") => "Xray-windows-32.zip",
            ("linux", "x86_64") => "Xray-linux-64.zip",
            ("linux", "aarch64") => "Xray-linux-arm64-v8a.zip",
            ("linux", "arm") => "Xray-linux-arm32-v7a.zip",
            ("linux", "x86") => "Xray-linux-32.zip",
            ("linux", "riscv64") => "Xray-linux-riscv64.zip",
            ("macos", "x86_64") => "Xray-macos-64.zip",
            ("macos", "aarch64") => "Xray-macos-arm64-v8a.zip",
            ("freebsd", "x86_64") => "Xray-freebsd-64.zip",
            ("freebsd", "aarch64") => "Xray-freebsd-arm64-v8a.zip",
            (os, arch) => bail!("no Xray-core build for {os}/{arch}"),
        })
    }
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

pub async fn ensure_core(
    http: &reqwest::Client,
    exliatycld: &Exliatycld,
    kind: CoreKind,
    force: bool,
    mirror_base: &str,
) -> Result<PathBuf> {
    match kind {
        CoreKind::Hy2 => ensure_hysteria(http, exliatycld, force, mirror_base).await,
        CoreKind::Xray => ensure_xray(http, exliatycld, force, mirror_base).await,
    }
}

async fn ensure_hysteria(http: &reqwest::Client, exliatycld: &Exliatycld, force: bool, mirror_base: &str) -> Result<PathBuf> {
    let dest = exliatycld.hy2_bin();
    let asset = HostTarget::detect().hysteria_asset()?;
    if !force && dest.is_file() {
        tracing::info!(path = %dest.display(), "hysteria2 already present");
        return Ok(dest);
    }
    tokio::fs::create_dir_all(exliatycld.hy2_dir()).await?;
    let bytes = fetch_bin("hy2", asset, "apernet/hysteria", http, mirror_base).await?;
    tokio::fs::write(&dest, &bytes).await?;
    make_executable(&dest)?;
    tokio::fs::write(exliatycld.hy2_dir().join("ASSET"), asset.as_bytes()).await?;
    tracing::info!(path = %dest.display(), bytes = bytes.len(), "hysteria2 ready");
    Ok(dest)
}

async fn ensure_xray(http: &reqwest::Client, exliatycld: &Exliatycld, force: bool, mirror_base: &str) -> Result<PathBuf> {
    let dest = exliatycld.xray_bin();
    let zip_name = HostTarget::detect().xray_zip()?;
    if !force && dest.is_file() {
        tracing::info!(path = %dest.display(), "xray-core already present");
        return Ok(dest);
    }
    tokio::fs::create_dir_all(exliatycld.xray_dir()).await?;
    let bytes = fetch_bin("xray", zip_name, "XTLS/Xray-core", http, mirror_base).await?;
    let dir = exliatycld.xray_dir();
    tokio::task::spawn_blocking(move || unzip_to(&bytes, &dir))
        .await
        .context("join unzip")??;
    if !dest.is_file() {
        bail!("zip {} extracted but {} is missing", zip_name, dest.display());
    }
    make_executable(&dest)?;
    tokio::fs::write(exliatycld.xray_dir().join("ASSET"), zip_name.as_bytes()).await?;
    tracing::info!(path = %dest.display(), "xray-core ready");
    Ok(dest)
}

/// Fetch a core binary: GitHub is tried first (it's the canonical source),
/// but only after confirming github.com is actually reachable - a plain
/// HEAD request capped at 5s. If that probe hangs/times out or fails
/// (github.com blocked on this network, which happens), we skip straight to
/// our own mirror instead of wasting the node's overall setup timeout on a
/// GitHub download that was never going to complete. If GitHub is reachable
/// but the actual asset download still fails for some other reason, we also
/// fall back to the mirror as a last resort.
///
/// `mirror_base` is `{control server origin}/clientapi/fetch-bin` - i.e. the
/// same server this node registers/pushes config through (`--server`,
/// defaulting to https://cliapi.exliatycl.online), not a separately
/// hardcoded host. Self-hosters pointing `--server` at their own control
/// server automatically get their own mirror for free.
async fn fetch_bin(
    mirror_core: &str,
    asset_name: &str,
    gh_repo: &str,
    http: &reqwest::Client,
    mirror_base: &str,
) -> Result<Vec<u8>> {
    if github_reachable(http).await {
        match try_github(http, gh_repo, asset_name).await {
            Ok(bytes) => return Ok(bytes),
            Err(e) => {
                tracing::warn!(error = %e, "GitHub download failed after reachability check, falling back to mirror");
            }
        }
    } else {
        tracing::warn!("github.com not reachable within 5s, using mirror directly");
    }

    let arch = HostTarget::detect().mirror_arch()?;
    let mirror_url = format!("{}/{mirror_core}/{arch}", mirror_base.trim_end_matches('/'));
    tracing::info!(url = %mirror_url, "downloading from mirror");
    download(http, &mirror_url).await
}

async fn try_github(http: &reqwest::Client, gh_repo: &str, asset_name: &str) -> Result<Vec<u8>> {
    let url = resolve_github_asset(http, gh_repo, asset_name).await?;
    tracing::info!(asset = asset_name, %url, "downloading from GitHub");
    download(http, &url).await
}

/// HEAD github.com with a hard 5s cap. Any failure (timeout, DNS, TLS block,
/// connection reset - all typical symptoms of a network that blocks GitHub)
/// is treated as "not reachable".
async fn github_reachable(http: &reqwest::Client) -> bool {
    http.head("https://github.com")
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .is_ok()
}

async fn resolve_github_asset(http: &reqwest::Client, repo: &str, asset: &str) -> Result<String> {
    let api = format!("https://api.github.com/repos/{repo}/releases?per_page=15");
    let releases: Vec<GhRelease> = http
        .get(&api)
        .header("User-Agent", UA)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("list releases {repo}"))?
        .error_for_status()
        .with_context(|| format!("GitHub API {repo}"))?
        .json()
        .await
        .context("parse GitHub releases")?;

    for rel in &releases {
        if let Some(a) = rel.assets.iter().find(|a| a.name == asset) {
            tracing::info!(tag = %rel.tag_name, asset, "matched GitHub asset");
            return Ok(a.browser_download_url.clone());
        }
    }

    // Fallback: GitHub's /latest/download redirect (works for Xray; Hy2 tags are app/v*).
    Ok(format!(
        "https://github.com/{repo}/releases/latest/download/{asset}"
    ))
}

async fn download(http: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let resp = http
        .get(url)
        .header("User-Agent", UA)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("download {url}"))?;
    let bytes = resp.bytes().await.context("download body")?;
    if bytes.len() < 1024 {
        bail!("download from {url} was suspiciously small ({} bytes)", bytes.len());
    }
    Ok(bytes.to_vec())
}

fn unzip_to(bytes: &[u8], dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).context("open zip")?;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).context("zip entry")?;
        let Some(rel) = file.enclosed_name() else {
            continue;
        };
        let out = dest.join(rel);
        if file.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut outfile = std::fs::File::create(&out)
            .with_context(|| format!("create {}", out.display()))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        outfile.write_all(&buf)?;
    }
    Ok(())
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)?;
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_linux_amd64() {
        let t = HostTarget {
            os: "linux",
            arch: "x86_64",
        };
        assert_eq!(t.hysteria_asset().unwrap(), "hysteria-linux-amd64");
        assert_eq!(t.xray_zip().unwrap(), "Xray-linux-64.zip");
    }

    #[test]
    fn known_windows_amd64() {
        let t = HostTarget {
            os: "windows",
            arch: "x86_64",
        };
        assert_eq!(t.hysteria_asset().unwrap(), "hysteria-windows-amd64.exe");
        assert_eq!(t.xray_zip().unwrap(), "Xray-windows-64.zip");
    }
}
