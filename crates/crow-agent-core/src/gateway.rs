use crow_agent_protocol::ArenaInferenceReceiptV1;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

#[derive(Debug, Clone, Serialize)]
pub struct InferenceRequest {
    pub arena_id: Uuid,
    pub run_id: Uuid,
    pub cycle_id: Uuid,
    pub model: String,
    pub messages: Vec<Value>,
    pub tools: Vec<Value>,
    pub max_tokens: u32,
    pub temperature_millis: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InferenceResponse {
    pub output: Value,
    pub receipt: ArenaInferenceReceiptV1,
}

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("invalid gateway URL")]
    Url,
    #[error("invalid authorization material")]
    Authorization,
    #[error("gateway request failed")]
    Request(#[from] reqwest::Error),
    #[error("gateway returned an unsuccessful status: {0}")]
    Status(u16),
    #[error("gateway receipt is not bound to the request")]
    ReceiptBinding,
}

#[derive(Debug)]
pub struct GatewayClient {
    endpoint: Url,
    token: Zeroizing<String>,
    client: reqwest::Client,
}

impl GatewayClient {
    pub fn new(api_origin: &str, token: String) -> Result<Self, GatewayError> {
        let endpoint = Url::parse(api_origin)
            .map_err(|_| GatewayError::Url)?
            .join("/api/v1/arena/inference")
            .map_err(|_| GatewayError::Url)?;
        if token.trim().is_empty() {
            return Err(GatewayError::Authorization);
        }
        let https_only = endpoint.scheme() == "https";
        Ok(Self {
            endpoint,
            token: Zeroizing::new(token),
            client: reqwest::Client::builder().https_only(https_only).build()?,
        })
    }

    pub async fn infer(
        &self,
        request: &InferenceRequest,
    ) -> Result<InferenceResponse, GatewayError> {
        let authorization = HeaderValue::from_str(&format!("Bearer {}", self.token.as_str()))
            .map_err(|_| GatewayError::Authorization)?;
        let response = self
            .client
            .post(self.endpoint.clone())
            .header(AUTHORIZATION, authorization)
            .header(CONTENT_TYPE, "application/json")
            .json(request)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(GatewayError::Status(response.status().as_u16()));
        }
        let result = response.json::<InferenceResponse>().await?;
        if result.receipt.arena_id != request.arena_id
            || result.receipt.run_id != request.run_id
            || result.receipt.cycle_id != request.cycle_id
            || result.receipt.model_id != request.model
        {
            return Err(GatewayError::ReceiptBinding);
        }
        Ok(result)
    }
}
