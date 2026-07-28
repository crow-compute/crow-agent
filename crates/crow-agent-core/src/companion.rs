use crow_agent_protocol::{HARNESS_PROTOCOL_V1, canonical_json};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;
use uuid::Uuid;

pub const MAX_COMPANION_MESSAGE_BYTES: usize = 8 * 1024;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanionActionV1 {
    Status,
    Pause,
    Resume,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompanionRequestV1 {
    pub protocol: String,
    pub request_id: Uuid,
    pub nonce: u64,
    pub action: CompanionActionV1,
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
        let request_id = Uuid::new_v4();
        let unsigned = UnsignedRequest {
            protocol: HARNESS_PROTOCOL_V1,
            request_id,
            nonce,
            action,
        };
        Ok(Self {
            protocol: HARNESS_PROTOCOL_V1.into(),
            request_id,
            nonce,
            action,
            mac: sign_value(key, &unsigned)?,
        })
    }

    pub fn verify(&self, key: &[u8; 32]) -> Result<(), CompanionIpcError> {
        if self.protocol != HARNESS_PROTOCOL_V1 {
            return Err(CompanionIpcError::Protocol);
        }
        verify_value(
            key,
            &UnsignedRequest {
                protocol: &self.protocol,
                request_id: self.request_id,
                nonce: self.nonce,
                action: self.action,
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
}
