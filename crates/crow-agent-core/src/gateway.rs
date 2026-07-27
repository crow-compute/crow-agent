use crate::runtime::{InferenceProvider, InferenceTurn, ModelTurn, ModelTurnRequest, RuntimeError};
use async_trait::async_trait;
use crow_agent_protocol::{ArenaInferenceReceiptV1, canonical_json, sha256};
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
    pub input: Value,
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
            .join("/api/v1/harness/inference")
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
        let input_sha256 = hex::encode(sha256(
            &canonical_json(request).map_err(|_| GatewayError::ReceiptBinding)?,
        ));
        let output_sha256 = hex::encode(sha256(
            &canonical_json(&result.output).map_err(|_| GatewayError::ReceiptBinding)?,
        ));
        let empty_tool_calls = Value::Array(Vec::new());
        let tool_calls = result.output.get("tool_calls").unwrap_or(&empty_tool_calls);
        let tool_calls_sha256 = hex::encode(sha256(
            &canonical_json(tool_calls).map_err(|_| GatewayError::ReceiptBinding)?,
        ));
        if result.receipt.arena_id != request.arena_id
            || result.receipt.run_id != request.run_id
            || result.receipt.cycle_id != request.cycle_id
            || result.receipt.model_id != request.model
            || result.receipt.input_sha256 != input_sha256
            || result.receipt.output_sha256 != output_sha256
            || result.receipt.tool_calls_sha256 != tool_calls_sha256
            || result.receipt.verify().is_err()
        {
            return Err(GatewayError::ReceiptBinding);
        }
        Ok(result)
    }
}

#[async_trait]
impl InferenceProvider for GatewayClient {
    async fn infer(&self, request: &ModelTurnRequest) -> Result<InferenceTurn, RuntimeError> {
        let input = serde_json::to_value(request).map_err(|_| RuntimeError::Inference)?;
        let canonical = canonical_json(request).map_err(|_| RuntimeError::Inference)?;
        let prompt = String::from_utf8(canonical).map_err(|_| RuntimeError::Inference)?;
        let response = GatewayClient::infer(
            self,
            &InferenceRequest {
                arena_id: request.arena_id,
                run_id: request.run_id,
                cycle_id: request.cycle_id,
                model: request.model_id.clone(),
                input,
                messages: vec![
                    serde_json::json!({
                        "role": "system",
                        "content": "Return one compact JSON object with exactly tool_calls and proposal. Use fixed-point integers only. Never include markdown. tool_calls may use market_snapshot, portfolio_snapshot, or recent_candles. Set proposal to null while requesting tools; otherwise tool_calls must be empty."
                    }),
                    serde_json::json!({"role": "user", "content": prompt}),
                ],
                tools: Vec::new(),
                max_tokens: 2_048,
                temperature_millis: 0,
            },
        )
        .await
        .map_err(|_| RuntimeError::Inference)?;
        let output = serde_json::from_value::<ModelTurn>(response.output)
            .map_err(|_| RuntimeError::Inference)?;
        Ok(InferenceTurn {
            output,
            receipt: response.receipt,
        })
    }
}
