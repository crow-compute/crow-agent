use crow_agent_protocol::{AgentVersionEnvelopeV1, RunEventEnvelopeV1};
use reqwest::{
    Client, StatusCode,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};
use thiserror::Error;
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Error)]
pub enum HarnessApiError {
    #[error("Crow API origin is invalid")]
    Origin,
    #[error("device authorization material is invalid")]
    Authorization,
    #[error("Crow harness request failed")]
    Request(#[from] reqwest::Error),
    #[error("Crow harness request failed closed with HTTP {0}")]
    Status(u16),
    #[error("Crow harness response is invalid")]
    Response,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HarnessRunV1 {
    pub id: Uuid,
    pub arena_id: Uuid,
    pub device_id: Uuid,
    pub agent_version_id: Uuid,
    pub client_release: String,
    pub status: String,
    pub next_sequence: u64,
    pub last_event_hash: String,
    pub disqualification_reason: Option<String>,
    pub handoff_snapshot: Option<Value>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub stopped_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StartHarnessRunV1 {
    pub arena_id: Uuid,
    pub agent_version_id: Uuid,
    pub execution_account: String,
    pub client_release: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff_snapshot: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct StartHarnessRunResponse {
    run: HarnessRunV1,
    lease_token: String,
    #[serde(with = "time::serde::rfc3339")]
    lease_expires_at: OffsetDateTime,
}

#[derive(Debug)]
pub struct StartedHarnessRunV1 {
    pub run: HarnessRunV1,
    pub lease_token: Zeroizing<String>,
    pub lease_expires_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
struct RenewLeaseRequest<'a> {
    lease_token: &'a str,
}

#[derive(Debug, Deserialize)]
struct RenewLeaseResponse {
    #[serde(with = "time::serde::rfc3339")]
    lease_expires_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
struct AppendEventResponse {
    event_id: Uuid,
    server_receipt: String,
}

#[derive(Debug, Deserialize)]
struct AgentVersionListResponse {
    versions: Vec<AgentVersionEnvelopeV1>,
}

#[derive(Debug, Serialize)]
struct SwitchRunStrategyRequest {
    agent_version_id: Uuid,
}

#[derive(Clone)]
pub struct RotatingAccessToken {
    inner: Arc<RwLock<Zeroizing<String>>>,
}

impl std::fmt::Debug for RotatingAccessToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("RotatingAccessToken")
            .field(&"[REDACTED]")
            .finish()
    }
}

impl RotatingAccessToken {
    pub fn new(token: &str) -> Result<Self, HarnessApiError> {
        if token.trim().is_empty() {
            return Err(HarnessApiError::Authorization);
        }
        Ok(Self {
            inner: Arc::new(RwLock::new(Zeroizing::new(token.to_owned()))),
        })
    }

    pub fn replace(&self, token: &str) -> Result<(), HarnessApiError> {
        if token.trim().is_empty() {
            return Err(HarnessApiError::Authorization);
        }
        *self
            .inner
            .write()
            .map_err(|_| HarnessApiError::Authorization)? = Zeroizing::new(token.to_owned());
        Ok(())
    }

    pub fn authorization(&self) -> Result<HeaderValue, HarnessApiError> {
        let token = self
            .inner
            .read()
            .map_err(|_| HarnessApiError::Authorization)?;
        HeaderValue::from_str(&format!("Bearer {}", token.as_str()))
            .map_err(|_| HarnessApiError::Authorization)
    }
}

pub struct HarnessApiClient {
    origin: Url,
    access_token: RotatingAccessToken,
    client: Client,
}

impl std::fmt::Debug for HarnessApiClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HarnessApiClient")
            .field("origin", &self.origin)
            .field("authorization", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl HarnessApiClient {
    pub fn new(api_origin: &str, access_token: &str) -> Result<Self, HarnessApiError> {
        Self::with_access_token(api_origin, RotatingAccessToken::new(access_token)?)
    }

    pub fn with_access_token(
        api_origin: &str,
        access_token: RotatingAccessToken,
    ) -> Result<Self, HarnessApiError> {
        let origin = Url::parse(api_origin).map_err(|_| HarnessApiError::Origin)?;
        let host = origin.host_str().ok_or(HarnessApiError::Origin)?;
        if origin.scheme() != "https"
            || !(host == "crowcompute.ai" || host.ends_with(".crowcompute.ai"))
            || origin.cannot_be_a_base()
        {
            return Err(HarnessApiError::Origin);
        }
        access_token.authorization()?;
        Ok(Self {
            origin,
            access_token,
            client: Client::builder()
                .https_only(true)
                .timeout(REQUEST_TIMEOUT)
                .build()?,
        })
    }

    pub async fn start_run(
        &self,
        request: &StartHarnessRunV1,
    ) -> Result<StartedHarnessRunV1, HarnessApiError> {
        let response = self.post_json("/api/v1/harness/runs", request).await?;
        let response = decode_success::<StartHarnessRunResponse>(response).await?;
        if response.lease_token.is_empty()
            || response.run.id == Uuid::nil()
            || response.run.arena_id != request.arena_id
            || response.run.agent_version_id != request.agent_version_id
            || response.run.status != "running"
        {
            return Err(HarnessApiError::Response);
        }
        Ok(StartedHarnessRunV1 {
            run: response.run,
            lease_token: Zeroizing::new(response.lease_token),
            lease_expires_at: response.lease_expires_at,
        })
    }

    pub async fn agent_version(
        &self,
        version_id: Uuid,
    ) -> Result<AgentVersionEnvelopeV1, HarnessApiError> {
        if version_id == Uuid::nil() {
            return Err(HarnessApiError::Response);
        }
        let response = self.get("/api/v1/harness/agent-versions").await?;
        let versions = decode_success::<AgentVersionListResponse>(response)
            .await?
            .versions;
        versions
            .into_iter()
            .find(|version| version.version_id == version_id)
            .ok_or(HarnessApiError::Response)
    }

    pub async fn renew_lease(
        &self,
        run_id: Uuid,
        lease_token: &Zeroizing<String>,
    ) -> Result<OffsetDateTime, HarnessApiError> {
        if lease_token.is_empty() {
            return Err(HarnessApiError::Authorization);
        }
        let path = format!("/api/v1/harness/runs/{run_id}/lease");
        let response = self
            .post_json(
                &path,
                &RenewLeaseRequest {
                    lease_token: lease_token.as_str(),
                },
            )
            .await?;
        Ok(decode_success::<RenewLeaseResponse>(response)
            .await?
            .lease_expires_at)
    }

    pub async fn switch_run_strategy(
        &self,
        run_id: Uuid,
        agent_version_id: Uuid,
    ) -> Result<(), HarnessApiError> {
        if run_id == Uuid::nil() || agent_version_id == Uuid::nil() {
            return Err(HarnessApiError::Response);
        }
        let path = format!("/api/v1/harness/runs/{run_id}/strategy");
        let response = self
            .patch_json(&path, &SwitchRunStrategyRequest { agent_version_id })
            .await?;
        if !response.status().is_success() {
            return Err(HarnessApiError::Status(response.status().as_u16()));
        }
        Ok(())
    }

    pub async fn append_event(
        &self,
        event: &RunEventEnvelopeV1,
    ) -> Result<String, HarnessApiError> {
        event.verify().map_err(|_| HarnessApiError::Response)?;
        if event.server_receipt.is_some() {
            return Err(HarnessApiError::Response);
        }
        let path = format!("/api/v1/harness/runs/{}/events", event.run_id);
        let response = self.post_json(&path, event).await?;
        let accepted = decode_success::<AppendEventResponse>(response).await?;
        if accepted.event_id != event.event_id || accepted.server_receipt.is_empty() {
            return Err(HarnessApiError::Response);
        }
        Ok(accepted.server_receipt)
    }

    async fn post_json<T: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<reqwest::Response, HarnessApiError> {
        let endpoint = self
            .origin
            .join(path)
            .map_err(|_| HarnessApiError::Origin)?;
        Ok(self
            .client
            .post(endpoint)
            .header(AUTHORIZATION, self.access_token.authorization()?)
            .header(CONTENT_TYPE, "application/json")
            .json(body)
            .send()
            .await?)
    }

    async fn patch_json<T: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<reqwest::Response, HarnessApiError> {
        let endpoint = self
            .origin
            .join(path)
            .map_err(|_| HarnessApiError::Origin)?;
        Ok(self
            .client
            .patch(endpoint)
            .header(AUTHORIZATION, self.access_token.authorization()?)
            .header(CONTENT_TYPE, "application/json")
            .json(body)
            .send()
            .await?)
    }

    async fn get(&self, path: &str) -> Result<reqwest::Response, HarnessApiError> {
        let endpoint = self
            .origin
            .join(path)
            .map_err(|_| HarnessApiError::Origin)?;
        Ok(self
            .client
            .get(endpoint)
            .header(AUTHORIZATION, self.access_token.authorization()?)
            .send()
            .await?)
    }
}

async fn decode_success<T: DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, HarnessApiError> {
    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(HarnessApiError::Authorization);
    }
    if !response.status().is_success() {
        return Err(HarnessApiError::Status(response.status().as_u16()));
    }
    response
        .json::<T>()
        .await
        .map_err(|_| HarnessApiError::Response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_rejects_plaintext_and_non_crow_origins() {
        assert!(HarnessApiClient::new("http://api.crowcompute.ai", "token").is_err());
        assert!(HarnessApiClient::new("https://crowcompute.ai.example.com", "token").is_err());
        assert!(HarnessApiClient::new("https://api.crowcompute.ai", "").is_err());
        assert!(HarnessApiClient::new("https://api.crowcompute.ai", "token").is_ok());
    }

    #[test]
    fn client_debug_redacts_access_token() -> Result<(), HarnessApiError> {
        let client = HarnessApiClient::new(
            "https://api.crowcompute.ai",
            "crow_device_access_do_not_log",
        )?;
        let debug = format!("{client:?}");
        assert!(!debug.contains("crow_device_access_do_not_log"));
        assert!(debug.contains("[REDACTED]"));
        Ok(())
    }

    #[test]
    fn rotating_access_token_changes_future_authorization_without_leaking()
    -> Result<(), HarnessApiError> {
        let token = RotatingAccessToken::new("first-secret")?;
        let first = token.authorization()?;
        token.replace("second-secret")?;
        let second = token.authorization()?;
        assert_eq!(first.to_str().ok(), Some("Bearer first-secret"));
        assert_eq!(second.to_str().ok(), Some("Bearer second-secret"));
        let debug = format!("{token:?}");
        assert!(!debug.contains("first-secret") && !debug.contains("second-secret"));
        Ok(())
    }
}
