use crate::StrategyBundleV1;
use crow_agent_protocol::{HARNESS_PROTOCOL_V1, canonical_json};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;
use uuid::Uuid;

pub const MAX_COMPANION_MESSAGE_BYTES: usize = 16 * 1024;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanionActionV1 {
    Status,
    Pause,
    Resume,
    Stop,
    UpdateSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSettingsV1 {
    pub decision_cooldown_seconds: u32,
    pub isolated_leverage: u8,
}

impl RunSettingsV1 {
    pub fn validate(self) -> Result<(), CompanionIpcError> {
        if !(crow_agent_protocol::MIN_LIVE_DECISION_INTERVAL_SECONDS
            ..=crow_agent_protocol::MAX_CLIENT_DECISION_COOLDOWN_SECONDS)
            .contains(&self.decision_cooldown_seconds)
            || !self.decision_cooldown_seconds.is_multiple_of(60)
            || !(crow_agent_protocol::MIN_CLIENT_ISOLATED_LEVERAGE
                ..=crow_agent_protocol::MAX_CLIENT_ISOLATED_LEVERAGE)
                .contains(&self.isolated_leverage)
        {
            return Err(CompanionIpcError::Protocol);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompanionRequestV1 {
    pub protocol: String,
    pub request_id: Uuid,
    pub nonce: u64,
    pub action: CompanionActionV1,
    pub strategy: Option<StrategyBundleV1>,
    pub settings: Option<RunSettingsV1>,
    pub mac: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompanionResponseV1 {
    pub protocol: String,
    pub request_id: Uuid,
    pub nonce: u64,
    pub accepted: bool,
    pub execution_state: String,
    pub active_run: Option<Uuid>,
    pub error: Option<String>,
    pub mac: String,
}

#[derive(Debug, Error)]
pub enum CompanionIpcError {
    #[error("companion IPC payload is invalid")]
    Serialization,
    #[error("companion IPC authentication failed")]
    Authentication,
    #[error("companion IPC protocol is unsupported")]
    Protocol,
}

#[derive(Serialize)]
struct UnsignedRequest<'a> {
    protocol: &'a str,
    request_id: Uuid,
    nonce: u64,
    action: CompanionActionV1,
    strategy: Option<&'a StrategyBundleV1>,
    settings: Option<RunSettingsV1>,
}

#[derive(Serialize)]
struct UnsignedResponse<'a> {
    protocol: &'a str,
    request_id: Uuid,
    nonce: u64,
    accepted: bool,
    execution_state: &'a str,
    active_run: Option<Uuid>,
    error: Option<&'a str>,
}

impl CompanionRequestV1 {
    pub fn sign(
        key: &[u8; 32],
        nonce: u64,
        action: CompanionActionV1,
    ) -> Result<Self, CompanionIpcError> {
        Self::sign_with_strategy(key, nonce, action, None)
    }

    pub fn sign_with_strategy(
        key: &[u8; 32],
        nonce: u64,
        action: CompanionActionV1,
        strategy: Option<StrategyBundleV1>,
    ) -> Result<Self, CompanionIpcError> {
        Self::sign_with_options(key, nonce, action, strategy, None)
    }

    pub fn sign_with_options(
        key: &[u8; 32],
        nonce: u64,
        action: CompanionActionV1,
        strategy: Option<StrategyBundleV1>,
        settings: Option<RunSettingsV1>,
    ) -> Result<Self, CompanionIpcError> {
        if strategy.is_some() && action != CompanionActionV1::Resume {
            return Err(CompanionIpcError::Protocol);
        }
        if strategy
            .as_ref()
            .is_some_and(|value| value.validate().is_err())
        {
            return Err(CompanionIpcError::Protocol);
        }
        if settings.is_some() && action != CompanionActionV1::UpdateSettings {
            return Err(CompanionIpcError::Protocol);
        }
        if let Some(settings) = settings {
            settings.validate()?;
        }
        let request_id = Uuid::new_v4();
        let unsigned = UnsignedRequest {
            protocol: HARNESS_PROTOCOL_V1,
            request_id,
            nonce,
            action,
            strategy: strategy.as_ref(),
            settings,
        };
        let mac = sign_value(key, &unsigned)?;
        Ok(Self {
            protocol: HARNESS_PROTOCOL_V1.into(),
            request_id,
            nonce,
            action,
            strategy,
            settings,
            mac,
        })
    }

    pub fn verify(&self, key: &[u8; 32]) -> Result<(), CompanionIpcError> {
        if self.protocol != HARNESS_PROTOCOL_V1
            || (self.strategy.is_some() && self.action != CompanionActionV1::Resume)
            || (self.settings.is_some() && self.action != CompanionActionV1::UpdateSettings)
            || self
                .strategy
                .as_ref()
                .is_some_and(|value| value.validate().is_err())
        {
            return Err(CompanionIpcError::Protocol);
        }
        verify_value(
            key,
            &UnsignedRequest {
                protocol: &self.protocol,
                request_id: self.request_id,
                nonce: self.nonce,
                action: self.action,
                strategy: self.strategy.as_ref(),
                settings: self.settings,
            },
            &self.mac,
        )
    }
}

impl CompanionResponseV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn sign(
        key: &[u8; 32],
        request_id: Uuid,
        nonce: u64,
        accepted: bool,
        execution_state: impl Into<String>,
        active_run: Option<Uuid>,
        error: Option<String>,
    ) -> Result<Self, CompanionIpcError> {
        let execution_state = execution_state.into();
        let unsigned = UnsignedResponse {
            protocol: HARNESS_PROTOCOL_V1,
            request_id,
            nonce,
            accepted,
            execution_state: &execution_state,
            active_run,
            error: error.as_deref(),
        };
        let mac = sign_value(key, &unsigned)?;
        Ok(Self {
            protocol: HARNESS_PROTOCOL_V1.into(),
            request_id,
            nonce,
            accepted,
            execution_state,
            active_run,
            error,
            mac,
        })
    }

    pub fn verify(
        &self,
        key: &[u8; 32],
        request: &CompanionRequestV1,
    ) -> Result<(), CompanionIpcError> {
        if self.protocol != HARNESS_PROTOCOL_V1
            || self.request_id != request.request_id
            || self.nonce != request.nonce
        {
            return Err(CompanionIpcError::Protocol);
        }
        verify_value(
            key,
            &UnsignedResponse {
                protocol: &self.protocol,
                request_id: self.request_id,
                nonce: self.nonce,
                accepted: self.accepted,
                execution_state: &self.execution_state,
                active_run: self.active_run,
                error: self.error.as_deref(),
            },
            &self.mac,
        )
    }
}

fn sign_value<T: Serialize>(key: &[u8; 32], value: &T) -> Result<String, CompanionIpcError> {
    let bytes = canonical_json(value).map_err(|_| CompanionIpcError::Serialization)?;
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| CompanionIpcError::Authentication)?;
    mac.update(&bytes);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn verify_value<T: Serialize>(
    key: &[u8; 32],
    value: &T,
    signature: &str,
) -> Result<(), CompanionIpcError> {
    let bytes = canonical_json(value).map_err(|_| CompanionIpcError::Serialization)?;
    let signature = hex::decode(signature).map_err(|_| CompanionIpcError::Authentication)?;
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| CompanionIpcError::Authentication)?;
    mac.update(&bytes);
    mac.verify_slice(&signature)
        .map_err(|_| CompanionIpcError::Authentication)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_response_bind_action_nonce_and_request() -> Result<(), CompanionIpcError> {
        let key = [7_u8; 32];
        let request = CompanionRequestV1::sign(&key, 42, CompanionActionV1::Pause)?;
        request.verify(&key)?;
        let response = CompanionResponseV1::sign(
            &key,
            request.request_id,
            request.nonce,
            true,
            "paused",
            None,
            None,
        )?;
        response.verify(&key, &request)?;
        Ok(())
    }

    #[test]
    fn tampered_request_and_cross_request_response_fail() -> Result<(), CompanionIpcError> {
        let key = [11_u8; 32];
        let mut request = CompanionRequestV1::sign(&key, 9, CompanionActionV1::Pause)?;
        request.action = CompanionActionV1::Stop;
        assert!(request.verify(&key).is_err());

        let original = CompanionRequestV1::sign(&key, 10, CompanionActionV1::Status)?;
        let other = CompanionRequestV1::sign(&key, 11, CompanionActionV1::Status)?;
        let response = CompanionResponseV1::sign(
            &key,
            original.request_id,
            original.nonce,
            true,
            "paused",
            None,
            None,
        )?;
        assert!(response.verify(&key, &other).is_err());
        Ok(())
    }

    #[test]
    fn strategy_switch_is_bound_to_a_resume_request() -> Result<(), CompanionIpcError> {
        let key = [13_u8; 32];
        let strategy = StrategyBundleV1 {
            protocol: HARNESS_PROTOCOL_V1.into(),
            version_id: Uuid::new_v4(),
            model_id: "crow-qwen3-5-27b".into(),
            name: "Degen".into(),
            system_instructions: "Trade the owner strategy.".into(),
            tools: crate::REQUIRED_STRATEGY_TOOLS.map(str::to_owned).to_vec(),
            created_at: time::OffsetDateTime::now_utc(),
        };
        let request = CompanionRequestV1::sign_with_strategy(
            &key,
            12,
            CompanionActionV1::Resume,
            Some(strategy),
        )?;
        request.verify(&key)?;
        assert!(request.strategy.is_some());
        Ok(())
    }
}
