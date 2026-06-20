//! ModRetro's MRPatcher service — the first [`UpdateService`].
//!
//! Reverse-engineered from MRUpdater's "Cart Clinic": POST a genuine ModRetro
//! ROM and the service returns an IPS patch from *your* version to today's latest.
//! We present honestly as `throwback/<version>` with a random per-request id (the
//! service doesn't gate on the client version or fingerprint the id — verified by
//! probe), rather than impersonating MRUpdater.

use super::{Artifact, Identity, ServiceError, UpdateService, Upgrade};
use base64::Engine;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ENDPOINT: &str = "https://cbzr2zpag5.execute-api.us-east-1.amazonaws.com/default/MRPatcher";
const RESPONSE_VERSION: &str = "2.0";
const GAME_ID_SIZE: usize = 512; // bytes sent to the lightweight /game_id route
const TIMEOUT: Duration = Duration::from_secs(30);

pub struct ModRetroService;

impl ModRetroService {
    pub fn new() -> Self {
        ModRetroService
    }

    fn post(&self, url: &str, body: &[u8]) -> Result<Value, ServiceError> {
        let resp = ureq::post(url)
            .timeout(TIMEOUT)
            .set("Content-Type", "application/octet-stream")
            .set("X-MR-Client-Version", concat!("throwback/", env!("CARGO_PKG_VERSION")))
            .set("X-MR-Client-MAC", &random_id())
            .set("X-MR-Client-Platform-System", std::env::consts::OS)
            .set("X-MR-Client-Platform-Architecture", std::env::consts::ARCH)
            .set("X-MR-Client-Expected-Response-Version", RESPONSE_VERSION)
            .send_bytes(body);

        match resp {
            Ok(r) => {
                let text = r.into_string().map_err(|e| ServiceError::Network(e.to_string()))?;
                serde_json::from_str(&text).map_err(|e| ServiceError::Decode(e.to_string()))
            }
            Err(ureq::Error::Status(code, r)) => Err(ServiceError::Http {
                status: code,
                body: r.into_string().unwrap_or_default().chars().take(300).collect(),
            }),
            Err(ureq::Error::Transport(t)) => Err(ServiceError::Network(t.to_string())),
        }
    }
}

impl Default for ModRetroService {
    fn default() -> Self {
        Self::new()
    }
}

impl UpdateService for ModRetroService {
    fn name(&self) -> &str {
        "ModRetro"
    }

    fn identify(&self, rom: &[u8]) -> Result<Option<Identity>, ServiceError> {
        let head = &rom[..rom.len().min(GAME_ID_SIZE)];
        let v = self.post(&format!("{ENDPOINT}/game_id"), head)?;
        Ok(parse_identify(&v, self.name()))
    }

    fn fetch_upgrade(&self, rom: &[u8], id: &Identity) -> Result<Option<Upgrade>, ServiceError> {
        let v = self.post(ENDPOINT, rom)?;
        parse_upgrade(&v, &id.current_version)
    }
}

/// A random, opaque, per-request id (64 hex chars). Derived from time + pid + a
/// counter via splitmix64 — non-semantic, so no crypto RNG (or extra dep) needed,
/// and unlike MRUpdater's MAC hash it carries no device fingerprint.
fn random_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut seed = nanos
        ^ (std::process::id() as u64).rotate_left(32)
        ^ COUNTER.fetch_add(1, Ordering::Relaxed).rotate_left(17);
    let mut out = String::with_capacity(64);
    for _ in 0..4 {
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.push_str(&format!("{z:016x}"));
    }
    out
}

/// Pull a version-ish field as a string (the API sends strings, but tolerate numbers).
fn version_field(v: &Value, key: &str) -> Option<String> {
    match v.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// `/game_id` response → identity, or `None` if the service didn't recognize the ROM.
fn parse_identify(v: &Value, service: &str) -> Option<Identity> {
    if v.get("error").and_then(Value::as_str).is_some_and(|e| !e.is_empty()) {
        return None;
    }
    let title = match v.get("game_title").and_then(Value::as_str) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => return None,
    };
    Some(Identity {
        service: service.to_string(),
        title,
        current_version: version_field(v, "uploaded_version").unwrap_or_else(|| "?".to_string()),
    })
}

/// Full MRPatcher response → an [`Upgrade`], or `None` when already latest.
fn parse_upgrade(v: &Value, current: &str) -> Result<Option<Upgrade>, ServiceError> {
    if v.get("needs_updater").and_then(Value::as_bool) == Some(true) {
        return Err(ServiceError::ClientRejected(
            "ModRetro now requires its official updater for this game".to_string(),
        ));
    }
    if let Some(err) = v.get("error").and_then(Value::as_str)
        && !err.is_empty()
    {
        return Err(ServiceError::Service(err.to_string()));
    }
    // No patch => the uploaded ROM is already the latest version.
    let patch_b64 = match v.get("patch").and_then(Value::as_str) {
        Some(p) if !p.is_empty() => p,
        _ => return Ok(None),
    };
    let ips = base64::engine::general_purpose::STANDARD
        .decode(patch_b64)
        .map_err(|e| ServiceError::Decode(format!("patch base64: {e}")))?;
    Ok(Some(Upgrade {
        from_version: version_field(v, "uploaded_version").unwrap_or_else(|| current.to_string()),
        to_version: version_field(v, "latest_version").unwrap_or_else(|| "?".to_string()),
        artifact: Artifact::Patch(ips),
        changelog: v.get("changes").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from),
        save_compatible: v.get("save_compatible").and_then(Value::as_bool),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identify_recognized() {
        let v = json!({"game_title": "dragonyhm", "uploaded_version": "1.1", "error": null});
        let id = parse_identify(&v, "ModRetro").unwrap();
        assert_eq!(id.title, "dragonyhm");
        assert_eq!(id.current_version, "1.1");
        assert_eq!(id.service, "ModRetro");
    }

    #[test]
    fn identify_unrecognized() {
        assert!(parse_identify(&json!({"error": "unknown game"}), "ModRetro").is_none());
        assert!(parse_identify(&json!({"game_title": ""}), "ModRetro").is_none());
        assert!(parse_identify(&json!({}), "ModRetro").is_none());
    }

    #[test]
    fn upgrade_available() {
        // "QVRDSA==" is base64 for "ATCH" — stand-in bytes; parse_upgrade doesn't
        // validate the patch body (that happens later in Patch::load).
        let v = json!({
            "uploaded_version": "1.1", "latest_version": "1.2",
            "patch": "QVRDSA==", "changes": "fixed stuff", "save_compatible": false
        });
        let up = parse_upgrade(&v, "1.1").unwrap().unwrap();
        assert_eq!(up.from_version, "1.1");
        assert_eq!(up.to_version, "1.2");
        assert_eq!(up.save_compatible, Some(false));
        assert_eq!(up.changelog.as_deref(), Some("fixed stuff"));
        assert!(matches!(up.artifact, Artifact::Patch(ref b) if b == b"ATCH"));
    }

    #[test]
    fn upgrade_already_latest() {
        let v = json!({"uploaded_version": "1.2", "latest_version": "1.2"});
        assert!(parse_upgrade(&v, "1.2").unwrap().is_none());
    }

    #[test]
    fn upgrade_needs_updater_is_client_rejected() {
        let v = json!({"needs_updater": true});
        assert!(matches!(parse_upgrade(&v, "1.0"), Err(ServiceError::ClientRejected(_))));
    }

    #[test]
    fn upgrade_service_error() {
        let v = json!({"error": "corrupt rom"});
        assert!(matches!(parse_upgrade(&v, "1.0"), Err(ServiceError::Service(_))));
    }

    #[test]
    fn random_id_is_64_hex_and_varies() {
        let a = random_id();
        let b = random_id();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
