//! TCL Home cloud authentication chain.
//!
//! Reverse-engineered flow (ported from the `nemesa/ha-tcl-home-unofficial-integration`
//! Python integration and cross-checked against `DavidIlie/tcl-home-ac`):
//!
//!   login            -> access token (`token`), refresh token, username, country
//!   cloud_url_get    -> per-account regional endpoints + AWS region
//!   refresh_tokens   -> SaaS token (signs REST) + Cognito token + IoT endpoint
//!   GetCredentials   -> temporary AWS credentials (via Cognito Identity)
//!
//! The resulting [`Session`] carries everything the REST and IoT layers need.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const LOGIN_URL: &str = "https://pa.account.tcl.com/account/login?clientId=54148614";
const CLOUD_URL_GET: &str = "https://prod-center.aws.tcljd.com/v3/global/cloud_url_get";
const APP_ID: &str = "wx6e1af3fa84fbe523";
const COGNITO_UA: &str = "aws-sdk-android/2.22.6 Linux/6.1.23-android14-4-00257-g7e35917775b8-ab9964412 Dalvik/2.1.0/0 en_US";

/// Everything needed to talk to the TCL cloud + AWS IoT for one logged-in account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Logged-in account name (kept for diagnostics / future per-user calls).
    #[allow(dead_code)]
    pub username: String,
    pub country: String,
    /// SaaS token — signs all authenticated TCL REST calls.
    pub saas_token: String,
    /// Base URL for device REST endpoints (e.g. https://prod-eu.aws.tcljd.com).
    pub device_url: String,
    /// AWS region for Cognito + IoT (e.g. eu-central-1).
    pub aws_region: String,
    /// AWS IoT data-plane endpoint host (account-specific ATS host).
    pub iot_endpoint: Option<String>,
    /// Temporary AWS credentials for signing IoT data-plane requests.
    pub aws: AwsCredentials,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_key: String,
    pub session_token: String,
    /// Unix epoch seconds when these credentials expire (best effort).
    /// Currently we re-auth reactively on error rather than pre-emptively.
    #[allow(dead_code)]
    pub expiration: Option<u64>,
}

/// Run the full authentication chain and return a ready-to-use [`Session`].
pub async fn login(client: &reqwest::Client, username: &str, password: &str) -> Result<Session> {
    let login = do_login(client, username, password).await?;
    let urls = get_cloud_urls(client, &login.username, &login.token).await?;
    let tokens = refresh_tokens(client, &urls.cloud_url, &login.username, &login.token).await?;
    let aws = get_aws_credentials(client, &urls.cloud_region, &tokens.cognito_token).await?;

    Ok(Session {
        username: login.username,
        country: login.country,
        saas_token: tokens.saas_token,
        device_url: urls.device_url,
        aws_region: urls.cloud_region,
        iot_endpoint: tokens.mqtt_endpoint,
        aws,
    })
}

// ---------------------------------------------------------------------------
// Stage 1: login
// ---------------------------------------------------------------------------

struct LoginResult {
    token: String,
    username: String,
    country: String,
}

async fn do_login(client: &reqwest::Client, username: &str, password: &str) -> Result<LoginResult> {
    let pw_md5 = format!("{:x}", md5::compute(password.as_bytes()));
    let body = json!({
        "equipment": 2,
        "password": pw_md5,
        "osType": 1,
        "username": username,
        "clientVersion": "4.8.1",
        "osVersion": "6.0",
        "deviceModel": "AndroidAndroid SDK built for x86",
        "captchaRule": 2,
        "channel": "app",
    });

    let resp = client
        .post(LOGIN_URL)
        .header("th_platform", "android")
        .header("th_version", "4.8.1")
        .header("th_appbulid", "830") // literal misspelling required by the API
        .header("user-agent", "Android")
        .header("content-type", "application/json; charset=UTF-8")
        .json(&body)
        .send()
        .await
        .context("login request failed")?;

    let v: Value = json_or_err(resp, "login").await?;

    let status = v.get("status").and_then(|s| s.as_i64()).unwrap_or(0);
    if status != 1 {
        // status 3 = wrong username/password. Surface any message the server
        // sent, falling back to the raw payload for unfamiliar error codes.
        let msg = v
            .get("msg")
            .or_else(|| v.get("message"))
            .or_else(|| v.get("errorMessage"))
            .and_then(|m| m.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| truncate(&v.to_string(), 300));
        let hint = if status == 3 {
            " (check email/password)"
        } else {
            ""
        };
        return Err(anyhow!("login rejected (status={status}){hint}: {msg}"));
    }

    let token = get_str(&v, &["token"]).context("login response missing `token`")?;
    let username = get_str(&v, &["user", "username"]).unwrap_or_else(|_| username.to_string());
    let country = get_str(&v, &["user", "country_abbr"])
        .or_else(|_| get_str(&v, &["user", "countryAbbr"]))
        .unwrap_or_default();

    Ok(LoginResult {
        token,
        username,
        country,
    })
}

// ---------------------------------------------------------------------------
// Stage 2: cloud_url_get (region + endpoint discovery)
// ---------------------------------------------------------------------------

struct CloudUrls {
    cloud_url: String,
    device_url: String,
    cloud_region: String,
}

async fn get_cloud_urls(
    client: &reqwest::Client,
    username: &str,
    sso_token: &str,
) -> Result<CloudUrls> {
    let body = json!({ "ssoId": username, "ssoToken": sso_token });
    let resp = client
        .post(CLOUD_URL_GET)
        .header("user-agent", "Android")
        .header("content-type", "application/json; charset=UTF-8")
        .json(&body)
        .send()
        .await
        .context("cloud_url_get request failed")?;

    let v: Value = json_or_err(resp, "cloud_url_get").await?;
    let data = v.get("data").cloned().unwrap_or(Value::Null);

    let cloud_url =
        get_str(&data, &["cloud_url"]).context("cloud_url_get response missing data.cloud_url")?;
    let device_url = get_str(&data, &["device_url"])
        .context("cloud_url_get response missing data.device_url")?;
    let cloud_region = get_str(&data, &["cloud_region"])
        .context("cloud_url_get response missing data.cloud_region")?;

    Ok(CloudUrls {
        cloud_url: cloud_url.trim_end_matches('/').to_string(),
        device_url: device_url.trim_end_matches('/').to_string(),
        cloud_region,
    })
}

// ---------------------------------------------------------------------------
// Stage 3: refresh_tokens (SaaS + Cognito tokens)
// ---------------------------------------------------------------------------

struct Tokens {
    saas_token: String,
    cognito_token: String,
    mqtt_endpoint: Option<String>,
}

async fn refresh_tokens(
    client: &reqwest::Client,
    cloud_url: &str,
    username: &str,
    sso_token: &str,
) -> Result<Tokens> {
    let url = format!("{cloud_url}/v3/auth/refresh_tokens");
    let body = json!({ "userId": username, "ssoToken": sso_token, "appId": APP_ID });
    let resp = client
        .post(&url)
        .header("user-agent", "Android")
        .header("content-type", "application/json; charset=UTF-8")
        // NB: no manual accept-encoding — reqwest's gzip feature advertises and
        // decompresses automatically only when it owns that header.
        .json(&body)
        .send()
        .await
        .context("refresh_tokens request failed")?;

    let v: Value = json_or_err(resp, "refresh_tokens").await?;
    let data = v.get("data").cloned().unwrap_or(Value::Null);

    let saas_token = get_str(&data, &["saas_token"])
        .or_else(|_| get_str(&data, &["saasToken"]))
        .context("refresh_tokens response missing saas_token")?;
    let cognito_token = get_str(&data, &["cognito_token"])
        .or_else(|_| get_str(&data, &["cognitoToken"]))
        .context("refresh_tokens response missing cognito_token")?;
    let mqtt_endpoint = get_str(&data, &["mqtt_endpoint"])
        .or_else(|_| get_str(&data, &["mqttEndpoint"]))
        .ok()
        .filter(|s| !s.is_empty());

    Ok(Tokens {
        saas_token,
        cognito_token,
        mqtt_endpoint,
    })
}

// ---------------------------------------------------------------------------
// Stage 4: AWS credentials via Cognito Identity (enhanced/developer flow)
// ---------------------------------------------------------------------------

async fn get_aws_credentials(
    client: &reqwest::Client,
    region: &str,
    cognito_token: &str,
) -> Result<AwsCredentials> {
    let identity_id =
        jwt_claim(cognito_token, "sub").context("could not read `sub` claim from cognito_token")?;

    let url = format!("https://cognito-identity.{region}.amazonaws.com/");
    let body = json!({
        "IdentityId": identity_id,
        "Logins": { "cognito-identity.amazonaws.com": cognito_token },
    });

    let resp = client
        .post(&url)
        .header("user-agent", COGNITO_UA)
        .header(
            "x-amz-target",
            "AWSCognitoIdentityService.GetCredentialsForIdentity",
        )
        .header("content-type", "application/x-amz-json-1.1")
        .json(&body)
        .send()
        .await
        .context("Cognito GetCredentialsForIdentity request failed")?;

    let v: Value = json_or_err(resp, "GetCredentialsForIdentity").await?;
    let creds = v
        .get("Credentials")
        .context("Cognito response missing Credentials")?;

    let access_key_id = get_str(creds, &["AccessKeyId"])?;
    let secret_key = get_str(creds, &["SecretKey"])?;
    let session_token = get_str(creds, &["SessionToken"])?;
    let expiration = creds
        .get("Expiration")
        .and_then(|e| e.as_f64())
        .map(|f| f as u64);

    Ok(AwsCredentials {
        access_key_id,
        secret_key,
        session_token,
        expiration,
    })
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Parse a JSON response, surfacing HTTP + body detail on non-success.
async fn json_or_err(resp: reqwest::Response, stage: &str) -> Result<Value> {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!(
            "{stage} returned HTTP {status}: {}",
            truncate(&text, 400)
        ));
    }
    serde_json::from_str(&text)
        .with_context(|| format!("{stage} returned non-JSON body: {}", truncate(&text, 400)))
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

/// Pull a nested string out of a JSON value by a path of keys.
fn get_str(v: &Value, path: &[&str]) -> Result<String> {
    let mut cur = v;
    for key in path {
        cur = cur
            .get(key)
            .ok_or_else(|| anyhow!("missing field `{}`", path.join(".")))?;
    }
    cur.as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("field `{}` is not a string", path.join(".")))
}

/// Decode a JWT payload (no signature verification) and return a string claim.
fn jwt_claim(token: &str, claim: &str) -> Result<String> {
    use base64::Engine;
    let payload_b64 = token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("malformed JWT"))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .context("JWT payload is not valid base64url")?;
    let payload: Value = serde_json::from_slice(&bytes).context("JWT payload is not JSON")?;
    payload
        .get(claim)
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("JWT missing claim `{claim}`"))
}
