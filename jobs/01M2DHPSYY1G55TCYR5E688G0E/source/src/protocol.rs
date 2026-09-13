//! JSON shapes the control server (`../server`) actually sends and accepts.
//! Dotted keys are kept as map keys so we match the Node protocol byte-for-byte.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Deserialize)]
pub struct RegTicket {
    pub status: String,
    pub setup_code: String,
    pub token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreKind {
    Hy2,
    Xray,
}

impl CoreKind {
    pub fn prefix(self) -> &'static str {
        match self {
            CoreKind::Hy2 => "hyserver",
            CoreKind::Xray => "xray",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SetupConfig {
    pub core: CoreKind,
    pub port: u16,
    pub bw_ul: String,
    pub bw_dl: String,
    pub auth: String,
    pub obfs: Option<String>,
    pub protocol: Option<String>,
    pub transport: Option<String>,
    pub tls: Option<String>,
    pub allow_insecure: bool,
    pub req_id: Option<String>,
}

impl SetupConfig {
    pub fn from_server_json(v: &Value) -> anyhow::Result<Self> {
        let obj = v.as_object().ok_or_else(|| anyhow::anyhow!("setup payload is not an object"))?;
        let req_id = obj.get("reqId").and_then(|x| x.as_str()).map(str::to_owned);
        let allow_insecure = obj
            .get("allowInsecure")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if obj.contains_key("hyserver.port") {
            let port = parse_port(obj.get("hyserver.port"))?;
            let auth = string_field(obj, "authserver.auth")
                .or_else(|| string_field(obj, "auth"))
                .unwrap_or_default();
            return Ok(Self {
                core: CoreKind::Hy2,
                port,
                bw_ul: string_field(obj, "hyserver.bw.ul").unwrap_or_default(),
                bw_dl: string_field(obj, "hyserver.bw.dl").unwrap_or_default(),
                auth,
                obfs: string_field(obj, "authserver.obfs"),
                protocol: None,
                transport: None,
                tls: None,
                allow_insecure,
                req_id,
            });
        }

        if obj.contains_key("xray.port") {
            let port = parse_port(obj.get("xray.port"))?;
            let auth = string_field(obj, "xray.credential")
                .or_else(|| string_field(obj, "auth"))
                .unwrap_or_default();
            return Ok(Self {
                core: CoreKind::Xray,
                port,
                bw_ul: string_field(obj, "xray.bw.ul").unwrap_or_default(),
                bw_dl: string_field(obj, "xray.bw.dl").unwrap_or_default(),
                auth,
                obfs: None,
                protocol: string_field(obj, "xray.protocol"),
                transport: string_field(obj, "xray.transport"),
                tls: string_field(obj, "xray.tls"),
                allow_insecure,
                req_id,
            });
        }

        anyhow::bail!("unknown setup payload (neither hy2 nor xray keys)")
    }
}

fn string_field(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    })
}

fn parse_port(v: Option<&Value>) -> anyhow::Result<u16> {
    let v = v.ok_or_else(|| anyhow::anyhow!("missing port"))?;
    match v {
        Value::Number(n) => n
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .ok_or_else(|| anyhow::anyhow!("bad port number")),
        Value::String(s) => s
            .parse()
            .map_err(|_| anyhow::anyhow!("bad port string {s}")),
        _ => anyhow::bail!("port is not a number or string"),
    }
}

#[derive(Debug, Serialize)]
pub struct AliveSample {
    pub ram_used_kib: u64,
    pub ram_total_kib: u64,
    pub cpu_pct: f32,
    pub net_ul_bits: u64,
    pub net_dl_bits: u64,
}

pub fn alive_json(core: CoreKind, s: &AliveSample) -> Value {
    let p = core.prefix();
    serde_json::json!({
        "status": "ALIVE",
        format!("{p}.usage.ram.used"): s.ram_used_kib.to_string(),
        format!("{p}.usage.ram.total"): s.ram_total_kib.to_string(),
        format!("{p}.usage.cpu"): format!("{:.2}%", s.cpu_pct),
        format!("{p}.total.network.ul"): s.net_ul_bits.to_string(),
        format!("{p}.total.network.dl"): s.net_dl_bits.to_string(),
    })
}

/// A granular progress update sent to the dashboard while `first.setup.config`
/// is being applied, so the browser can show real stages (downloading
/// binaries, generating certs, starting up…) over SSE instead of a blind
/// 30s spinner. `stage` is a short machine-readable slug; `detail` is
/// optional human-readable context (e.g. a filename or error).
pub fn stage(stage: &str, detail: Option<&str>, req_id: Option<&str>) -> Value {
    let mut m = serde_json::json!({
        "status": "STAGE",
        "action": "setup.stage",
        "stage": stage,
    });
    let obj = m.as_object_mut().unwrap();
    if let Some(d) = detail {
        obj.insert("detail".into(), Value::String(d.into()));
    }
    if let Some(id) = req_id {
        obj.insert("reqId".into(), Value::String(id.into()));
    }
    m
}

pub fn setup_ok(core: CoreKind, url: &str, req_id: Option<&str>) -> Value {
    let key = match core {
        CoreKind::Hy2 => "hyserver.url",
        CoreKind::Xray => "xray.url",
    };
    let mut m = serde_json::json!({ "status": "OKAY", key: url });
    if let Some(id) = req_id {
        m.as_object_mut().unwrap().insert("reqId".into(), Value::String(id.into()));
    }
    m
}

pub fn ack(action: &str, req_id: Option<&str>, extra: Value) -> Value {
    let mut m = extra;
    let obj = m.as_object_mut().unwrap();
    obj.insert("status".into(), Value::String("OKAY".into()));
    obj.insert("action".into(), Value::String(action.into()));
    if let Some(id) = req_id {
        obj.insert("reqId".into(), Value::String(id.into()));
    }
    m
}

pub fn error_ack(action: &str, req_id: Option<&str>, error: &str) -> Value {
    let mut m = serde_json::json!({
        "status": "ERROR",
        "action": action,
        "error": error,
    });
    if let Some(id) = req_id {
        m.as_object_mut()
            .unwrap()
            .insert("reqId".into(), Value::String(id.into()));
    }
    m
}

pub fn incoming_action(v: &Value) -> Option<&str> {
    v.get("action").and_then(Value::as_str)
}

pub fn incoming_req_id(v: &Value) -> Option<&str> {
    v.get("reqId").and_then(Value::as_str)
}

pub fn incoming_user(v: &Value) -> Option<&str> {
    v.get("user").and_then(Value::as_str)
}

pub fn incoming_auth(v: &Value) -> Option<String> {
    v.as_object().and_then(|o| {
        string_field(o, "auth")
            .or_else(|| string_field(o, "credential"))
            .or_else(|| string_field(o, "xray.credential"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hy2_setup() {
        let v = serde_json::json!({
            "status": "OKAY",
            "action": "first.setup.config",
            "reqId": "aabbccdd",
            "hyserver.port": "443",
            "hyserver.bw.ul": "100 mbps",
            "hyserver.bw.dl": "100 mbps",
            "authserver.auth": "secret",
            "authserver.obfs": "obfs",
            "allowInsecure": true
        });
        let c = SetupConfig::from_server_json(&v).unwrap();
        assert_eq!(c.core, CoreKind::Hy2);
        assert_eq!(c.port, 443);
        assert_eq!(c.auth, "secret");
        assert_eq!(c.req_id.as_deref(), Some("aabbccdd"));
        assert!(c.allow_insecure);
    }

    #[test]
    fn alive_keys_match_server() {
        let v = alive_json(
            CoreKind::Hy2,
            &AliveSample {
                ram_used_kib: 10,
                ram_total_kib: 20,
                cpu_pct: 1.5,
                net_ul_bits: 3,
                net_dl_bits: 4,
            },
        );
        assert_eq!(v["status"], "ALIVE");
        assert_eq!(v["hyserver.usage.ram.used"], "10");
        assert_eq!(v["hyserver.total.network.dl"], "4");
    }
}
