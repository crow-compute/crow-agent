use crate::ExecutionGate;
use crow_agent_core::EncryptedJournal;
use crow_agent_protocol::{DeviceIdentity, HARNESS_PROTOCOL_V1, RemoteAction, RunEventEnvelopeV1};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

const SYNTHETIC_PRIVATE_SENTINEL: &str = "crow-soak-private-payload-sentinel";
const SYNTHETIC_TOKEN_SENTINEL: &str = "crow_soak_refresh_token_sentinel";
const JOURNAL_KEY: [u8; 32] = [0x5a; 32];
const DEVICE_SEED: [u8; 32] = [0x33; 32];

#[derive(Debug, Error)]
pub enum SoakError {
    #[error("soak duration and interval are invalid")]
    InvalidSchedule,
    #[error("soak state filesystem failed")]
    Io(#[from] std::io::Error),
    #[error("soak journal failed")]
    Journal(#[from] crow_agent_core::JournalError),
    #[error("soak protocol event failed")]
    Protocol(#[from] crow_agent_protocol::ProtocolError),
    #[error("soak report encoding failed")]
    Json(#[from] serde_json::Error),
    #[error("soak fault injection was unexpectedly accepted")]
    FaultAccepted,
    #[error("soak encrypted state leaked a synthetic credential or private payload")]
    PlaintextLeak,
    #[error("soak encrypted state could not be recovered after reopen")]
    Recovery,
    #[error("soak remote control state transition failed")]
    RemoteControl,
    #[error("soak checkpoint does not match its encrypted journal")]
    Checkpoint,
    #[error("soak report is already complete")]
    AlreadyComplete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoakReport {
    pub protocol: String,
    pub mode: String,
    pub status: String,
    pub started_at: String,
    pub expected_end_at: String,
    pub updated_at: String,
    pub duration_seconds: u64,
    pub interval_seconds: u64,
    pub cycles_completed: u64,
    pub events_appended: u64,
    pub journal_reopens: u64,
    pub duplicate_events_rejected: u64,
    pub sequence_gaps_rejected: u64,
    pub remote_controls_applied: u64,
    pub encrypted_recoveries: u64,
    pub plaintext_leak_scans: u64,
    pub last_event_sha256: String,
}

#[allow(clippy::too_many_lines)]
pub async fn run(
    state_directory: &Path,
    report_path: &Path,
    duration: Duration,
    interval: Duration,
) -> Result<SoakReport, SoakError> {
    if duration < interval
        || interval.is_zero()
        || duration.subsec_nanos() != 0
        || interval.subsec_nanos() != 0
        || !duration.as_secs().is_multiple_of(interval.as_secs())
    {
        return Err(SoakError::InvalidSchedule);
    }
    fs::create_dir_all(state_directory)?;
    if let Some(parent) = report_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let duration_seconds = duration.as_secs();
    let interval_seconds = interval.as_secs();
    let journal_path = state_directory.join("journal.db");
    let identity = DeviceIdentity::from_seed(&DEVICE_SEED);
    let arena_id = Uuid::from_u128(1);
    let run_id = Uuid::from_u128(2);
    let (mut report, started_at, expected_end_at) = if report_path.exists() {
        resume_report(
            report_path,
            &journal_path,
            run_id,
            duration_seconds,
            interval_seconds,
        )?
    } else {
        new_report(report_path, duration_seconds, interval_seconds)?
    };
    let expected_cycles = duration_seconds / interval_seconds;

    while report.cycles_completed < expected_cycles {
        let cycle_offset = interval_seconds
            .checked_mul(report.cycles_completed)
            .ok_or(SoakError::InvalidSchedule)?;
        let target = started_at
            + time::Duration::seconds(
                i64::try_from(cycle_offset).map_err(|_| SoakError::InvalidSchedule)?,
            );
        sleep_until(target).await;
        let sequence = report
            .events_appended
            .checked_add(1)
            .ok_or(SoakError::Recovery)?;
        let cycle_id = Uuid::new_v4();
        let event = RunEventEnvelopeV1::sign(
            identity.signing_key(),
            arena_id,
            run_id,
            Some(cycle_id),
            sequence,
            report.last_event_sha256.clone(),
            "portfolio_snapshot".into(),
            OffsetDateTime::now_utc(),
            json!({
                "cycle": report.cycles_completed + 1,
                "equity_micro_usdc": 1_000_000,
            }),
        )?;
        {
            let mut journal = EncryptedJournal::open(&journal_path, JOURNAL_KEY)?;
            report.journal_reopens = report.journal_reopens.saturating_add(1);
            journal.append(&event, &json!({"raw_prompt": SYNTHETIC_PRIVATE_SENTINEL}))?;
            if journal.private_payload(&event.event_sha256)?
                != Some(json!({"raw_prompt": SYNTHETIC_PRIVATE_SENTINEL}))
            {
                return Err(SoakError::Recovery);
            }
            report.encrypted_recoveries = report.encrypted_recoveries.saturating_add(1);
            journal.put_secret("device-refresh-token", SYNTHETIC_TOKEN_SENTINEL.as_bytes())?;
            let recovered = journal
                .secret("device-refresh-token")?
                .ok_or(SoakError::Recovery)?;
            if recovered.as_slice() != SYNTHETIC_TOKEN_SENTINEL.as_bytes() {
                return Err(SoakError::Recovery);
            }
            report.encrypted_recoveries = report.encrypted_recoveries.saturating_add(1);

            if journal
                .append(&event, &json!({"raw_prompt": SYNTHETIC_PRIVATE_SENTINEL}))
                .is_ok()
            {
                return Err(SoakError::FaultAccepted);
            }
            report.duplicate_events_rejected = report.duplicate_events_rejected.saturating_add(1);
            let gap = RunEventEnvelopeV1::sign(
                identity.signing_key(),
                arena_id,
                run_id,
                Some(cycle_id),
                sequence.saturating_add(2),
                event.event_sha256.clone(),
                "portfolio_snapshot".into(),
                OffsetDateTime::now_utc(),
                json!({"equity_micro_usdc": 1_000_000}),
            )?;
            if journal.append(&gap, &json!({})).is_ok() {
                return Err(SoakError::FaultAccepted);
            }
            report.sequence_gaps_rejected = report.sequence_gaps_rejected.saturating_add(1);
        }

        exercise_remote_controls()?;
        report.remote_controls_applied = report.remote_controls_applied.saturating_add(3);
        scan_for_plaintext(state_directory)?;
        report.plaintext_leak_scans = report.plaintext_leak_scans.saturating_add(1);
        report.cycles_completed = report.cycles_completed.saturating_add(1);
        report.events_appended = sequence;
        report.last_event_sha256 = event.event_sha256;
        report.updated_at = format_time(OffsetDateTime::now_utc());
        write_report(report_path, &report)?;
    }
    sleep_until(expected_end_at).await;
    report.status = "complete".into();
    report.updated_at = format_time(OffsetDateTime::now_utc());
    write_report(report_path, &report)?;
    Ok(report)
}

fn new_report(
    report_path: &Path,
    duration_seconds: u64,
    interval_seconds: u64,
) -> Result<(SoakReport, OffsetDateTime, OffsetDateTime), SoakError> {
    let started_at = OffsetDateTime::now_utc();
    let expected_end_at = started_at
        + time::Duration::seconds(
            i64::try_from(duration_seconds).map_err(|_| SoakError::InvalidSchedule)?,
        );
    let report = SoakReport {
        protocol: HARNESS_PROTOCOL_V1.into(),
        mode: "local_headless_component_soak".into(),
        status: "running".into(),
        started_at: format_time(started_at),
        expected_end_at: format_time(expected_end_at),
        updated_at: format_time(started_at),
        duration_seconds,
        interval_seconds,
        cycles_completed: 0,
        events_appended: 0,
        journal_reopens: 0,
        duplicate_events_rejected: 0,
        sequence_gaps_rejected: 0,
        remote_controls_applied: 0,
        encrypted_recoveries: 0,
        plaintext_leak_scans: 0,
        last_event_sha256: "0".repeat(64),
    };
    write_report(report_path, &report)?;
    Ok((report, started_at, expected_end_at))
}

fn resume_report(
    report_path: &Path,
    journal_path: &Path,
    run_id: Uuid,
    duration_seconds: u64,
    interval_seconds: u64,
) -> Result<(SoakReport, OffsetDateTime, OffsetDateTime), SoakError> {
    let report: SoakReport = serde_json::from_slice(&fs::read(report_path)?)?;
    if report.status == "complete" {
        return Err(SoakError::AlreadyComplete);
    }
    let balanced = report.protocol == HARNESS_PROTOCOL_V1
        && report.mode == "local_headless_component_soak"
        && report.status == "running"
        && report.duration_seconds == duration_seconds
        && report.interval_seconds == interval_seconds
        && report.events_appended == report.cycles_completed
        && report.journal_reopens == report.cycles_completed
        && report.duplicate_events_rejected == report.cycles_completed
        && report.sequence_gaps_rejected == report.cycles_completed
        && report.remote_controls_applied == report.cycles_completed.saturating_mul(3)
        && report.encrypted_recoveries == report.cycles_completed.saturating_mul(2)
        && report.plaintext_leak_scans == report.cycles_completed;
    if !balanced {
        return Err(SoakError::Checkpoint);
    }
    let started_at = parse_time(&report.started_at)?;
    let expected_end_at = parse_time(&report.expected_end_at)?;
    let calculated_end = started_at
        + time::Duration::seconds(
            i64::try_from(duration_seconds).map_err(|_| SoakError::InvalidSchedule)?,
        );
    if expected_end_at != calculated_end {
        return Err(SoakError::Checkpoint);
    }
    let journal = EncryptedJournal::open(journal_path, JOURNAL_KEY)?;
    let expected_state = if report.events_appended == 0 {
        None
    } else {
        Some((report.events_appended, report.last_event_sha256.clone()))
    };
    if journal.latest_event_state(run_id)? != expected_state {
        return Err(SoakError::Checkpoint);
    }
    Ok((report, started_at, expected_end_at))
}

async fn sleep_until(target: OffsetDateTime) {
    let remaining = target - OffsetDateTime::now_utc();
    if remaining.is_positive()
        && let Ok(duration) = Duration::try_from(remaining)
    {
        tokio::time::sleep(duration).await;
    }
}

fn exercise_remote_controls() -> Result<(), SoakError> {
    let gate = ExecutionGate::new();
    if !gate.apply(RemoteAction::Resume) || gate.label() != "running" {
        return Err(SoakError::RemoteControl);
    }
    if !gate.apply(RemoteAction::Pause) || gate.label() != "paused" {
        return Err(SoakError::RemoteControl);
    }
    if !gate.apply(RemoteAction::Stop) || gate.label() != "stopped" {
        return Err(SoakError::RemoteControl);
    }
    Ok(())
}

fn scan_for_plaintext(state_directory: &Path) -> Result<(), SoakError> {
    for entry in fs::read_dir(state_directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let raw = fs::read(entry.path())?;
        if contains(&raw, SYNTHETIC_PRIVATE_SENTINEL.as_bytes())
            || contains(&raw, SYNTHETIC_TOKEN_SENTINEL.as_bytes())
        {
            return Err(SoakError::PlaintextLeak);
        }
    }
    Ok(())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|candidate| candidate == needle)
}

fn write_report(path: &Path, report: &SoakReport) -> Result<(), SoakError> {
    let temporary = temporary_report_path(path);
    fs::write(&temporary, serde_json::to_vec_pretty(report)?)?;
    #[cfg(target_os = "windows")]
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}

fn temporary_report_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".tmp");
    PathBuf::from(value)
}

fn format_time(value: OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "invalid-time".into())
}

fn parse_time(value: &str) -> Result<OffsetDateTime, SoakError> {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map_err(|_| SoakError::Checkpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn short_soak_resumes_after_restart() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let report_path = directory.path().join("report.json");
        let state_path = directory.path().join("state");
        let task_state = state_path.clone();
        let task_report = report_path.clone();
        let first = tokio::spawn(async move {
            run(
                &task_state,
                &task_report,
                Duration::from_secs(2),
                Duration::from_secs(1),
            )
            .await
        });
        for _ in 0..100 {
            if let Ok(raw) = fs::read(&report_path)
                && let Ok(report) = serde_json::from_slice::<SoakReport>(&raw)
                && report.cycles_completed == 1
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let before_restart: SoakReport = serde_json::from_slice(&fs::read(&report_path)?)?;
        assert_eq!(before_restart.cycles_completed, 1);
        first.abort();
        let _ = first.await;
        let report = run(
            &state_path,
            &report_path,
            Duration::from_secs(2),
            Duration::from_secs(1),
        )
        .await?;
        assert_eq!(report.status, "complete");
        assert_eq!(report.started_at, before_restart.started_at);
        assert_eq!(report.cycles_completed, 2);
        assert_eq!(report.events_appended, report.cycles_completed);
        assert_eq!(report.duplicate_events_rejected, report.cycles_completed);
        assert_eq!(report.sequence_gaps_rejected, report.cycles_completed);
        assert_eq!(report.remote_controls_applied, report.cycles_completed * 3);
        assert_eq!(report.plaintext_leak_scans, report.cycles_completed);
        assert!(matches!(
            run(
                &state_path,
                &report_path,
                Duration::from_secs(2),
                Duration::from_secs(1),
            )
            .await,
            Err(SoakError::AlreadyComplete)
        ));
        Ok(())
    }
}
