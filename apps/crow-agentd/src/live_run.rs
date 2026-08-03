use crate::{ExecutionGate, PendingStrategy};
use crow_agent_core::{
    DeviceEncryptionKey, DurableRunEventWriter, EncryptedJournal, GatewayClient, HarnessApiClient,
    HyperliquidBookStream, HyperliquidVenue, LiveRiskState, RotatingAccessToken, StartHarnessRunV1,
    execute_live_cycle, load_live_risk_state, open_agent_version, reconcile_live_state,
    store_live_risk_state,
};
use crow_agent_protocol::{ArenaMode, DeviceIdentity, SignedArenaManifestV1};
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
    access_token: RotatingAccessToken,
    identity: &DeviceIdentity,
    execution_gate: &ExecutionGate,
    active_run: &Arc<Mutex<Option<Uuid>>>,
    pending_strategy: &PendingStrategy,
) -> Result<LiveSessionOutcome, LiveRunError> {
    let manifest = &config.signed.manifest;
    let now = OffsetDateTime::now_utc();
    if now >= manifest.ends_at {
        return Err(LiveRunError::Configuration);
    }
    let expected_cycles = expected_cycle_count(manifest)?;
    let journal_path = state_directory.join("journal.db");
    let mut journal = EncryptedJournal::open(&journal_path, journal_key)?;
    let api = HarnessApiClient::with_access_token(api_origin, access_token.clone())?;
    let run_id = acquire_run(config, &journal, &api).await?;
    let active_strategy_version = journal
        .secret(&strategy_key(run_id))?
        .and_then(|value| std::str::from_utf8(&value).ok()?.parse::<Uuid>().ok())
        .unwrap_or(config.agent_version_id);
    let envelope = api.agent_version(active_strategy_version).await?;
    let mut strategy = open_agent_version(&envelope, device_id, device_encryption_key)
        .map_err(|_| LiveRunError::Configuration)?;
    if strategy.model_id != config.model_id {
        return Err(LiveRunError::Configuration);
    }
    let venue = HyperliquidVenue::connect_testnet(api_wallet_key).await?;

    initialize_event_chain(config, run_id, &mut journal, &api, identity).await?;
    let mut completed_cycles = journal
        .event_count(run_id, "cycle_started")?
        .checked_add(journal.event_count(run_id, "missed_cycle")?)
        .ok_or(LiveRunError::State)?;
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
    let due_cycles = due_cycle_count(manifest, OffsetDateTime::now_utc())?;
    while completed_cycles < due_cycles {
        append_missed_cycle(
            &mut journal,
            &api,
            identity,
            manifest.arena_id,
            run_id,
            scheduled_cycle_at(manifest, completed_cycles)?,
            "companion_unavailable_at_boundary",
        )
        .await?;
        completed_cycles = completed_cycles.checked_add(1).ok_or(LiveRunError::State)?;
    }
    if completed_cycles >= expected_cycles {
        append_lifecycle(
            &mut journal,
            &api,
            identity,
            manifest.arena_id,
            run_id,
            "run_stopped",
            "arena_schedule",
        )
        .await?;
        journal.delete_secret(&run_id_key(manifest.arena_id))?;
        journal.delete_secret(&lease_key(manifest.arena_id))?;
        journal.delete_secret(&strategy_key(run_id))?;
        *active_run.lock().map_err(|_| LiveRunError::State)? = None;
        return Ok(LiveSessionOutcome::Stopped);
    }
    let mut stream = HyperliquidBookStream::connect_testnet()?;
    let initial = stream.reconcile().await?;
    let mut books = initial
        .into_iter()
        .map(|book| (book.symbol.clone(), book))
        .collect::<BTreeMap<_, _>>();
    let resume_after_reconciliation = should_run_after_reconciliation(&journal, run_id)?;
    let mut recorded_gate = "paused";
    if resume_after_reconciliation {
        if !execution_gate.apply(crow_agent_protocol::RemoteAction::Resume)
            && !execution_gate.is_running()
        {
            return Err(LiveRunError::State);
        }
        append_lifecycle(
            &mut journal,
            &api,
            identity,
            manifest.arena_id,
            run_id,
            "run_resumed",
            "automatic_after_reconciliation",
        )
        .await?;
        recorded_gate = "running";
    }
    *active_run.lock().map_err(|_| LiveRunError::State)? = Some(run_id);
    let mut lease_tick = tokio::time::interval(Duration::from_secs(10));
    lease_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut lifecycle_tick = tokio::time::interval(Duration::from_millis(200));
    lifecycle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut cycle_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + duration_until_cycle(manifest, completed_cycles)?,
        Duration::from_secs(u64::from(manifest.decision_interval_seconds)),
    );
    cycle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    info!(
        arena_id = %manifest.arena_id,
        run_id = %run_id,
        execution_state = recorded_gate,
        "live arena session reconciled"
    );
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
                    append_lifecycle(
                        &mut journal,
                        &api,
                        identity,
                        manifest.arena_id,
                        run_id,
                        event_type,
                        "authenticated_control",
                    ).await?;
                    recorded_gate = current;
                    if current == "stopped" {
                        journal.delete_secret(&run_id_key(manifest.arena_id))?;
                        journal.delete_secret(&lease_key(manifest.arena_id))?;
                        journal.delete_secret(&strategy_key(run_id))?;
                        *active_run.lock().map_err(|_| LiveRunError::State)? = None;
                        return Ok(LiveSessionOutcome::Stopped);
                    }
                }
                if execution_gate.label() == "paused" {
                    let candidate = pending_strategy
                        .lock()
                        .map_err(|_| LiveRunError::State)?
                        .clone();
                    if let Some(candidate) = candidate {
                        if candidate.model_id != config.model_id
                            || candidate.validate().is_err()
                            || !manifest.eligible_models.iter().any(|model| model == &candidate.model_id)
                        {
                            return Err(LiveRunError::Configuration);
                        }
                        api.switch_run_strategy(run_id, candidate.version_id).await?;
                        journal.put_secret(
                            &strategy_key(run_id),
                            candidate.version_id.to_string().as_bytes(),
                        )?;
                        let mut writer = DurableRunEventWriter::new(
                            &mut journal,
                            &api,
                            identity,
                            manifest.arena_id,
                            run_id,
                        );
                        writer.append(
                            None,
                            "strategy_changed",
                            json!({
                                "previous_agent_version_id": strategy.version_id,
                                "agent_version_id": candidate.version_id,
                            }),
                            &Value::Null,
                        ).await?;
                        strategy = candidate;
                        *pending_strategy.lock().map_err(|_| LiveRunError::State)? = None;
                        if !execution_gate.apply(crow_agent_protocol::RemoteAction::Resume) {
                            return Err(LiveRunError::State);
                        }
                    }
                }
            }
            _ = cycle_tick.tick() => {
                // The wall-clock tick can wake just before the signed end and
                // spend time reconciling before the cycle event is appended.
                // The immutable schedule and durable journal are authoritative:
                // never begin more cycles than the manifest contains.
                if completed_cycles >= expected_cycles || OffsetDateTime::now_utc() >= manifest.ends_at {
                    append_lifecycle(
                        &mut journal,
                        &api,
                        identity,
                        manifest.arena_id,
                        run_id,
                        "run_stopped",
                        "arena_schedule",
                    ).await?;
                    journal.delete_secret(&run_id_key(manifest.arena_id))?;
                    journal.delete_secret(&lease_key(manifest.arena_id))?;
                    journal.delete_secret(&strategy_key(run_id))?;
                    *active_run.lock().map_err(|_| LiveRunError::State)? = None;
                    return Ok(LiveSessionOutcome::Stopped);
                }
                if !execution_gate.is_running() {
                    append_missed_cycle(
                        &mut journal,
                        &api,
                        identity,
                        manifest.arena_id,
                        run_id,
                        scheduled_cycle_at(manifest, completed_cycles)?,
                        "execution_paused_at_boundary",
                    ).await?;
                    completed_cycles = completed_cycles.checked_add(1).ok_or(LiveRunError::State)?;
                    continue;
                }
                let snapshot_books = books.values().cloned().collect::<Vec<_>>();
                let gateway = GatewayClient::with_access_token(api_origin, access_token.clone())?;
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
                ).await;
                match result {
                    Ok(result) => info!(
                        cycle_id = %result.cycle_id,
                        symbol = result.proposal_symbol,
                        submitted = result.order_submitted,
                        "live arena cycle durably accepted"
                    ),
                    Err(crow_agent_core::LiveCycleError::Runtime(error)) => warn!(
                        failure_class = error.failure_class(),
                        "live arena model decision failed safely; continuing on the signed schedule"
                    ),
                    Err(error) => return Err(error.into()),
                }
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
    let manifest_sha256 = config.signed.manifest_sha256.clone();
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
    source: &str,
) -> Result<(), LiveRunError> {
    let mut writer = DurableRunEventWriter::new(journal, api, identity, arena_id, run_id);
    writer
        .append(None, event_type, json!({"source": source}), &Value::Null)
        .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn append_missed_cycle(
    journal: &mut EncryptedJournal,
    api: &HarnessApiClient,
    identity: &DeviceIdentity,
    arena_id: Uuid,
    run_id: Uuid,
    scheduled_at: OffsetDateTime,
    reason: &str,
) -> Result<(), LiveRunError> {
    let cycle_id = Uuid::new_v4();
    let mut writer = DurableRunEventWriter::new(journal, api, identity, arena_id, run_id);
    writer
        .append(
            Some(cycle_id),
            "missed_cycle",
            json!({
                "scheduled_at": scheduled_at,
                "reason": reason,
                "order_submitted": false,
            }),
            &Value::Null,
        )
        .await?;
    Ok(())
}

fn should_run_after_reconciliation(
    journal: &EncryptedJournal,
    run_id: Uuid,
) -> Result<bool, LiveRunError> {
    let latest = journal
        .public_events()?
        .into_iter()
        .filter(|event| {
            event.run_id == run_id
                && matches!(
                    event.event_type.as_str(),
                    "run_started" | "run_paused" | "run_resumed" | "run_stopped"
                )
        })
        .max_by_key(|event| event.sequence);
    Ok(match latest {
        Some(event) if event.event_type == "run_started" => true,
        Some(event) if event.event_type == "run_resumed" => true,
        Some(event) if event.event_type == "run_paused" => {
            event.payload.get("reason").and_then(Value::as_str) == Some("awaiting_explicit_resume")
        }
        _ => false,
    })
}

fn duration_until_cycle(
    manifest: &crow_agent_protocol::ArenaManifestV1,
    completed_cycles: u64,
) -> Result<Duration, LiveRunError> {
    let now = OffsetDateTime::now_utc();
    let next = scheduled_cycle_at(manifest, completed_cycles)?;
    if next <= now {
        return Ok(Duration::ZERO);
    }
    Duration::try_from(next - now).map_err(|_| LiveRunError::State)
}

fn scheduled_cycle_at(
    manifest: &crow_agent_protocol::ArenaManifestV1,
    cycle_index: u64,
) -> Result<OffsetDateTime, LiveRunError> {
    let interval = i64::from(manifest.decision_interval_seconds);
    let cycle_index = i64::try_from(cycle_index).map_err(|_| LiveRunError::State)?;
    let offset = interval
        .checked_mul(cycle_index)
        .ok_or(LiveRunError::State)?;
    Ok(manifest.starts_at + time::Duration::seconds(offset))
}

fn due_cycle_count(
    manifest: &crow_agent_protocol::ArenaManifestV1,
    now: OffsetDateTime,
) -> Result<u64, LiveRunError> {
    let expected = expected_cycle_count(manifest)?;
    if now < manifest.starts_at {
        return Ok(0);
    }
    let interval = i64::from(manifest.decision_interval_seconds);
    let elapsed = (now.min(manifest.ends_at) - manifest.starts_at).whole_seconds();
    let due = elapsed.div_euclid(interval).saturating_add(1);
    Ok(u64::try_from(due)
        .map_err(|_| LiveRunError::State)?
        .min(expected))
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

fn strategy_key(run_id: Uuid) -> String {
    format!("active-strategy-version-{run_id}")
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
    use tempfile::tempdir;

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
    fn reconciliation_resumes_initial_and_running_states_but_preserves_user_pause()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let identity = DeviceIdentity::generate();
        let arena_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let mut journal =
            EncryptedJournal::open(&directory.path().join("journal.db"), [47_u8; 32])?;
        let started = crow_agent_protocol::RunEventEnvelopeV1::sign(
            identity.signing_key(),
            arena_id,
            run_id,
            None,
            1,
            "0".repeat(64),
            "run_started".into(),
            OffsetDateTime::now_utc(),
            json!({}),
        )?;
        journal.append(&started, &Value::Null)?;
        assert!(should_run_after_reconciliation(&journal, run_id)?);

        let initial_hold = crow_agent_protocol::RunEventEnvelopeV1::sign(
            identity.signing_key(),
            arena_id,
            run_id,
            None,
            2,
            started.event_sha256.clone(),
            "run_paused".into(),
            OffsetDateTime::now_utc(),
            json!({"reason": "awaiting_explicit_resume"}),
        )?;
        journal.append(&initial_hold, &Value::Null)?;
        assert!(should_run_after_reconciliation(&journal, run_id)?);

        let resumed = crow_agent_protocol::RunEventEnvelopeV1::sign(
            identity.signing_key(),
            arena_id,
            run_id,
            None,
            3,
            initial_hold.event_sha256.clone(),
            "run_resumed".into(),
            OffsetDateTime::now_utc(),
            json!({"source": "automatic_after_reconciliation"}),
        )?;
        journal.append(&resumed, &Value::Null)?;
        assert!(should_run_after_reconciliation(&journal, run_id)?);

        let user_pause = crow_agent_protocol::RunEventEnvelopeV1::sign(
            identity.signing_key(),
            arena_id,
            run_id,
            None,
            4,
            resumed.event_sha256.clone(),
            "run_paused".into(),
            OffsetDateTime::now_utc(),
            json!({"source": "authenticated_control"}),
        )?;
        journal.append(&user_pause, &Value::Null)?;
        assert!(!should_run_after_reconciliation(&journal, run_id)?);
        Ok(())
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
        assert_eq!(
            due_cycle_count(&manifest, manifest.starts_at - time::Duration::seconds(1))?,
            0
        );
        assert_eq!(due_cycle_count(&manifest, manifest.starts_at)?, 1);
        assert_eq!(
            due_cycle_count(&manifest, manifest.starts_at + time::Duration::seconds(899))?,
            1
        );
        assert_eq!(
            due_cycle_count(&manifest, manifest.starts_at + time::Duration::seconds(900))?,
            2
        );
        assert_eq!(due_cycle_count(&manifest, manifest.ends_at)?, 2);
        assert_eq!(
            scheduled_cycle_at(&manifest, 1)?,
            manifest.starts_at + time::Duration::seconds(900)
        );
        manifest.ends_at += time::Duration::seconds(1);
        assert!(expected_cycle_count(&manifest).is_err());
        Ok(())
    }
}
