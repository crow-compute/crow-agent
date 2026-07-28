use crate::crypto::{
    BundleCiphertext, CryptoError, DeviceEncryptionKey, WrappedBundleKey, decrypt_bundle,
    encrypt_bundle, generate_bundle_key, wrap_bundle_key,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crow_agent_protocol::{
    AgentVersionEnvelopeV1, DeviceKeyWrapV1, HARNESS_PROTOCOL_V1, canonical_json, sha256,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

pub const REQUIRED_STRATEGY_TOOLS: [&str; 3] =
    ["market_snapshot", "portfolio_snapshot", "recent_candles"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyBundleV1 {
    pub protocol: String,
    pub version_id: Uuid,
    pub model_id: String,
    pub name: String,
    pub system_instructions: String,
    pub tools: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentVersionRecipient {
    pub device_id: Uuid,
    pub encryption_public_key: [u8; 32],
}

#[derive(Debug, Error)]
pub enum AgentVersionError {
    #[error("agent version bundle is invalid")]
    Invalid,
    #[error("agent version cryptography failed")]
    Crypto(#[from] CryptoError),
    #[error("agent version serialization failed")]
    Serialization,
    #[error("agent version is not wrapped to this device")]
    DeviceWrap,
}

impl StrategyBundleV1 {
    pub fn validate(&self) -> Result<(), AgentVersionError> {
        if self.protocol != HARNESS_PROTOCOL_V1
            || self.version_id == Uuid::nil()
            || self.model_id.trim().is_empty()
            || self.name.trim().is_empty()
            || self.name.chars().count() > 80
            || self.system_instructions.trim().is_empty()
            || self.system_instructions.chars().count() > 8_192
            || self.tools != REQUIRED_STRATEGY_TOOLS.map(str::to_owned)
        {
            return Err(AgentVersionError::Invalid);
        }
        Ok(())
    }
}

pub fn seal_agent_version(
    bundle: &StrategyBundleV1,
    agent_id: Uuid,
    version: u32,
    recipients: &[AgentVersionRecipient],
) -> Result<AgentVersionEnvelopeV1, AgentVersionError> {
    bundle.validate()?;
    if agent_id == Uuid::nil() || version == 0 || recipients.is_empty() || recipients.len() > 32 {
        return Err(AgentVersionError::Invalid);
    }
    let mut recipient_ids = recipients
        .iter()
        .map(|recipient| recipient.device_id)
        .collect::<Vec<_>>();
    recipient_ids.sort_unstable();
    recipient_ids.dedup();
    if recipient_ids.len() != recipients.len()
        || recipient_ids
            .iter()
            .any(|device_id| *device_id == Uuid::nil())
    {
        return Err(AgentVersionError::Invalid);
    }

    let aad = agent_version_aad(bundle.version_id);
    let plaintext = canonical_json(bundle).map_err(|_| AgentVersionError::Serialization)?;
    let configuration_sha256 = hex::encode(sha256(&plaintext));
    let bundle_key = generate_bundle_key();
    let encrypted = encrypt_bundle(&bundle_key, &plaintext, &aad)?;
    let key_wraps = recipients
        .iter()
        .map(|recipient| {
            let wrapped = wrap_bundle_key(&bundle_key, &recipient.encryption_public_key, &aad)?;
            Ok(DeviceKeyWrapV1 {
                device_id: recipient.device_id,
                ephemeral_public_key: wrapped.ephemeral_public_key,
                nonce: wrapped.nonce,
                wrapped_key: wrapped.wrapped_key,
            })
        })
        .collect::<Result<Vec<_>, CryptoError>>()?;

    Ok(AgentVersionEnvelopeV1 {
        protocol: HARNESS_PROTOCOL_V1.into(),
        version_id: bundle.version_id,
        agent_id,
        version,
        model_id: bundle.model_id.clone(),
        configuration_sha256,
        ciphertext: encrypted.ciphertext,
        nonce: encrypted.nonce,
        key_wraps,
        created_at: bundle.created_at,
    })
}

pub fn open_agent_version(
    envelope: &AgentVersionEnvelopeV1,
    device_id: Uuid,
    device_key: &DeviceEncryptionKey,
) -> Result<StrategyBundleV1, AgentVersionError> {
    if envelope.protocol != HARNESS_PROTOCOL_V1
        || envelope.version_id == Uuid::nil()
        || envelope.agent_id == Uuid::nil()
        || envelope.version == 0
        || envelope.model_id.trim().is_empty()
    {
        return Err(AgentVersionError::Invalid);
    }
    let wrap = envelope
        .key_wraps
        .iter()
        .find(|wrap| wrap.device_id == device_id)
        .ok_or(AgentVersionError::DeviceWrap)?;
    let aad = agent_version_aad(envelope.version_id);
    let bundle_key = device_key.unwrap_bundle_key(
        &WrappedBundleKey {
            ephemeral_public_key: wrap.ephemeral_public_key.clone(),
            nonce: wrap.nonce.clone(),
            wrapped_key: wrap.wrapped_key.clone(),
        },
        &aad,
    )?;
    let plaintext = decrypt_bundle(
        &bundle_key,
        &BundleCiphertext {
            nonce: envelope.nonce.clone(),
            ciphertext: envelope.ciphertext.clone(),
        },
        &aad,
    )?;
    if hex::encode(sha256(&plaintext)) != envelope.configuration_sha256 {
        return Err(AgentVersionError::Invalid);
    }
    let bundle = serde_json::from_slice::<StrategyBundleV1>(&plaintext)
        .map_err(|_| AgentVersionError::Serialization)?;
    bundle.validate()?;
    if bundle.version_id != envelope.version_id || bundle.model_id != envelope.model_id {
        return Err(AgentVersionError::Invalid);
    }
    Ok(bundle)
}

pub fn decode_device_encryption_public_key(value: &str) -> Result<[u8; 32], AgentVersionError> {
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| AgentVersionError::Invalid)?
        .try_into()
        .map_err(|_| AgentVersionError::Invalid)
}

fn agent_version_aad(version_id: Uuid) -> Vec<u8> {
    format!("crow-agent-version-v1:{version_id}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immutable_agent_version_round_trips_only_for_wrapped_device() -> Result<(), AgentVersionError>
    {
        let first_device = Uuid::new_v4();
        let second_device = Uuid::new_v4();
        let first_key = DeviceEncryptionKey::generate();
        let second_key = DeviceEncryptionKey::generate();
        let bundle = StrategyBundleV1 {
            protocol: HARNESS_PROTOCOL_V1.into(),
            version_id: Uuid::new_v4(),
            model_id: "crow-qwen3-5-27b".into(),
            name: "Balanced testnet allocator".into(),
            system_instructions: "Prefer holds unless verified momentum clears policy.".into(),
            tools: REQUIRED_STRATEGY_TOOLS.map(str::to_owned).to_vec(),
            created_at: OffsetDateTime::now_utc(),
        };
        let envelope = seal_agent_version(
            &bundle,
            Uuid::new_v4(),
            1,
            &[AgentVersionRecipient {
                device_id: first_device,
                encryption_public_key: first_key.public_key(),
            }],
        )?;
        assert_eq!(
            open_agent_version(&envelope, first_device, &first_key)?,
            bundle
        );
        assert!(open_agent_version(&envelope, second_device, &second_key).is_err());
        Ok(())
    }

    #[test]
    fn encrypted_strategy_is_bound_to_configuration_hash() -> Result<(), AgentVersionError> {
        let device_id = Uuid::new_v4();
        let key = DeviceEncryptionKey::generate();
        let bundle = StrategyBundleV1 {
            protocol: HARNESS_PROTOCOL_V1.into(),
            version_id: Uuid::new_v4(),
            model_id: "crow-qwen3-5-27b".into(),
            name: "Hash-bound strategy".into(),
            system_instructions: "Hold when evidence is incomplete.".into(),
            tools: REQUIRED_STRATEGY_TOOLS.map(str::to_owned).to_vec(),
            created_at: OffsetDateTime::now_utc(),
        };
        let mut envelope = seal_agent_version(
            &bundle,
            Uuid::new_v4(),
            1,
            &[AgentVersionRecipient {
                device_id,
                encryption_public_key: key.public_key(),
            }],
        )?;
        envelope.configuration_sha256 = "00".repeat(32);
        assert!(open_agent_version(&envelope, device_id, &key).is_err());
        Ok(())
    }
}
