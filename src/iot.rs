//! AWS IoT device-shadow access.
//!
//! Reading state is `GetThingShadow`. Sending a command mirrors the phone app:
//! **publish** the desired-state document to `$aws/things/{id}/shadow/update`.
//! (The Cognito identity is granted `iot:Publish` for this topic but NOT
//! `iot:UpdateThingShadow`, so the direct shadow-update API returns 403 —
//! publishing is the authorized path.) SigV4 is handled by the SDK using the
//! temporary Cognito credentials.

use anyhow::{anyhow, Context, Result};
use aws_sdk_iotdataplane::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_iotdataplane::error::ProvideErrorMetadata;
use aws_sdk_iotdataplane::primitives::Blob;
use aws_sdk_iotdataplane::Client;
use aws_smithy_types::timeout::TimeoutConfig;
use serde_json::{json, Value};
use std::time::Duration;

/// Thin, cloneable wrapper around the IoT data-plane client bound to one thing.
#[derive(Clone)]
pub struct ShadowClient {
    client: Client,
    thing_name: String,
}

impl ShadowClient {
    pub fn new(session: &crate::auth::Session, thing_name: &str) -> Self {
        let creds = Credentials::new(
            session.aws.access_key_id.clone(),
            session.aws.secret_key.clone(),
            Some(session.aws.session_token.clone()),
            None, // expiry is enforced by re-login, not the SDK
            "tcl-cognito",
        );

        // The AWS SDK's default connect timeout is 3.1s. TCL's IoT endpoint is
        // IPv6-only on some networks and can take ~3s to connect, tripping that
        // default. Give it generous headroom.
        let timeouts = TimeoutConfig::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(20))
            .operation_attempt_timeout(Duration::from_secs(30))
            .build();

        let mut builder = aws_sdk_iotdataplane::config::Builder::default()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(session.aws_region.clone()))
            .timeout_config(timeouts)
            .credentials_provider(creds);

        // TCL may hand the endpoint back with a scheme and/or an MQTT port
        // (:8883); the HTTPS data-plane API needs a bare host on 443.
        if let Some(ep) = &session.iot_endpoint {
            if let Some(host) = normalise_host(ep) {
                builder = builder.endpoint_url(format!("https://{host}"));
            }
        }

        Self {
            client: Client::from_conf(builder.build()),
            thing_name: thing_name.to_string(),
        }
    }

    /// Fetch the full shadow document (`state.reported`, `state.desired`, …).
    pub async fn get_shadow(&self) -> Result<Value> {
        let out = self
            .client
            .get_thing_shadow()
            .thing_name(&self.thing_name)
            .send()
            .await
            .map_err(|e| anyhow!("read state: {}", service_error(&e)))?;

        let payload = out
            .payload()
            .ok_or_else(|| anyhow!("GetThingShadow returned empty payload"))?;
        let v: Value = serde_json::from_slice(payload.as_ref())
            .context("shadow payload was not valid JSON")?;
        Ok(v)
    }

    /// Send a desired-state patch by publishing to the shadow update topic
    /// (the authorized, app-equivalent path).
    pub async fn set_desired(&self, desired: Value) -> Result<()> {
        let client_token = format!("mobile_{}", unix_seconds());
        let doc = json!({ "state": { "desired": desired }, "clientToken": client_token });
        let bytes = serde_json::to_vec(&doc)?;
        let topic = format!("$aws/things/{}/shadow/update", self.thing_name);

        self.client
            .publish()
            .topic(topic)
            .qos(1)
            .payload(Blob::new(bytes))
            .send()
            .await
            .map_err(|e| anyhow!("publish command: {}", service_error(&e)))?;
        Ok(())
    }
}

fn unix_seconds() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Strip scheme, path, and port from an endpoint string, leaving a bare host.
fn normalise_host(ep: &str) -> Option<String> {
    let host = ep
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("wss://")
        .trim_start_matches("ssl://")
        .trim_start_matches("tls://")
        .trim_start_matches("mqtt://")
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Concise, human-readable rendering of an SdkError — code + message only, not
/// the multi-line Debug dump of the whole error chain.
fn service_error<E>(e: &aws_sdk_iotdataplane::error::SdkError<E>) -> String
where
    E: ProvideErrorMetadata + std::error::Error,
{
    use aws_sdk_iotdataplane::error::SdkError;
    match e {
        SdkError::ServiceError(se) => {
            let inner = se.err();
            match (inner.code(), inner.message()) {
                (Some(code), Some(msg)) if !msg.is_empty() => format!("{code}: {msg}"),
                (Some(code), _) => code.to_string(),
                _ => "service error".to_string(),
            }
        }
        SdkError::TimeoutError(_) => "timed out".to_string(),
        SdkError::DispatchFailure(_) => "connection failure (network/DNS)".to_string(),
        SdkError::ResponseError(_) => "unexpected response".to_string(),
        SdkError::ConstructionFailure(_) => "request construction failed".to_string(),
        _ => "unknown error".to_string(),
    }
}
