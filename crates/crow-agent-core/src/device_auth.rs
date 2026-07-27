use crow_agent_protocol::{DeviceIdentity, HARNESS_PROTOCOL_V1};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

const PRODUCTION_API_ORIGIN: &str = "https://api.crowcompute.ai";

#[derive(Debug, Error)]
pub enum DeviceAuthorizationError {
    #[error("Crow API origin is invalid")]
    Origin,
    #[error("device label is invalid")]
    Label,
    #[error("Crow device authorization request failed")]
    Request(#[from] reqwest::Error),
    #[error("Crow device authorization is pending")]
    Pending,
    #[error("Crow device authorization expired")]
    Expired,
    #[error("Crow device authorization was rejected")]
    Rejected,
    #[error("Crow device authorization response is invalid")]
    Response,
}

#[derive(Debug, Serialize)]
struct StartRequest<'a> {
    protocol: &'static str,
    device_label: &'a str,
    platform: &'a str,
    signing_public_key: &'a str,
    encryption_public_key: &'a str,
}

#[derive(Debug, Deserialize)]
struct StartResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
    interval_seconds: u16,
}

pub struct DeviceAuthorizationSession {
    pub user_code: String,
    pub verification_uri: Url,
    pub expires_at: OffsetDateTime,
    pub interval: Duration,
    device_code: Zeroizing<String>,
}

impl std::fmt::Debug for DeviceAuthorizationSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceAuthorizationSession")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("expires_at", &self.expires_at)
            .field("interval", &self.interval)
            .field("device_code", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Serialize)]
struct ExchangeRequest<'a> {
    protocol: &'static str,
    device_code: &'a str,
    signing_public_key: String,
    device_proof: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    device_id: Uuid,
    access_token: String,
    refresh_token: String,
    #[serde(with = "time::serde::rfc3339")]
    access_expires_at: OffsetDateTime,
}

pub struct DeviceTokens {
    pub device_id: Uuid,
    pub access_token: Zeroizing<String>,
    pub refresh_token: Zeroizing<String>,
    pub access_expires_at: OffsetDateTime,
}

impl std::fmt::Debug for DeviceTokens {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceTokens")
            .field("device_id", &self.device_id)
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("access_expires_at", &self.access_expires_at)
            .finish()
    }
}

#[derive(Debug, Serialize)]
struct RotateRequest<'a> {
    protocol: &'static str,
    refresh_token: &'a str,
    signing_public_key: String,
    device_proof: String,
}

#[derive(Debug, Clone)]
pub struct DeviceAuthorizationClient {
    origin: Url,
    client: Client,
}

impl DeviceAuthorizationClient {
    pub fn production() -> Result<Self, DeviceAuthorizationError> {
        Self::new(PRODUCTION_API_ORIGIN)
    }

    pub fn new(api_origin: &str) -> Result<Self, DeviceAuthorizationError> {
        let origin = Url::parse(api_origin).map_err(|_| DeviceAuthorizationError::Origin)?;
        let host = origin.host_str().ok_or(DeviceAuthorizationError::Origin)?;
        if origin.scheme() != "https"
            || !(host == "crowcompute.ai" || host.ends_with(".crowcompute.ai"))
            || origin.cannot_be_a_base()
        {
            return Err(DeviceAuthorizationError::Origin);
        }
        Ok(Self {
            origin,
            client: Client::builder()
                .https_only(true)
                .timeout(Duration::from_secs(15))
                .build()?,
        })
    }

    pub async fn start(
        &self,
        device_label: &str,
        platform: &str,
        identity: &DeviceIdentity,
        encryption_public_key: &str,
    ) -> Result<DeviceAuthorizationSession, DeviceAuthorizationError> {
        let label = device_label.trim();
        if label.is_empty() || label.len() > 80 || platform.trim().is_empty() {
            return Err(DeviceAuthorizationError::Label);
        }
        let endpoint = self
            .origin
            .join("/api/v1/harness/device-authorizations")
            .map_err(|_| DeviceAuthorizationError::Origin)?;
        let response = self
            .client
            .post(endpoint)
            .json(&StartRequest {
                protocol: HARNESS_PROTOCOL_V1,
                device_label: label,
                platform,
                signing_public_key: &identity.public_key(),
                encryption_public_key,
            })
            .send()
            .await?
            .error_for_status()?
            .json::<StartResponse>()
            .await?;
        let verification_uri = Url::parse(&response.verification_uri)
            .map_err(|_| DeviceAuthorizationError::Response)?;
        if verification_uri.scheme() != "https"
            || verification_uri.host_str() != Some("crowcompute.ai")
            || response.device_code.is_empty()
            || response.user_code.is_empty()
            || response.interval_seconds == 0
        {
            return Err(DeviceAuthorizationError::Response);
        }
        Ok(DeviceAuthorizationSession {
            user_code: response.user_code,
            verification_uri,
            expires_at: response.expires_at,
            interval: Duration::from_secs(u64::from(response.interval_seconds)),
            device_code: Zeroizing::new(response.device_code),
        })
    }

    pub async fn exchange(
        &self,
        session: &DeviceAuthorizationSession,
        identity: &DeviceIdentity,
    ) -> Result<DeviceTokens, DeviceAuthorizationError> {
        if OffsetDateTime::now_utc() >= session.expires_at {
            return Err(DeviceAuthorizationError::Expired);
        }
        let endpoint = self
            .origin
            .join("/api/v1/harness/device-authorizations/exchange")
            .map_err(|_| DeviceAuthorizationError::Origin)?;
        let response = self
            .client
            .post(endpoint)
            .json(&ExchangeRequest {
                protocol: HARNESS_PROTOCOL_V1,
                device_code: session.device_code.as_str(),
                signing_public_key: identity.public_key(),
                device_proof: identity.sign_bytes(session.device_code.as_bytes()),
            })
            .send()
            .await?;
        match response.status() {
            StatusCode::ACCEPTED => Err(DeviceAuthorizationError::Pending),
            StatusCode::GONE => Err(DeviceAuthorizationError::Expired),
            StatusCode::FORBIDDEN => Err(DeviceAuthorizationError::Rejected),
            status if status.is_success() => {
                let tokens = response.json::<TokenResponse>().await?;
                if tokens.access_token.is_empty() || tokens.refresh_token.is_empty() {
                    return Err(DeviceAuthorizationError::Response);
                }
                Ok(DeviceTokens {
                    device_id: tokens.device_id,
                    access_token: Zeroizing::new(tokens.access_token),
                    refresh_token: Zeroizing::new(tokens.refresh_token),
                    access_expires_at: tokens.access_expires_at,
                })
            }
            _ => Err(DeviceAuthorizationError::Response),
        }
    }

    pub async fn rotate(
        &self,
        refresh_token: &Zeroizing<String>,
        identity: &DeviceIdentity,
    ) -> Result<DeviceTokens, DeviceAuthorizationError> {
        let endpoint = self
            .origin
            .join("/api/v1/harness/device-tokens/rotate")
            .map_err(|_| DeviceAuthorizationError::Origin)?;
        let response = self
            .client
            .post(endpoint)
            .json(&RotateRequest {
                protocol: HARNESS_PROTOCOL_V1,
                refresh_token: refresh_token.as_str(),
                signing_public_key: identity.public_key(),
                device_proof: identity.sign_bytes(refresh_token.as_bytes()),
            })
            .send()
            .await?
            .error_for_status()?
            .json::<TokenResponse>()
            .await?;
        if response.access_token.is_empty() || response.refresh_token.is_empty() {
            return Err(DeviceAuthorizationError::Response);
        }
        Ok(DeviceTokens {
            device_id: response.device_id,
            access_token: Zeroizing::new(response.access_token),
            refresh_token: Zeroizing::new(response.refresh_token),
            access_expires_at: response.access_expires_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_rejects_non_crow_or_plaintext_origins() {
        assert!(DeviceAuthorizationClient::new("http://api.crowcompute.ai").is_err());
        assert!(DeviceAuthorizationClient::new("https://crowcompute.ai.example.com").is_err());
        assert!(DeviceAuthorizationClient::new("https://api.crowcompute.ai").is_ok());
    }

    #[test]
    fn session_debug_redacts_device_code() {
        let session = DeviceAuthorizationSession {
            user_code: "ABCD-EFGH".into(),
            verification_uri: Url::parse("https://crowcompute.ai/device")
                .unwrap_or_else(|_| unreachable!("static URL is valid")),
            expires_at: OffsetDateTime::UNIX_EPOCH,
            interval: Duration::from_secs(5),
            device_code: Zeroizing::new("do-not-log-me".into()),
        };
        assert!(!format!("{session:?}").contains("do-not-log-me"));
    }
}
