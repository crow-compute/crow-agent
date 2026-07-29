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
pub const DATASET_SOURCE_V1: &str = "hyperliquid-mainnet-info-v1";
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
    #[error("remote command is invalid")]
    InvalidRemoteCommand,
    #[error("inference receipt is invalid")]
    InvalidInferenceReceipt,
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
    #[serde(with = "time::serde::rfc3339")]
    pub starts_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
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
pub struct SignedArenaManifestV1 {
    pub manifest: ArenaManifestV1,
    pub manifest_sha256: String,
    pub signer_public_key: String,
    pub signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signed_manifest: Option<Value>,
}

impl SignedArenaManifestV1 {
    pub fn sign(
        manifest: ArenaManifestV1,
        signing_key: &SigningKey,
    ) -> Result<Self, ProtocolError> {
        manifest.validate()?;
        let digest = sha256(&canonical_json(&manifest)?);
        Ok(Self {
            manifest,
            manifest_sha256: hex::encode(digest),
            signer_public_key: URL_SAFE_NO_PAD.encode(signing_key.verifying_key().as_bytes()),
            signature: URL_SAFE_NO_PAD.encode(signing_key.sign(&digest).to_bytes()),
            signed_manifest: None,
        })
    }

    pub fn from_signed_value(
        manifest: Value,
        manifest_sha256: String,
        signer_public_key: String,
        signature: String,
    ) -> Result<Self, ProtocolError> {
        let typed_manifest = serde_json::from_value::<ArenaManifestV1>(manifest.clone())
            .map_err(ProtocolError::CanonicalJson)?;
        let signed = Self {
            manifest: typed_manifest,
            manifest_sha256,
            signer_public_key,
            signature,
            signed_manifest: Some(manifest),
        };
        signed.verify()?;
        Ok(signed)
    }

    pub fn verify(&self) -> Result<(), ProtocolError> {
        self.manifest.validate()?;
        let canonical = if let Some(signed_manifest) = &self.signed_manifest {
            let typed_manifest = serde_json::from_value::<ArenaManifestV1>(signed_manifest.clone())
                .map_err(ProtocolError::CanonicalJson)?;
            if typed_manifest != self.manifest {
                return Err(ProtocolError::InvalidEventHash);
            }
            canonical_json(signed_manifest)?
        } else {
            canonical_json(&self.manifest)?
        };
        let digest = sha256(&canonical);
        if hex::encode(digest) != self.manifest_sha256 {
            return Err(ProtocolError::InvalidEventHash);
        }
        verify_signature(&self.signer_public_key, &self.signature, &digest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetFileV1 {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetManifestV1 {
    pub protocol: String,
    pub source: String,
    pub dataset_id: Uuid,
    pub version: u32,
    pub interval_seconds: u32,
    pub symbols: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub starts_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub ends_at: OffsetDateTime,
    pub files: Vec<DatasetFileV1>,
    pub package_sha256: String,
    pub signer_public_key: String,
    pub signature: String,
}

#[derive(Debug, Serialize)]
struct UnsignedDatasetManifest<'a> {
    protocol: &'a str,
    source: &'a str,
    dataset_id: Uuid,
    version: u32,
    interval_seconds: u32,
    symbols: &'a [String],
    #[serde(with = "time::serde::rfc3339")]
    starts_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    ends_at: OffsetDateTime,
    files: &'a [DatasetFileV1],
    package_sha256: &'a str,
    signer_public_key: &'a str,
}

impl DatasetManifestV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn sign(
        signing_key: &SigningKey,
        dataset_id: Uuid,
        version: u32,
        starts_at: OffsetDateTime,
        ends_at: OffsetDateTime,
        files: Vec<DatasetFileV1>,
        package_sha256: String,
    ) -> Result<Self, ProtocolError> {
        let signer_public_key = URL_SAFE_NO_PAD.encode(signing_key.verifying_key().as_bytes());
        let mut manifest = Self {
            protocol: HARNESS_PROTOCOL_V1.into(),
            source: DATASET_SOURCE_V1.into(),
            dataset_id,
            version,
            interval_seconds: 900,
            symbols: ALLOWED_SYMBOLS.map(str::to_owned).to_vec(),
            starts_at,
            ends_at,
            files,
            package_sha256,
            signer_public_key,
            signature: String::new(),
        };
        manifest.validate()?;
        let digest = sha256(&canonical_json(&manifest.unsigned())?);
        manifest.signature = URL_SAFE_NO_PAD.encode(signing_key.sign(&digest).to_bytes());
        Ok(manifest)
    }

    pub fn verify(&self) -> Result<(), ProtocolError> {
        self.validate()?;
        let digest = sha256(&canonical_json(&self.unsigned())?);
        verify_signature(&self.signer_public_key, &self.signature, &digest)
    }

    fn validate(&self) -> Result<(), ProtocolError> {
        if self.protocol != HARNESS_PROTOCOL_V1
            || self.source != DATASET_SOURCE_V1
            || self.version == 0
            || self.interval_seconds != 900
            || self.symbols != ALLOWED_SYMBOLS.map(str::to_owned)
            || self.starts_at >= self.ends_at
            || self.files.is_empty()
            || !is_sha256(&self.package_sha256)
            || self.files.iter().any(|file| {
                file.path.is_empty()
                    || file.path.starts_with('/')
                    || file.path.contains("..")
                    || file.bytes == 0
                    || !is_sha256(&file.sha256)
            })
        {
            return Err(ProtocolError::InvalidManifest(
                "dataset manifest is invalid".into(),
            ));
        }
        Ok(())
    }

    fn unsigned(&self) -> UnsignedDatasetManifest<'_> {
        UnsignedDatasetManifest {
            protocol: &self.protocol,
            source: &self.source,
            dataset_id: self.dataset_id,
            version: self.version,
            interval_seconds: self.interval_seconds,
            symbols: &self.symbols,
            starts_at: self.starts_at,
            ends_at: self.ends_at,
            files: &self.files,
            package_sha256: &self.package_sha256,
            signer_public_key: &self.signer_public_key,
        }
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
    #[serde(with = "time::serde::rfc3339")]
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
    #[serde(with = "time::serde::rfc3339")]
    pub finalized_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
struct UnsignedArenaInferenceReceipt<'a> {
    protocol: &'a str,
    receipt_id: Uuid,
    arena_id: Uuid,
    run_id: Uuid,
    cycle_id: Uuid,
    model_id: &'a str,
    model_revision: &'a str,
    runtime_digest: &'a str,
    input_sha256: &'a str,
    output_sha256: &'a str,
    tool_calls_sha256: &'a str,
    input_tokens: u64,
    output_tokens: u64,
    amount_microcredits: u64,
    gateway_public_key: &'a str,
    #[serde(with = "time::serde::rfc3339")]
    finalized_at: OffsetDateTime,
}

impl ArenaInferenceReceiptV1 {
    pub fn verify(&self) -> Result<(), ProtocolError> {
        if self.protocol != HARNESS_PROTOCOL_V1
            || self.model_id.is_empty()
            || self.model_revision.is_empty()
            || self.runtime_digest.is_empty()
            || self.runtime_digest.len() > 256
            || !is_sha256(&self.input_sha256)
            || !is_sha256(&self.output_sha256)
            || !is_sha256(&self.tool_calls_sha256)
        {
            return Err(ProtocolError::InvalidInferenceReceipt);
        }
        let unsigned = UnsignedArenaInferenceReceipt {
            protocol: &self.protocol,
            receipt_id: self.receipt_id,
            arena_id: self.arena_id,
            run_id: self.run_id,
            cycle_id: self.cycle_id,
            model_id: &self.model_id,
            model_revision: &self.model_revision,
            runtime_digest: &self.runtime_digest,
            input_sha256: &self.input_sha256,
            output_sha256: &self.output_sha256,
            tool_calls_sha256: &self.tool_calls_sha256,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            amount_microcredits: self.amount_microcredits,
            gateway_public_key: &self.gateway_public_key,
            finalized_at: self.finalized_at,
        };
        verify_signature(
            &self.gateway_public_key,
            &self.gateway_signature,
            &canonical_json(&unsigned)?,
        )
        .map_err(|_| ProtocolError::InvalidInferenceReceipt)
    }
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
    #[serde(with = "time::serde::rfc3339")]
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
    #[serde(with = "time::serde::rfc3339")]
    pub issued_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub controller_device_id: Uuid,
    pub controller_signature: String,
    pub relay_receipt: Option<String>,
}

#[derive(Debug, Serialize)]
struct UnsignedRemoteCommand<'a> {
    protocol: &'a str,
    command_id: Uuid,
    target_device_id: Uuid,
    run_id: Uuid,
    action: RemoteAction,
    nonce: u64,
    #[serde(with = "time::serde::rfc3339")]
    issued_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
    controller_device_id: Uuid,
}

impl RemoteCommandV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn sign(
        signing_key: &SigningKey,
        target_device_id: Uuid,
        run_id: Uuid,
        action: RemoteAction,
        nonce: u64,
        issued_at: OffsetDateTime,
        expires_at: OffsetDateTime,
        controller_device_id: Uuid,
    ) -> Result<Self, ProtocolError> {
        if expires_at <= issued_at {
            return Err(ProtocolError::InvalidRemoteCommand);
        }
        let command_id = Uuid::new_v4();
        let unsigned = UnsignedRemoteCommand {
            protocol: HARNESS_PROTOCOL_V1,
            command_id,
            target_device_id,
            run_id,
            action,
            nonce,
            issued_at,
            expires_at,
            controller_device_id,
        };
        let digest = sha256(&canonical_json(&unsigned)?);
        Ok(Self {
            protocol: HARNESS_PROTOCOL_V1.into(),
            command_id,
            target_device_id,
            run_id,
            action,
            nonce,
            issued_at,
            expires_at,
            controller_device_id,
            controller_signature: URL_SAFE_NO_PAD.encode(signing_key.sign(&digest).to_bytes()),
            relay_receipt: None,
        })
    }

    pub fn verify(
        &self,
        controller_public_key: &str,
        now: OffsetDateTime,
    ) -> Result<(), ProtocolError> {
        if self.protocol != HARNESS_PROTOCOL_V1
            || self.expires_at <= self.issued_at
            || now < self.issued_at
            || now > self.expires_at
        {
            return Err(ProtocolError::InvalidRemoteCommand);
        }
        let unsigned = UnsignedRemoteCommand {
            protocol: &self.protocol,
            command_id: self.command_id,
            target_device_id: self.target_device_id,
            run_id: self.run_id,
            action: self.action,
            nonce: self.nonce,
            issued_at: self.issued_at,
            expires_at: self.expires_at,
            controller_device_id: self.controller_device_id,
        };
        let digest = sha256(&canonical_json(&unsigned)?);
        verify_signature(controller_public_key, &self.controller_signature, &digest)
    }
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
    #[serde(with = "time::serde::rfc3339")]
    pub occurred_at: OffsetDateTime,
    pub payload: Value,
    pub event_sha256: String,
    pub device_public_key: String,
    pub device_signature: String,
    #[serde(default)]
    pub server_receipt: Option<String>,
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
    #[serde(with = "time::serde::rfc3339")]
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
            server_receipt: None,
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
        verify_signature(&self.device_public_key, &self.device_signature, &digest)
    }

    #[must_use]
    pub fn with_server_receipt(mut self, receipt: String) -> Self {
        self.server_receipt = Some(receipt);
        self
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

fn verify_signature(
    public_key: &str,
    encoded_signature: &str,
    message: &[u8],
) -> Result<(), ProtocolError> {
    let public_raw = URL_SAFE_NO_PAD
        .decode(public_key)
        .map_err(|_| ProtocolError::InvalidPublicKey)?;
    let public_array: [u8; 32] = public_raw
        .try_into()
        .map_err(|_| ProtocolError::InvalidPublicKey)?;
    let public =
        VerifyingKey::from_bytes(&public_array).map_err(|_| ProtocolError::InvalidPublicKey)?;
    let signature_raw = URL_SAFE_NO_PAD
        .decode(encoded_signature)
        .map_err(|_| ProtocolError::InvalidSignatureEncoding)?;
    let signature = Signature::from_slice(&signature_raw)
        .map_err(|_| ProtocolError::InvalidSignatureEncoding)?;
    public
        .verify(message, &signature)
        .map_err(|_| ProtocolError::InvalidEventSignature)
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
        let manifest = valid_manifest();
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn signed_manifest_detects_mutation() -> Result<(), ProtocolError> {
        let identity = DeviceIdentity::generate();
        let mut signed = SignedArenaManifestV1::sign(valid_manifest(), identity.signing_key())?;
        signed.manifest.required_client_version = "0.2.0".into();
        assert!(signed.verify().is_err());
        Ok(())
    }

    #[test]
    fn signed_manifest_preserves_operator_timestamp_bytes() -> Result<(), ProtocolError> {
        let identity = DeviceIdentity::generate();
        let mut manifest =
            serde_json::to_value(valid_manifest()).map_err(ProtocolError::CanonicalJson)?;
        manifest["starts_at"] = json!("2026-07-01T00:00:00.000Z");
        manifest["ends_at"] = json!("2026-07-02T00:00:00.000Z");
        let digest = sha256(&canonical_json(&manifest)?);
        let signed = SignedArenaManifestV1::from_signed_value(
            manifest,
            hex::encode(digest),
            identity.public_key(),
            identity.sign_bytes(&digest),
        )?;

        signed.verify()?;
        assert_ne!(
            signed.manifest_sha256,
            hex::encode(sha256(&canonical_json(&signed.manifest)?))
        );

        let encoded = serde_json::to_vec(&signed).map_err(ProtocolError::CanonicalJson)?;
        let restored = serde_json::from_slice::<SignedArenaManifestV1>(&encoded)
            .map_err(ProtocolError::CanonicalJson)?;
        restored.verify()
    }

    #[test]
    fn remote_command_binds_expiry_and_action() -> Result<(), ProtocolError> {
        let identity = DeviceIdentity::generate();
        let issued_at = datetime!(2026-07-01 00:00 UTC);
        let expires_at = datetime!(2026-07-01 00:00:05 UTC);
        let mut command = RemoteCommandV1::sign(
            identity.signing_key(),
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            RemoteAction::Stop,
            1,
            issued_at,
            expires_at,
            Uuid::from_u128(3),
        )?;
        command.verify(&identity.public_key(), datetime!(2026-07-01 00:00:03 UTC))?;
        command.action = RemoteAction::Resume;
        assert!(
            command
                .verify(&identity.public_key(), datetime!(2026-07-01 00:00:03 UTC))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn inference_receipt_signature_binds_accounting_and_hashes() -> Result<(), ProtocolError> {
        let identity = DeviceIdentity::generate();
        let mut receipt = ArenaInferenceReceiptV1 {
            protocol: HARNESS_PROTOCOL_V1.into(),
            receipt_id: Uuid::from_u128(1),
            arena_id: Uuid::from_u128(2),
            run_id: Uuid::from_u128(3),
            cycle_id: Uuid::from_u128(4),
            model_id: ALLOWED_MODELS[0].into(),
            model_revision: "revision".into(),
            runtime_digest: "managed-capacity/v1".into(),
            input_sha256: "1".repeat(64),
            output_sha256: "2".repeat(64),
            tool_calls_sha256: "3".repeat(64),
            input_tokens: 100,
            output_tokens: 20,
            amount_microcredits: 120,
            gateway_public_key: identity.public_key(),
            gateway_signature: String::new(),
            finalized_at: datetime!(2026-07-01 00:00 UTC),
        };
        let unsigned = UnsignedArenaInferenceReceipt {
            protocol: &receipt.protocol,
            receipt_id: receipt.receipt_id,
            arena_id: receipt.arena_id,
            run_id: receipt.run_id,
            cycle_id: receipt.cycle_id,
            model_id: &receipt.model_id,
            model_revision: &receipt.model_revision,
            runtime_digest: &receipt.runtime_digest,
            input_sha256: &receipt.input_sha256,
            output_sha256: &receipt.output_sha256,
            tool_calls_sha256: &receipt.tool_calls_sha256,
            input_tokens: receipt.input_tokens,
            output_tokens: receipt.output_tokens,
            amount_microcredits: receipt.amount_microcredits,
            gateway_public_key: &receipt.gateway_public_key,
            finalized_at: receipt.finalized_at,
        };
        receipt.gateway_signature = URL_SAFE_NO_PAD.encode(
            identity
                .signing_key()
                .sign(&canonical_json(&unsigned)?)
                .to_bytes(),
        );
        receipt.verify()?;
        receipt.amount_microcredits += 1;
        assert!(receipt.verify().is_err());
        Ok(())
    }

    fn valid_manifest() -> ArenaManifestV1 {
        ArenaManifestV1 {
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
        }
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
