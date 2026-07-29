use crate::ExecutionGate;
use crow_agent_core::{
    DeviceEncryptionKey, DurableRunEventWriter, EncryptedJournal, GatewayClient, HarnessApiClient,
    HyperliquidBookStream, HyperliquidVenue, LiveRiskState, StartHarnessRunV1, execute_live_cycle,
    load_live_risk_state, open_agent_version, reconcile_live_state, store_live_risk_state,
};
use crow_agent_protocol::{
    ArenaMode, DeviceIdentity, SignedArenaManifestV1, canonical_json, sha256,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use thiserror::Error;
use time::OffsetDateTime;
use tracing::{info, warn};
use uuid::Uuid;
use zeroize::Zeroizing;

const RUN_ID_PREFIX: &str = "live-run-id";
const RUN_LEASE_PREFIX: &str = "live-run-lease";

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LiveArenaConfig {
    pub arena_manifest: PathBuf,
    #[serde(default)]
    pub handoff_snapshot: Option<PathBuf>,
    pub agent_version_id: Uuid,
    pub execution_account: String,
    pub model_id: String,
    pub client_release: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedLiveArena {
    pub signed: SignedArenaManifestV1,
    pub handoff_snapshot: Option<Value>,
    pub agent_version_id: Uuid,
    pub execution_account: String,
    pub model_id: String,
    pub client_release: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveSessionOutcome {
    Stopped,
}

#[derive(Debug, Error)]
pub(crate) enum LiveRunError {
    #[error("live arena configuration is invalid")]
    Configuration,
    #[error("signed live arena manifest is invalid")]
    Manifest,
    #[error("live arena control plane is unavailable")]
    ControlPlane(#[from] crow_agent_core::HarnessApiError),
    #[error("live arena venue is unavailable")]
    Venue(#[from] crow_agent_core::HyperliquidError),
    #[error("live arena journal is unavailable")]
    Journal(#[from] crow_agent_core::JournalError),
    #[error("live arena event path is unavailable")]
    Event(#[from] crow_agent_core::DurableRunEventError),
    #[error("live arena cycle failed closed")]
    Cycle(#[from] crow_agent_core::LiveCycleError),
    #[error("live arena gateway is unavailable")]
    Gateway(#[from] crow_agent_core::GatewayError),
    #[error("live arena state is invalid")]
    State,
}

pub(crate) fn prepare(config: LiveArenaConfig) -> Result<PreparedLiveArena, LiveRunError> {
    let signed = serde_json::from_slice::<SignedArenaManifestV1>(
        &fs::read(&config.arena_manifest).map_err(|_| LiveRunError::Configuration)?,
    )
    .map_err(|_| LiveRunError::Manifest)?;
    signed.verify().map_err(|_| LiveRunError::Manifest)?;
    let manifest = &signed.manifest;
    if manifest.mode != ArenaMode::HyperliquidTestnet
        || !manifest
            .eligible_models
            .iter()
            .any(|model| model == &config.model_id)
        || config.agent_version_id == Uuid::nil()
        || config.client_release.trim().is_empty()
        || !valid_execution_account(&config.execution_account)
    {
        return Err(LiveRunError::Configuration);
    }
    let handoff_snapshot = config
        .handoff_snapshot
        .map(|path| {
            serde_json::from_slice::<Value>(
                &fs::read(path).map_err(|_| LiveRunError::Configuration)?,
            )
            .map_err(|_| LiveRunError::Configuration)
        })
        .transpose()?;
    if handoff_snapshot
        .as_ref()
        .is_some_and(|snapshot| !snapshot.is_object() || !fixed_point_value(snapshot))
    {
        return Err(LiveRunError::Configuration);
    }
    Ok(PreparedLiveArena {
        signed,
        handoff_snapshot,
        agent_version_id: config.agent_version_id,
        execution_account: config.execution_account.to_ascii_lowercase(),
        model_id: config.model_id,
        client_release: config.client_release,
    })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) async fn run_session(
    config: &PreparedLiveArena,
    api_origin: &str,
    state_directory: &Path,
    journal_key: [u8; 32],
    api_wallet_key: &Zeroizing<[u8; 32]>,
    device_id: Uuid,
    device_encryption_key: &DeviceEncryptionKey,
    access_token: &Zeroizing<String>,
    identity: &DeviceIdentity,
    execution_gate: &ExecutionGate,
    active_run: &Arc<Mutex<Option<Uuid>>>,
) -> Result<LiveSessionOutcome, LiveRunError> {
    let manifest = &config.signed.manifest;
    let now = OffsetDateTime::now_utc();
    if now >= manifest.ends_at {
        return Err(LiveRunError::Configuration);
    }
    let expected_cycles = expected_cycle_count(manifest)?;
    let journal_path = state_directory.join("journal.db");
    let mut journal = EncryptedJournal::open(&journal_path, journal_key)?;
    let api = HarnessApiClient::new(api_origin, access_token.as_str())?;
    let envelope = api.agent_version(config.agent_version_id).await?;
    let strategy = open_agent_version(&envelope, device_id, device_encryption_key)
        .map_err(|_| LiveRunError::Configuration)?;
    if strategy.model_id != config.model_id {
        return Err(LiveRunError::Configuration);
    }
    let run_id = acquire_run(config, &journal, &api).await?;
    *active_run.lock().map_err(|_| LiveRunError::State)? = Some(run_id);
    let venue = HyperliquidVenue::connect_testnet(api_wallet_key).await?;

    initialize_event_chain(config, run_id, &mut journal, &api, identity).await?;
    let mut completed_cycles = journal.event_count(run_id, "cycle_started")?;
    if completed_cycles > expected_cycles {
        return Err(LiveRunError::State);
    }
    let mut risk = if let Some(state) = load_live_risk_state(&journal, run_id)? {
        state
    } else {
        let account = venue.account_snapshot(&config.execution_account).await?;
        if account.equity_micro_usdc <= 0 {
            return Err(LiveRunError::State);
        }
        let state = LiveRiskState {
            trading_day: now.date(),
            trading_day_start_equity_micro_usdc: account.equity_micro_usdc,
            peak_equity_micro_usdc: account.equity_micro_usdc,
            orders_today: 0,
            last_reconciliation_ms: unix_milliseconds(now)?,
        };
        store_live_risk_state(&journal, run_id, &state)?;
        state
    };
    reconcile_live_state(
        &mut journal,
        &api,
        identity,
        manifest.arena_id,
        run_id,
        &config.execution_account,
        &venue,
        &mut risk,
    )
    .await?;
    let mut stream = HyperliquidBookStream::connect_testnet()?;
    let initial = stream.reconcile().await?;
    let mut books = initial
        .into_iter()
        .map(|book| (book.symbol.clone(), book))
        .collect::<BTreeMap<_, _>>();
    let mut lease_tick = tokio::time::interval(Duration::from_secs(10));
    lease_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut lifecycle_tick = tokio::time::interval(Duration::from_millis(200));
    lifecycle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut cycle_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + duration_until_next_cycle(manifest)?,
        Duration::from_secs(u64::from(manifest.decision_interval_seconds)),
    );
    cycle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut recorded_gate = "paused";

    info!(arena_id = %manifest.arena_id, run_id = %run_id, "live arena session reconciled and fail-closed");
    loop {
        tokio::select! {
            _ = lease_tick.tick() => {
                let lease = load_secret_string(&journal, &lease_key(manifest.arena_id))?;
                api.renew_lease(run_id, &Zeroizing::new(lease)).await?;
            }
            _ = lifecycle_tick.tick() => {
                let current = execution_gate.label();
                if current != recorded_gate {
                    let event_type = match current {
                        "running" => "run_resumed",
                        "paused" => "run_paused",
                        "stopped" => "run_stopped",
                        _ => return Err(LiveRunError::State),
                    };
                    append_lifecycle(&mut journal, &api, identity, manifest.arena_id, run_id, event_type).await?;
                    recorded_gate = current;
                    if current == "stopped" {
                        journal.delete_secret(&run_id_key(manifest.arena_id))?;
                        journal.delete_secret(&lease_key(manifest.arena_id))?;
                        *active_run.lock().map_err(|_| LiveRunError::State)? = None;
                        return Ok(LiveSessionOutcome::Stopped);
                    }
                }
            }
            _ = cycle_tick.tick() => {
                // The wall-clock tick can wake just before the signed end and
                // spend time reconciling before the cycle event is appended.
                // The immutable schedule and durable journal are authoritative:
                // never begin more cycles than the manifest contains.
                if completed_cycles >= expected_cycles || OffsetDateTime::now_utc() >= manifest.ends_at {
                    append_lifecycle(&mut journal, &api, identity, manifest.arena_id, run_id, "run_stopped").await?;
                    journal.delete_secret(&run_id_key(manifest.arena_id))?;
                    journal.delete_secret(&lease_key(manifest.arena_id))?;
                    *active_run.lock().map_err(|_| LiveRunError::State)? = None;
                    return Ok(LiveSessionOutcome::Stopped);
                }
                if !execution_gate.is_running() {
                    continue;
                }
                let snapshot_books = books.values().cloned().collect::<Vec<_>>();
                let gateway = GatewayClient::new(api_origin, access_token.as_str())?;
                let result = execute_live_cycle(
                    &mut journal,
                    &api,
                    identity,
                    manifest,
                    run_id,
                    &config.model_id,
                    &strategy.system_instructions,
                    &config.execution_account,
                    &venue,
                    snapshot_books,
                    gateway,
                    &mut risk,
                    || execution_gate.is_running(),
                ).await?;
                info!(
                    cycle_id = %result.cycle_id,
                    symbol = result.proposal_symbol,
                    submitted = result.order_submitted,
                    "live arena cycle durably accepted"
                );
                completed_cycles = completed_cycles.checked_add(1).ok_or(LiveRunError::State)?;
            }
            snapshot = stream.next_snapshot() => {
                match snapshot {
                    Ok(snapshot) => {
                        books.insert(snapshot.symbol.clone(), snapshot);
                    }
                    Err(error) => {
                        warn!(error = %error, "Hyperliquid book stream interrupted; reconciling before reconnect");
                        books = stream.reconcile().await?
                            .into_iter()
                            .map(|book| (book.symbol.clone(), book))
                            .collect();
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        stream = HyperliquidBookStream::connect_testnet()?;
                    }
                }
            }
        }
    }
}

async fn acquire_run(
    config: &PreparedLiveArena,
    journal: &EncryptedJournal,
    api: &HarnessApiClient,
) -> Result<Uuid, LiveRunError> {
    let arena_id = config.signed.manifest.arena_id;
    let existing_id = journal.secret(&run_id_key(arena_id))?;
    let existing_lease = journal.secret(&lease_key(arena_id))?;
    match (existing_id, existing_lease) {
        (Some(id), Some(lease)) => {
            let id = std::str::from_utf8(&id)
                .ok()
                .and_then(|value| Uuid::parse_str(value).ok())
                .ok_or(LiveRunError::State)?;
            let lease = std::str::from_utf8(&lease)
                .map_err(|_| LiveRunError::State)?
                .to_owned();
            api.renew_lease(id, &Zeroizing::new(lease)).await?;
            Ok(id)
        }
        (None, None) => {
            let started = api
                .start_run(&StartHarnessRunV1 {
                    arena_id,
                    agent_version_id: config.agent_version_id,
                    execution_account: config.execution_account.clone(),
                    client_release: config.client_release.clone(),
                    handoff_snapshot: config.handoff_snapshot.clone(),
                })
                .await?;
            journal.put_secret(&run_id_key(arena_id), started.run.id.to_string().as_bytes())?;
            journal.put_secret(&lease_key(arena_id), started.lease_token.as_bytes())?;
            Ok(started.run.id)
        }
        _ => Err(LiveRunError::State),
    }
}

async fn initialize_event_chain(
    config: &PreparedLiveArena,
    run_id: Uuid,
    journal: &mut EncryptedJournal,
    api: &HarnessApiClient,
    identity: &DeviceIdentity,
) -> Result<(), LiveRunError> {
    let arena_id = config.signed.manifest.arena_id;
    if journal.latest_event_state(run_id)?.is_some() {
        let writer = DurableRunEventWriter::new(journal, api, identity, arena_id, run_id);
        writer.flush_pending().await?;
        return Ok(());
    }
    let manifest_sha256 = hex::encode(sha256(
        &canonical_json(&config.signed.manifest).map_err(|_| LiveRunError::Manifest)?,
    ));
    let mut writer = DurableRunEventWriter::new(journal, api, identity, arena_id, run_id);
    writer
        .append(
            None,
            "run_started",
            json!({
                "client_release": config.client_release,
                "manifest_sha256": manifest_sha256,
                "mode": "hyperliquid_testnet",
            }),
            &Value::Null,
        )
        .await?;
    if let Some(handoff_snapshot) = &config.handoff_snapshot {
        writer
            .append(
                None,
                "handoff_snapshot",
                handoff_snapshot.clone(),
                &Value::Null,
            )
            .await?;
    }
    writer
        .append(
            None,
            "run_paused",
            json!({"reason": "awaiting_explicit_resume"}),
            &Value::Null,
        )
        .await?;
    Ok(())
}

async fn append_lifecycle(
    journal: &mut EncryptedJournal,
    api: &HarnessApiClient,
    identity: &DeviceIdentity,
    arena_id: Uuid,
    run_id: Uuid,
    event_type: &str,
) -> Result<(), LiveRunError> {
    let mut writer = DurableRunEventWriter::new(journal, api, identity, arena_id, run_id);
    writer
        .append(
            None,
            event_type,
            json!({"source": "authenticated_control"}),
            &Value::Null,
        )
        .await?;
    Ok(())
}

fn duration_until_next_cycle(
    manifest: &crow_agent_protocol::ArenaManifestV1,
) -> Result<Duration, LiveRunError> {
    let now = OffsetDateTime::now_utc();
    if now >= manifest.ends_at {
        return Err(LiveRunError::Configuration);
    }
    if now < manifest.starts_at {
        return Duration::try_from(manifest.starts_at - now).map_err(|_| LiveRunError::State);
    }
    let interval = i64::from(manifest.decision_interval_seconds);
    let elapsed = (now - manifest.starts_at).whole_seconds();
    let next_offset = (elapsed.div_euclid(interval) + 1)
        .checked_mul(interval)
        .ok_or(LiveRunError::State)?;
    let next = manifest.starts_at + time::Duration::seconds(next_offset);
    Duration::try_from(next - now).map_err(|_| LiveRunError::State)
}

fn expected_cycle_count(
    manifest: &crow_agent_protocol::ArenaManifestV1,
) -> Result<u64, LiveRunError> {
    let interval = i64::from(manifest.decision_interval_seconds);
    let duration = (manifest.ends_at - manifest.starts_at).whole_seconds();
    if interval <= 0 || duration <= 0 || duration % interval != 0 {
        return Err(LiveRunError::Configuration);
    }
    u64::try_from(duration / interval).map_err(|_| LiveRunError::State)
}

fn load_secret_string(journal: &EncryptedJournal, name: &str) -> Result<String, LiveRunError> {
    let value = journal.secret(name)?.ok_or(LiveRunError::State)?;
    std::str::from_utf8(&value)
        .map(str::to_owned)
        .map_err(|_| LiveRunError::State)
}

fn run_id_key(arena_id: Uuid) -> String {
    format!("{RUN_ID_PREFIX}-{arena_id}")
}

fn lease_key(arena_id: Uuid) -> String {
    format!("{RUN_LEASE_PREFIX}-{arena_id}")
}

fn valid_execution_account(value: &str) -> bool {
    value.len() == 42
        && value.starts_with("0x")
        && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn fixed_point_value(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(_) | Value::String(_) => true,
        Value::Number(number) => number.as_i64().is_some() || number.as_u64().is_some(),
        Value::Array(values) => values.iter().all(fixed_point_value),
        Value::Object(values) => values.values().all(fixed_point_value),
    }
}

fn unix_milliseconds(value: OffsetDateTime) -> Result<u64, LiveRunError> {
    u64::try_from(value.unix_timestamp_nanos() / 1_000_000).map_err(|_| LiveRunError::State)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_account_is_strict_hex_address() {
        assert!(valid_execution_account(
            "0x0000000000000000000000000000000000000042"
        ));
        assert!(!valid_execution_account(
            "0x000000000000000000000000000000000000004z"
        ));
        assert!(!valid_execution_account("0x42"));
    }

    #[test]
    fn handoff_snapshot_requires_fixed_point_structured_json() {
        assert!(fixed_point_value(&json!({
            "equity_micro_usdc": 1_000_000,
            "positions": [{"symbol": "BTC", "quantity_e8": -42}]
        })));
        assert!(!fixed_point_value(&json!({"equity": 1.25})));
    }

    #[test]
    fn expected_cycles_excludes_the_end_boundary() -> Result<(), Box<dyn std::error::Error>> {
        let mut manifest = serde_json::from_value::<crow_agent_protocol::ArenaManifestV1>(json!({
            "protocol": "crow.harness.v1",
            "arena_id": Uuid::new_v4(),
            "manifest_version": 1,
            "mode": "hyperliquid_testnet",
            "starts_at": "2026-07-29T13:30:00Z",
            "ends_at": "2026-07-29T14:00:00Z",
            "decision_interval_seconds": 900,
            "symbols": ["BTC", "ETH", "SOL"],
            "eligible_models": ["crow-qwen3-5-27b"],
            "dataset_sha256": null,
            "required_client_version": "0.1.10",
            "risk_rules": {
                "cash_reserve_bps": 1000,
                "daily_loss_bps": 200,
                "drawdown_bps": 1000,
                "max_order_bps": 200,
                "max_position_bps": 1000,
                "max_spread_bps": 40,
                "max_oracle_gap_bps": 100,
                "book_max_age_seconds": 10,
                "max_orders_day": 20,
                "isolated_leverage": 1,
                "long_only": true,
                "ioc_only": true
            },
            "execution": {"half_spread_bps": 2, "slippage_bps": 3, "taker_fee_bps": 5},
            "scoring": {"net_return": 50, "sortino": 30, "inverse_drawdown": 20},
            "penalties": {
                "policy_rejection_millis": 1000,
                "missed_cycle_millis": 250,
                "cap_millis": 15000
            },
            "ticket": {
                "enabled": false,
                "usdc_address": null,
                "ticket_micro_usdc": 0,
                "participant_cap": 0,
                "prize_bps": 9000,
                "protocol_bps": 1000,
                "winner_bps": [5000, 3000, 2000]
            }
        }))?;
        assert_eq!(expected_cycle_count(&manifest)?, 2);
        manifest.ends_at += time::Duration::seconds(1);
        assert!(expected_cycle_count(&manifest).is_err());
        Ok(())
    }
}
