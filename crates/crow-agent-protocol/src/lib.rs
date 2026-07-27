//! Public, versioned wire types for the user-hosted Crow agent.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

pub const HARNESS_PROTOCOL_V1: &str = "crow.harness.v1";
pub const ALLOWED_SYMBOLS: [&str; 3] = ["BTC", "ETH", "SOL"];
pub const ALLOWED_MODELS: [&str; 3] = [
    "crow-qwen3-5-27b",
    "crow-qwen3-5-35b-a3b",
    "crow-gpt-oss-20b",
];

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("canonical JSON encoding failed")]
    CanonicalJson(#[source] serde_json::Error),
    #[error("invalid arena manifest: {0}")]
    InvalidManifest(String),
    #[error("invalid event hash")]
    InvalidEventHash,
    #[error("invalid event signature")]
    InvalidEventSignature,
    #[error("invalid public key")]
    InvalidPublicKey,
    #[error("invalid signature encoding")]
    InvalidSignatureEncoding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArenaMode {
    HistoricalBacktest,
    HyperliquidTestnet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskRulesV1 {
    pub cash_reserve_bps: u16,
    pub daily_loss_bps: u16,
    pub drawdown_bps: u16,
    pub max_order_bps: u16,
    pub max_position_bps: u16,
    pub max_spread_bps: u16,
    pub max_oracle_gap_bps: u16,
    pub book_max_age_seconds: u16,
    pub max_orders_day: u16,
    pub isolated_leverage: u8,
    pub long_only: bool,
    pub ioc_only: bool,
}

impl Default for RiskRulesV1 {
    fn default() -> Self {
        Self {
            cash_reserve_bps: 1_000,
            daily_loss_bps: 200,
            drawdown_bps: 1_000,
            max_order_bps: 200,
            max_position_bps: 1_000,
            max_spread_bps: 40,
            max_oracle_gap_bps: 100,
            book_max_age_seconds: 10,
            max_orders_day: 20,
            isolated_leverage: 1,
            long_only: true,
            ioc_only: true,
        }
    }
}

impl RiskRulesV1 {
    pub fn validate_ceiling(&self) -> Result<(), ProtocolError> {
        let ceiling = Self::default();
        let valid = self.cash_reserve_bps >= ceiling.cash_reserve_bps
            && self.daily_loss_bps <= ceiling.daily_loss_bps
            && self.drawdown_bps <= ceiling.drawdown_bps
            && self.max_order_bps <= ceiling.max_order_bps
            && self.max_position_bps <= ceiling.max_position_bps
            && self.max_spread_bps <= ceiling.max_spread_bps
            && self.max_oracle_gap_bps <= ceiling.max_oracle_gap_bps
            && self.book_max_age_seconds <= ceiling.book_max_age_seconds
            && self.max_orders_day <= ceiling.max_orders_day
            && self.isolated_leverage == 1
            && self.long_only
            && self.ioc_only;
        if valid {
            Ok(())
        } else {
            Err(ProtocolError::InvalidManifest(
                "risk rules exceed the Crow safety ceiling".into(),
            ))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoringWeightsV1 {
    pub net_return: u8,
    pub sortino: u8,
    pub inverse_drawdown: u8,
}

impl Default for ScoringWeightsV1 {
    fn default() -> Self {
        Self {
            net_return: 50,
            sortino: 30,
            inverse_drawdown: 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PenaltyRulesV1 {
    pub policy_rejection_millis: u32,
    pub missed_cycle_millis: u32,
    pub cap_millis: u32,
}

impl Default for PenaltyRulesV1 {
    fn default() -> Self {
        Self {
            policy_rejection_millis: 1_000,
            missed_cycle_millis: 250,
            cap_millis: 15_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionAssumptionsV1 {
    pub half_spread_bps: u16,
    pub slippage_bps: u16,
    pub taker_fee_bps: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketConfigV1 {
    pub enabled: bool,
    pub usdc_address: Option<String>,
    pub ticket_micro_usdc: u64,
    pub participant_cap: u32,
    pub prize_bps: u16,
    pub protocol_bps: u16,
    pub winner_bps: [u16; 3],
}

impl Default for TicketConfigV1 {
    fn default() -> Self {
        Self {
            enabled: false,
            usdc_address: None,
            ticket_micro_usdc: 0,
            participant_cap: 0,
            prize_bps: 9_000,
            protocol_bps: 1_000,
            winner_bps: [5_000, 3_000, 2_000],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArenaManifestV1 {
    pub protocol: String,
    pub arena_id: Uuid,
    pub manifest_version: u32,
    pub mode: ArenaMode,
    pub starts_at: OffsetDateTime,
    pub ends_at: OffsetDateTime,
    pub decision_interval_seconds: u32,
    pub symbols: Vec<String>,
    pub eligible_models: Vec<String>,
    pub dataset_sha256: Option<String>,
    pub required_client_version: String,
    pub risk_rules: RiskRulesV1,
    pub execution: ExecutionAssumptionsV1,
    pub scoring: ScoringWeightsV1,
    pub penalties: PenaltyRulesV1,
    pub ticket: TicketConfigV1,
}

impl ArenaManifestV1 {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.protocol != HARNESS_PROTOCOL_V1 {
            return Err(ProtocolError::InvalidManifest(
                "unsupported protocol".into(),
            ));
        }
        if self.starts_at >= self.ends_at {
            return Err(ProtocolError::InvalidManifest(
                "arena time window is empty".into(),
            ));
        }
        if self.decision_interval_seconds != 900 {
            return Err(ProtocolError::InvalidManifest(
                "v1 arenas require a 15-minute cadence".into(),
            ));
        }
        if self.symbols != ALLOWED_SYMBOLS.map(str::to_owned) {
            return Err(ProtocolError::InvalidManifest(
                "v1 symbols must be BTC, ETH, and SOL in canonical order".into(),
            ));
        }
        if self.eligible_models.is_empty()
            || self
                .eligible_models
                .iter()
                .any(|model| !ALLOWED_MODELS.contains(&model.as_str()))
        {
            return Err(ProtocolError::InvalidManifest(
                "eligible model set is invalid".into(),
            ));
        }
        if u16::from(self.scoring.net_return)
            + u16::from(self.scoring.sortino)
            + u16::from(self.scoring.inverse_drawdown)
            != 100
        {
            return Err(ProtocolError::InvalidManifest(
                "scoring weights must sum to 100".into(),
            ));
        }
        if self.ticket.prize_bps + self.ticket.protocol_bps != 10_000
            || self.ticket.winner_bps.iter().sum::<u16>() != 10_000
        {
            return Err(ProtocolError::InvalidManifest(
                "ticket and winner splits must each sum to 10000 bps".into(),
            ));
        }
        if self.mode == ArenaMode::HistoricalBacktest
            && self
                .dataset_sha256
                .as_deref()
                .is_none_or(|digest| !is_sha256(digest))
        {
            return Err(ProtocolError::InvalidManifest(
                "historical arenas require a SHA-256 dataset digest".into(),
            ));
        }
        self.risk_rules.validate_ceiling()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceKeyWrapV1 {
    pub device_id: Uuid,
    pub ephemeral_public_key: String,
    pub nonce: String,
    pub wrapped_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentVersionEnvelopeV1 {
    pub protocol: String,
    pub version_id: Uuid,
    pub agent_id: Uuid,
    pub version: u32,
    pub model_id: String,
    pub configuration_sha256: String,
    pub ciphertext: String,
    pub nonce: String,
    pub key_wraps: Vec<DeviceKeyWrapV1>,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArenaInferenceReceiptV1 {
    pub protocol: String,
    pub receipt_id: Uuid,
    pub arena_id: Uuid,
    pub run_id: Uuid,
    pub cycle_id: Uuid,
    pub model_id: String,
    pub model_revision: String,
    pub runtime_digest: String,
    pub input_sha256: String,
    pub output_sha256: String,
    pub tool_calls_sha256: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub amount_microcredits: u64,
    pub gateway_public_key: String,
    pub gateway_signature: String,
    pub finalized_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseTargetV1 {
    pub os: String,
    pub arch: String,
    pub artifact_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseManifestV1 {
    pub protocol: String,
    pub version: String,
    pub source_commit: String,
    pub source_committed_at: OffsetDateTime,
    pub targets: Vec<ReleaseTargetV1>,
    pub ui_sha256: Option<String>,
    pub sbom_sha256: String,
    pub vulnerability_report_sha256: String,
    pub compatible_protocols: Vec<String>,
    pub signer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAction {
    Pause,
    Resume,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteCommandV1 {
    pub protocol: String,
    pub command_id: Uuid,
    pub target_device_id: Uuid,
    pub run_id: Uuid,
    pub action: RemoteAction,
    pub nonce: u64,
    pub issued_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub controller_device_id: Uuid,
    pub controller_signature: String,
    pub relay_receipt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEventEnvelopeV1 {
    pub protocol: String,
    pub event_id: Uuid,
    pub arena_id: Uuid,
    pub run_id: Uuid,
    pub cycle_id: Option<Uuid>,
    pub sequence: u64,
    pub previous_event_sha256: String,
    pub event_type: String,
    pub occurred_at: OffsetDateTime,
    pub payload: Value,
    pub event_sha256: String,
    pub device_public_key: String,
    pub device_signature: String,
}

#[derive(Debug, Serialize)]
struct UnsignedRunEvent<'a> {
    protocol: &'a str,
    event_id: Uuid,
    arena_id: Uuid,
    run_id: Uuid,
    cycle_id: Option<Uuid>,
    sequence: u64,
    previous_event_sha256: &'a str,
    event_type: &'a str,
    occurred_at: OffsetDateTime,
    payload: &'a Value,
}

impl RunEventEnvelopeV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn sign(
        signing_key: &SigningKey,
        arena_id: Uuid,
        run_id: Uuid,
        cycle_id: Option<Uuid>,
        sequence: u64,
        previous_event_sha256: String,
        event_type: String,
        occurred_at: OffsetDateTime,
        payload: Value,
    ) -> Result<Self, ProtocolError> {
        let event_id = Uuid::new_v4();
        let unsigned = UnsignedRunEvent {
            protocol: HARNESS_PROTOCOL_V1,
            event_id,
            arena_id,
            run_id,
            cycle_id,
            sequence,
            previous_event_sha256: &previous_event_sha256,
            event_type: &event_type,
            occurred_at,
            payload: &payload,
        };
        let digest = sha256(&canonical_json(&unsigned)?);
        let signature = signing_key.sign(&digest);
        Ok(Self {
            protocol: HARNESS_PROTOCOL_V1.into(),
            event_id,
            arena_id,
            run_id,
            cycle_id,
            sequence,
            previous_event_sha256,
            event_type,
            occurred_at,
            payload,
            event_sha256: hex::encode(digest),
            device_public_key: URL_SAFE_NO_PAD.encode(signing_key.verifying_key().as_bytes()),
            device_signature: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
        })
    }

    pub fn verify(&self) -> Result<(), ProtocolError> {
        if self.protocol != HARNESS_PROTOCOL_V1 {
            return Err(ProtocolError::InvalidEventHash);
        }
        let unsigned = UnsignedRunEvent {
            protocol: &self.protocol,
            event_id: self.event_id,
            arena_id: self.arena_id,
            run_id: self.run_id,
            cycle_id: self.cycle_id,
            sequence: self.sequence,
            previous_event_sha256: &self.previous_event_sha256,
            event_type: &self.event_type,
            occurred_at: self.occurred_at,
            payload: &self.payload,
        };
        let digest = sha256(&canonical_json(&unsigned)?);
        if hex::encode(digest) != self.event_sha256 {
            return Err(ProtocolError::InvalidEventHash);
        }
        let public_raw = URL_SAFE_NO_PAD
            .decode(&self.device_public_key)
            .map_err(|_| ProtocolError::InvalidPublicKey)?;
        let public_array: [u8; 32] = public_raw
            .try_into()
            .map_err(|_| ProtocolError::InvalidPublicKey)?;
        let public =
            VerifyingKey::from_bytes(&public_array).map_err(|_| ProtocolError::InvalidPublicKey)?;
        let signature_raw = URL_SAFE_NO_PAD
            .decode(&self.device_signature)
            .map_err(|_| ProtocolError::InvalidSignatureEncoding)?;
        let signature = Signature::from_slice(&signature_raw)
            .map_err(|_| ProtocolError::InvalidSignatureEncoding)?;
        public
            .verify(&digest, &signature)
            .map_err(|_| ProtocolError::InvalidEventSignature)
    }
}

#[derive(Debug)]
pub struct DeviceIdentity {
    signing_key: SigningKey,
}

impl DeviceIdentity {
    #[must_use]
    pub fn generate() -> Self {
        Self {
            signing_key: SigningKey::generate(&mut OsRng),
        }
    }

    #[must_use]
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            signing_key: SigningKey::from_bytes(seed),
        }
    }

    #[must_use]
    pub fn seed(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }

    #[must_use]
    pub fn public_key(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.signing_key.verifying_key().as_bytes())
    }

    #[must_use]
    pub fn sign_bytes(&self, payload: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(self.signing_key.sign(payload).to_bytes())
    }

    #[must_use]
    pub fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }
}

pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    let value = serde_json::to_value(value).map_err(ProtocolError::CanonicalJson)?;
    let sorted = sort_json(value);
    serde_json::to_vec(&sorted).map_err(ProtocolError::CanonicalJson)
}

fn sort_json(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted = map
                .into_iter()
                .map(|(key, value)| (key, sort_json(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(values) => Value::Array(values.into_iter().map(sort_json).collect()),
        other => other,
    }
}

#[must_use]
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::macros::datetime;

    #[test]
    fn default_manifest_is_valid() {
        let manifest = ArenaManifestV1 {
            protocol: HARNESS_PROTOCOL_V1.into(),
            arena_id: Uuid::nil(),
            manifest_version: 1,
            mode: ArenaMode::HistoricalBacktest,
            starts_at: datetime!(2026-07-01 00:00 UTC),
            ends_at: datetime!(2026-07-02 00:00 UTC),
            decision_interval_seconds: 900,
            symbols: ALLOWED_SYMBOLS.map(str::to_owned).to_vec(),
            eligible_models: vec![ALLOWED_MODELS[0].into()],
            dataset_sha256: Some("a".repeat(64)),
            required_client_version: "0.1.0".into(),
            risk_rules: RiskRulesV1::default(),
            execution: ExecutionAssumptionsV1 {
                half_spread_bps: 2,
                slippage_bps: 3,
                taker_fee_bps: 5,
            },
            scoring: ScoringWeightsV1::default(),
            penalties: PenaltyRulesV1::default(),
            ticket: TicketConfigV1::default(),
        };
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn event_signature_detects_mutation() -> Result<(), ProtocolError> {
        let identity = DeviceIdentity::generate();
        let mut event = RunEventEnvelopeV1::sign(
            identity.signing_key(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            None,
            1,
            "0".repeat(64),
            "decision".into(),
            datetime!(2026-07-27 00:00 UTC),
            json!({"action": "hold"}),
        )?;
        event.verify()?;
        event.payload = json!({"action": "buy"});
        assert!(matches!(
            event.verify(),
            Err(ProtocolError::InvalidEventHash)
        ));
        Ok(())
    }

    #[test]
    fn canonical_json_sorts_nested_objects() -> Result<(), ProtocolError> {
        let left = json!({"z": 1, "a": {"y": 2, "b": 3}});
        let right = json!({"a": {"b": 3, "y": 2}, "z": 1});
        assert_eq!(canonical_json(&left)?, canonical_json(&right)?);
        Ok(())
    }
}
