//! Authenticated TCL REST calls (device discovery).
//!
//! Every call is signed with `md5(timestamp + nonce + saas_token)` carried in the
//! `timestamp` / `nonce` / `sign` headers, with the SaaS token in `accesstoken`.

use anyhow::{anyhow, Context, Result};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::Session;

/// A device (AC) as returned by the account's `get_things` listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    /// Cloud id == AWS IoT thingName; used directly for shadow read/write.
    pub device_id: String,
    pub nick_name: String,
    /// e.g. "Split AC" — determines the command dialect.
    pub device_name: String,
    pub is_online: bool,
    pub firmware_version: String,
}

/// A random 16-char `[a-z0-9]` nonce, matching the reference client.
fn nonce() -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..16)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

/// Current time in milliseconds since the Unix epoch, as a string.
fn timestamp_ms() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    ms.to_string()
}

/// Attach the full signed header set for an authenticated REST call.
fn sign(req: reqwest::RequestBuilder, session: &Session) -> reqwest::RequestBuilder {
    let ts = timestamp_ms();
    let nonce = nonce();
    let sign = format!(
        "{:x}",
        md5::compute(format!("{ts}{nonce}{}", session.saas_token).as_bytes())
    );
    req.header("platform", "android")
        .header("appversion", "5.4.1")
        .header("thomeversion", "4.8.1")
        .header("accesstoken", &session.saas_token)
        .header("countrycode", &session.country)
        .header("accept-language", "en")
        .header("timestamp", ts)
        .header("nonce", nonce)
        .header("sign", sign)
        .header("user-agent", "Android")
        .header("content-type", "application/json; charset=UTF-8")
    // NB: no manual accept-encoding — let reqwest's gzip feature handle it so
    // responses are auto-decompressed.
}

/// List the ACs / devices attached to the logged-in account.
pub async fn get_things(client: &reqwest::Client, session: &Session) -> Result<Vec<Device>> {
    let url = format!("{}/v3/user/get_things", session.device_url);
    let req = sign(client.post(&url).json(&json!({})), session);

    let resp = req.send().await.context("get_things request failed")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("get_things returned HTTP {status}: {}", text));
    }
    let v: Value = serde_json::from_str(&text)
        .with_context(|| format!("get_things returned non-JSON: {text}"))?;

    let data = v
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| anyhow!("get_things response has no `data` array: {v}"))?;

    let mut devices = Vec::new();
    for item in data {
        let device_id = str_field(item, &["device_id", "deviceId"]).unwrap_or_default();
        if device_id.is_empty() {
            continue;
        }
        devices.push(Device {
            device_id,
            nick_name: str_field(item, &["nick_name", "nickName"]).unwrap_or_default(),
            device_name: str_field(item, &["device_name", "deviceName"]).unwrap_or_default(),
            is_online: int_field(item, &["is_online", "isOnline"]).unwrap_or(0) != 0,
            firmware_version: str_field(item, &["firmware_version", "firmwareVersion"])
                .unwrap_or_default(),
        });
    }
    Ok(devices)
}

/// A signed authenticated GET, returning (HTTP status, raw body). Used for the
/// energy / work-time endpoints, which are GETs rather than POSTs.
pub async fn signed_get(
    client: &reqwest::Client,
    session: &Session,
    url: &str,
) -> Result<(u16, String)> {
    let resp = sign(client.get(url), session)
        .send()
        .await
        .with_context(|| format!("GET {url} failed"))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    Ok((status, text))
}

fn str_field(v: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

fn int_field(v: &Value, keys: &[&str]) -> Option<i64> {
    for k in keys {
        if let Some(n) = v.get(k) {
            if let Some(i) = n.as_i64() {
                return Some(i);
            }
            if let Some(b) = n.as_bool() {
                return Some(b as i64);
            }
            if let Some(s) = n.as_str() {
                if let Ok(i) = s.parse::<i64>() {
                    return Some(i);
                }
            }
        }
    }
    None
}
