use anyhow::{Context, Result};
use std::path::Path;
use time::{Duration, OffsetDateTime};

/// Self-signed cert for Hysteria2 / Xray TLS when the dashboard sent allowInsecure
/// or no operator certs were placed in `exliatycld/certs/`.
///
/// Covers every protocol that terminates TLS itself on this node
/// (VLESS/VMess/Shadowsocks/Trojan over Xray's `tls` mode, and Hysteria2,
/// which is TLS-only) - one cert/key pair, reused, valid for 10 years so it
/// doesn't need silent re-issuance under load.
pub fn ensure_self_signed(cert: &Path, key: &Path, public_host: &str) -> Result<()> {
    if cert.is_file() && key.is_file() {
        return Ok(());
    }
    if let Some(parent) = cert.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut params = rcgen::CertificateParams::new(vec![
        public_host.to_string(),
        "localhost".into(),
        "127.0.0.1".into(),
    ])
    .context("cert SAN")?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, public_host);
    params.is_ca = rcgen::IsCa::NoCa;
    // Explicit 10-year validity window (rcgen's default is much longer/
    // implementation-defined; pin it down so this is a *known* 10y cert).
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::days(1); // small clock-skew cushion
    params.not_after = now + Duration::days(365 * 10);

    let keypair = rcgen::KeyPair::generate().context("generate TLS key")?;
    let cert_obj = params.self_signed(&keypair).context("self-sign")?;

    std::fs::write(cert, cert_obj.pem()).with_context(|| format!("write {}", cert.display()))?;
    std::fs::write(key, keypair.serialize_pem()).with_context(|| format!("write {}", key.display()))?;
    tracing::info!(cert = %cert.display(), "wrote self-signed TLS cert");
    Ok(())
}
