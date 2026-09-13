use std::path::PathBuf;

/// On-disk layout (all relative to `--exliatycld`, default `./exliatycld`):
///
/// ```text
/// exliatycld/
///   state.json
///   users.json
///   certs/server.crt
///   certs/server.key
///   hysteria2/hysteria[.exe]
///   hysteria2/config.yaml
///   xray/xray[.exe]
///   xray/config.json
///   xray/geoip.dat …
/// ```
#[derive(Debug, Clone)]
pub struct Exliatycld {
    pub root: PathBuf,
}

impl Exliatycld {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.json")
    }

    pub fn users_file(&self) -> PathBuf {
        self.root.join("users.json")
    }

    pub fn certs_dir(&self) -> PathBuf {
        self.root.join("certs")
    }

    pub fn cert_pem(&self) -> PathBuf {
        absolute(self.certs_dir().join("server.crt"))
    }

    pub fn key_pem(&self) -> PathBuf {
        absolute(self.certs_dir().join("server.key"))
    }

    pub fn hy2_dir(&self) -> PathBuf {
        self.root.join("hysteria2")
    }

    pub fn hy2_bin(&self) -> PathBuf {
        self.hy2_dir().join(exe("hysteria"))
    }

    pub fn hy2_config(&self) -> PathBuf {
        self.hy2_dir().join("config.yaml")
    }

    pub fn xray_dir(&self) -> PathBuf {
        self.root.join("xray")
    }

    pub fn xray_bin(&self) -> PathBuf {
        self.xray_dir().join(exe("xray"))
    }

    pub fn xray_config(&self) -> PathBuf {
        self.xray_dir().join("config.json")
    }

    pub fn reality_file(&self) -> PathBuf {
        self.xray_dir().join("reality.json")
    }

    pub async fn ensure_dirs(&self) -> std::io::Result<()> {
        tokio::fs::create_dir_all(&self.root).await?;
        tokio::fs::create_dir_all(self.certs_dir()).await?;
        tokio::fs::create_dir_all(self.hy2_dir()).await?;
        tokio::fs::create_dir_all(self.xray_dir()).await?;
        Ok(())
    }
}

pub fn exe(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// Turn a (possibly relative) path into an absolute one, purely lexically — no
/// filesystem access, so it works even if the path doesn't exist yet. Used for
/// any path we embed as a *string* inside a downstream core's config file
/// (cert/key, etc.), since some cores (Xray-core) resolve relative paths found
/// inside their config relative to the config file's own directory rather than
/// the process's working directory, which double-nests a root-relative path.
fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}
