//! On-disk session cache at `~/.cache/tclac/session.json` (chmod 600).
//!
//! The temporary AWS credentials last ~1h; caching the whole [`Session`] plus
//! the selected [`Device`] lets one-shot CLI commands run without repeating the
//! multi-second login chain on every invocation. Validity is gated on the AWS
//! credential expiry; on any auth failure the caller should [`clear`] and retry.

use crate::auth::Session;
use crate::rest::Device;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
struct Cached {
    session: Session,
    device: Device,
}

fn cache_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "tclac")?;
    Some(dirs.cache_dir().join("session.json"))
}

fn now_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Return a cached (session, device) if present and the AWS creds have >60s left.
pub fn load_valid() -> Option<(Session, Device)> {
    let raw = std::fs::read_to_string(cache_path()?).ok()?;
    let c: Cached = serde_json::from_str(&raw).ok()?;
    let exp = c.session.aws.expiration?;
    if exp > now_secs() + 60 {
        Some((c.session, c.device))
    } else {
        None
    }
}

/// Best-effort write of the session + selected device to the cache.
pub fn save(session: &Session, device: &Device) {
    let Some(path) = cache_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let c = Cached {
        session: session.clone(),
        device: device.clone(),
    };
    if let Ok(raw) = serde_json::to_string(&c) {
        if std::fs::write(&path, raw).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
}

/// Remove the cache (call after an auth failure so the next run re-logs in).
pub fn clear() {
    if let Some(path) = cache_path() {
        let _ = std::fs::remove_file(path);
    }
}
