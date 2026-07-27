use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Parser, Subcommand};
use crow_agent_core::{
    BacktestEngine, DeviceAuthorizationClient, DeviceAuthorizationError, DeviceEncryptionKey,
    EncryptedJournal, ScheduledProposal, read_verified_dataset,
};
use crow_agent_protocol::{
    DatasetManifestV1, DeviceIdentity, HARNESS_PROTOCOL_V1, RemoteAction, RemoteCommandV1,
    SignedArenaManifestV1, canonical_json, sha256,
};
use futures_util::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};
use thiserror::Error;
use time::OffsetDateTime;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{
        Message,
        client::IntoClientRequest as _,
        http::{HeaderValue, header::SEC_WEBSOCKET_PROTOCOL},
    },
};
use tracing::{info, warn};
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

mod soak;

const REFRESH_TOKEN_SECRET: &str = "device-refresh-token";
const DEVICE_ID_SECRET: &str = "device-id";
const EXECUTION_RUNNING: u8 = 0;
const EXECUTION_PAUSED: u8 = 1;
const EXECUTION_STOPPED: u8 = 2;

#[derive(Debug, Parser)]
#[command(name = "crow-agentd", version, about = "User-hosted Crow arena agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate a signed arena manifest before provisioning it to a run.
    ValidateManifest { path: PathBuf },
    /// Replay deterministic fixed-point proposals against a signed historical dataset.
    Backtest {
        arena_manifest: PathBuf,
        dataset_manifest: PathBuf,
        dataset_directory: PathBuf,
        proposals: PathBuf,
        starting_cash_micro_usdc: i64,
    },
    /// Print only the public device identity derived from systemd credentials.
    DevicePublic,
    /// Authorize this headless host through a short-lived Crow browser code.
    Authorize {
        device_label: String,
        #[arg(long, default_value = "https://api.crowcompute.ai")]
        api_origin: String,
        #[arg(long, default_value = "/var/lib/crow-agent")]
        state_directory: PathBuf,
    },
    /// Run the checkpointed encrypted-journal and fail-closed component soak.
    Soak {
        #[arg(long, default_value = "/var/lib/crow-agent/soak")]
        state_directory: PathBuf,
        #[arg(long, default_value = "/var/lib/crow-agent/soak-report.json")]
        report: PathBuf,
        #[arg(long, default_value_t = 7_200)]
        duration_seconds: u64,
        #[arg(long, default_value_t = 900)]
        interval_seconds: u64,
    },
    /// Maintain the outbound Crow control connection.
    Run { config: PathBuf },
}

#[derive(Debug, Deserialize)]
struct DaemonConfig {
    device_id: Uuid,
    relay_url: String,
    #[serde(default = "production_api_origin")]
    api_origin: String,
    #[serde(default = "default_state_directory")]
    state_directory: PathBuf,
}

fn production_api_origin() -> String {
    "https://api.crowcompute.ai".into()
}

fn default_state_directory() -> PathBuf {
    PathBuf::from("/var/lib/crow-agent")
}

#[derive(Debug, Deserialize)]
struct WireEnvelope {
    protocol: String,
    #[serde(rename = "type")]
    kind: String,
    id: String,
    payload: Value,
}

#[derive(Debug, Serialize)]
struct OutboundEnvelope<'a> {
    protocol: &'static str,
    #[serde(rename = "type")]
    kind: &'a str,
    id: String,
    #[serde(with = "time::serde::rfc3339")]
    timestamp: OffsetDateTime,
    payload: Value,
}

#[derive(Debug, Deserialize)]
struct RemoteCommandDelivery {
    command: RemoteCommandV1,
    controller_public_key: String,
}

#[derive(Debug, Clone)]
struct ExecutionGate {
    state: Arc<AtomicU8>,
}

impl ExecutionGate {
    fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(EXECUTION_PAUSED)),
        }
    }

    fn apply(&self, action: RemoteAction) -> bool {
        match action {
            RemoteAction::Pause => {
                self.state.store(EXECUTION_PAUSED, Ordering::SeqCst);
                true
            }
            RemoteAction::Resume => self
                .state
                .compare_exchange(
                    EXECUTION_PAUSED,
                    EXECUTION_RUNNING,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok(),
            RemoteAction::Stop => {
                self.state.store(EXECUTION_STOPPED, Ordering::SeqCst);
                true
            }
        }
    }

    fn label(&self) -> &'static str {
        match self.state.load(Ordering::SeqCst) {
            EXECUTION_RUNNING => "running",
            EXECUTION_STOPPED => "stopped",
            _ => "paused",
        }
    }
}

#[derive(Debug, Error)]
enum DaemonError {
    #[error("configuration or credential file is unavailable")]
    Io(#[from] std::io::Error),
    #[error("configuration is invalid")]
    Json(#[from] serde_json::Error),
    #[error("device signing credential must be exactly 32 raw bytes")]
    DeviceCredential,
    #[error("device encryption credential must be exactly 32 raw bytes")]
    EncryptionCredential,
    #[error("journal credential must be exactly 32 raw bytes")]
    JournalCredential,
    #[error("device token credential is invalid")]
    DeviceTokenCredential,
    #[error("CREDENTIALS_DIRECTORY is required")]
    CredentialDirectory,
    #[error("relay URL must use wss")]
    RelayUrl,
    #[error("relay connection failed")]
    Relay(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("relay closed before authentication")]
    RelayClosed,
    #[error("relay challenge is invalid")]
    Challenge,
    #[error("arena manifest is invalid")]
    Manifest,
    #[error("signed dataset is invalid")]
    Dataset(#[from] crow_agent_core::DatasetError),
    #[error("backtest replay failed")]
    Backtest(#[from] crow_agent_core::BacktestError),
    #[error("encrypted journal failed")]
    Journal(#[from] crow_agent_core::JournalError),
    #[error("device token rotation failed")]
    DeviceAuthorization(#[from] crow_agent_core::DeviceAuthorizationError),
    #[error("device authorization was not completed before expiry")]
    AuthorizationExpired,
    #[error("remote command is invalid")]
    RemoteCommand,
    #[error("headless component soak failed")]
    Soak(#[from] soak::SoakError),
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<(), DaemonError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "crow_agentd=info".into()),
        )
        .json()
        .init();
    match Cli::parse().command {
        Command::ValidateManifest { path } => {
            let signed = serde_json::from_slice::<SignedArenaManifestV1>(&fs::read(path)?)?;
            signed.verify().map_err(|_| DaemonError::Manifest)?;
            println!(
                "valid {} {} {}",
                signed.manifest.arena_id, signed.manifest.manifest_version, signed.manifest_sha256
            );
        }
        Command::Backtest {
            arena_manifest,
            dataset_manifest,
            dataset_directory,
            proposals,
            starting_cash_micro_usdc,
        } => {
            let signed =
                serde_json::from_slice::<SignedArenaManifestV1>(&fs::read(arena_manifest)?)?;
            signed.verify().map_err(|_| DaemonError::Manifest)?;
            let dataset =
                serde_json::from_slice::<DatasetManifestV1>(&fs::read(dataset_manifest)?)?;
            let candles = read_verified_dataset(&dataset_directory, &dataset)?;
            if signed.manifest.mode != crow_agent_protocol::ArenaMode::HistoricalBacktest
                || signed.manifest.dataset_sha256.as_deref()
                    != Some(dataset.package_sha256.as_str())
            {
                return Err(DaemonError::Manifest);
            }
            let scheduled =
                serde_json::from_slice::<Vec<ScheduledProposal>>(&fs::read(proposals)?)?;
            let result = BacktestEngine::new(
                signed.manifest.risk_rules,
                signed.manifest.execution.clone(),
            )
            .run_synchronized_proposals(
                &candles,
                &scheduled,
                starting_cash_micro_usdc,
            )?;
            let replay = json!({
                "protocol": HARNESS_PROTOCOL_V1,
                "arena_id": signed.manifest.arena_id,
                "arena_manifest_sha256": signed.manifest_sha256,
                "dataset_sha256": dataset.package_sha256,
                "result": result,
            });
            let replay_sha256 = hex::encode(sha256(
                &canonical_json(&replay).map_err(|_| DaemonError::Manifest)?,
            ));
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "replay": replay,
                    "replay_sha256": replay_sha256,
                }))?
            );
        }
        Command::DevicePublic => {
            let identity = load_identity()?;
            println!("{}", identity.public_key());
        }
        Command::Authorize {
            device_label,
            api_origin,
            state_directory,
        } => {
            authorize_device(&device_label, &api_origin, &state_directory).await?;
        }
        Command::Soak {
            state_directory,
            report,
            duration_seconds,
            interval_seconds,
        } => {
            let result = soak::run(
                &state_directory,
                &report,
                Duration::from_secs(duration_seconds),
                Duration::from_secs(interval_seconds),
            )
            .await?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Command::Run { config } => {
            let config = serde_json::from_slice::<DaemonConfig>(&fs::read(config)?)?;
            let identity = load_identity()?;
            fs::create_dir_all(&config.state_directory)?;
            let journal_key = load_credential_32("journal-key", DaemonError::JournalCredential)?;
            let journal =
                EncryptedJournal::open(&config.state_directory.join("journal.db"), journal_key)?;
            let refresh_token = load_refresh_token(&journal)?;
            run_relay(&config, &identity, &journal, refresh_token).await?;
        }
    }
    Ok(())
}

async fn authorize_device(
    device_label: &str,
    api_origin: &str,
    state_directory: &Path,
) -> Result<(), DaemonError> {
    let identity = load_identity()?;
    let encryption_secret = load_credential_32(
        "device-encryption-secret",
        DaemonError::EncryptionCredential,
    )?;
    let encryption_key = DeviceEncryptionKey::from_secret(encryption_secret);
    let client = DeviceAuthorizationClient::new(api_origin)?;
    fs::create_dir_all(state_directory)?;
    let journal_key = load_credential_32("journal-key", DaemonError::JournalCredential)?;
    let journal = EncryptedJournal::open(&state_directory.join("journal.db"), journal_key)?;
    let session = client
        .start(
            device_label,
            "linux",
            &identity,
            &URL_SAFE_NO_PAD.encode(encryption_key.public_key()),
        )
        .await?;
    println!(
        "Open {} and enter {}",
        session.verification_uri, session.user_code
    );
    loop {
        if OffsetDateTime::now_utc() >= session.expires_at {
            return Err(DaemonError::AuthorizationExpired);
        }
        tokio::time::sleep(session.interval).await;
        match client.exchange(&session, &identity).await {
            Ok(tokens) => {
                journal.put_secret(REFRESH_TOKEN_SECRET, tokens.refresh_token.as_bytes())?;
                journal.put_secret(DEVICE_ID_SECRET, tokens.device_id.as_bytes())?;
                println!("Authorized device {}", tokens.device_id);
                return Ok(());
            }
            Err(DeviceAuthorizationError::Pending | DeviceAuthorizationError::Request(_)) => {}
            Err(DeviceAuthorizationError::Expired) => {
                return Err(DaemonError::AuthorizationExpired);
            }
            Err(error) => return Err(DaemonError::DeviceAuthorization(error)),
        }
    }
}

fn load_identity() -> Result<DeviceIdentity, DaemonError> {
    let seed = load_credential_32("device-signing-seed", DaemonError::DeviceCredential)?;
    Ok(DeviceIdentity::from_seed(&seed))
}

fn load_credential_32(name: &str, invalid: DaemonError) -> Result<[u8; 32], DaemonError> {
    let directory = credential_directory()?;
    let seed = Zeroizing::new(fs::read(directory.join(name))?);
    let seed: [u8; 32] = seed.as_slice().try_into().map_err(|_| invalid)?;
    Ok(seed)
}

fn credential_directory() -> Result<PathBuf, DaemonError> {
    std::env::var_os("CREDENTIALS_DIRECTORY")
        .map(PathBuf::from)
        .ok_or(DaemonError::CredentialDirectory)
}

fn load_refresh_token(journal: &EncryptedJournal) -> Result<Zeroizing<String>, DaemonError> {
    if let Some(stored) = journal.secret(REFRESH_TOKEN_SECRET)? {
        let token =
            String::from_utf8(stored.to_vec()).map_err(|_| DaemonError::DeviceTokenCredential)?;
        if token.starts_with("crow_device_refresh_") {
            return Ok(Zeroizing::new(token));
        }
    }
    let raw = Zeroizing::new(fs::read(credential_directory()?.join("device-token"))?);
    let token = String::from_utf8(raw.to_vec())
        .map_err(|_| DaemonError::DeviceTokenCredential)?
        .trim()
        .to_owned();
    if !token.starts_with("crow_device_refresh_") {
        return Err(DaemonError::DeviceTokenCredential);
    }
    Ok(Zeroizing::new(token))
}

async fn run_relay(
    config: &DaemonConfig,
    identity: &DeviceIdentity,
    journal: &EncryptedJournal,
    mut refresh_token: Zeroizing<String>,
) -> Result<(), DaemonError> {
    let url = Url::parse(&config.relay_url).map_err(|_| DaemonError::RelayUrl)?;
    if url.scheme() != "wss" {
        return Err(DaemonError::RelayUrl);
    }
    let authorization = DeviceAuthorizationClient::new(&config.api_origin)?;
    let execution_gate = ExecutionGate::new();
    let mut backoff = Duration::from_secs(1);
    loop {
        let tokens = match authorization.rotate(&refresh_token, identity).await {
            Ok(tokens) => tokens,
            Err(error) => {
                warn!(error = %error, "device token rotation failed; no new decisions are permitted");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_mins(1));
                continue;
            }
        };
        journal.put_secret(REFRESH_TOKEN_SECRET, tokens.refresh_token.as_bytes())?;
        refresh_token = Zeroizing::new(tokens.refresh_token.to_string());
        match relay_session(
            config,
            identity,
            journal,
            &tokens.access_token,
            tokens.access_expires_at,
            &execution_gate,
        )
        .await
        {
            Ok(()) => {
                backoff = Duration::from_secs(1);
            }
            Err(error) => {
                warn!(error = %error, "relay session ended; no new decisions are permitted");
                execution_gate
                    .state
                    .store(EXECUTION_PAUSED, Ordering::SeqCst);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_mins(1));
            }
        }
    }
}

async fn relay_session(
    config: &DaemonConfig,
    identity: &DeviceIdentity,
    journal: &EncryptedJournal,
    access_token: &Zeroizing<String>,
    access_expires_at: OffsetDateTime,
    execution_gate: &ExecutionGate,
) -> Result<(), DaemonError> {
    let mut connection = connect_authenticated(config, identity, access_token).await?;
    info!(device_id = %config.device_id, "outbound relay authentication sent");
    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
    let rotate_after = (access_expires_at - OffsetDateTime::now_utc() - time::Duration::minutes(1))
        .try_into()
        .unwrap_or(Duration::ZERO);
    let rotation = tokio::time::sleep(rotate_after);
    tokio::pin!(rotation);
    loop {
        tokio::select! {
            () = &mut rotation => {
                info!("rotating device token between control messages");
                return Ok(());
            }
            _ = heartbeat.tick() => {
                let envelope = OutboundEnvelope {
                    protocol: HARNESS_PROTOCOL_V1,
                    kind: "heartbeat",
                    id: Uuid::new_v4().to_string(),
                    timestamp: OffsetDateTime::now_utc(),
                    payload: json!({
                        "device_id": config.device_id,
                        "active_run": null,
                        "execution_state": execution_gate.label()
                    }),
                };
                connection.send(Message::Text(serde_json::to_string(&envelope)?.into())).await?;
            }
            message = connection.next() => {
                match message {
                    Some(Ok(Message::Text(raw))) => {
                        let envelope = serde_json::from_str::<WireEnvelope>(&raw)?;
                        if envelope.protocol == HARNESS_PROTOCOL_V1 && envelope.kind == "shutdown" {
                            info!("relay requested shutdown");
                            return Ok(());
                        }
                        if let Some(acknowledgement) = apply_relay_message(
                            envelope,
                            config.device_id,
                            execution_gate,
                            journal,
                        )? {
                            connection.send(Message::Text(serde_json::to_string(&acknowledgement)?.into())).await?;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => return Err(DaemonError::RelayClosed),
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(DaemonError::Relay(error)),
                }
            }
        }
    }
}

async fn connect_authenticated(
    config: &DaemonConfig,
    identity: &DeviceIdentity,
    access_token: &Zeroizing<String>,
) -> Result<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, DaemonError> {
    let mut request = config.relay_url.as_str().into_client_request()?;
    request.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static(HARNESS_PROTOCOL_V1),
    );
    let (mut connection, response) = connect_async(request).await?;
    if response
        .headers()
        .get(SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        != Some(HARNESS_PROTOCOL_V1)
    {
        return Err(DaemonError::Challenge);
    }
    let challenge_message = connection.next().await.ok_or(DaemonError::RelayClosed)??;
    let challenge = decode_envelope(challenge_message)?;
    if challenge.protocol != HARNESS_PROTOCOL_V1 || challenge.kind != "challenge" {
        return Err(DaemonError::Challenge);
    }
    let nonce = challenge
        .payload
        .get("nonce")
        .and_then(Value::as_str)
        .ok_or(DaemonError::Challenge)?;
    let auth = OutboundEnvelope {
        protocol: HARNESS_PROTOCOL_V1,
        kind: "auth",
        id: challenge.id,
        timestamp: OffsetDateTime::now_utc(),
        payload: json!({
            "device_id": config.device_id,
            "device_public_key": identity.public_key(),
            "access_token": access_token.as_str(),
            "signature": identity.sign_bytes(nonce.as_bytes())
        }),
    };
    connection
        .send(Message::Text(serde_json::to_string(&auth)?.into()))
        .await?;
    Ok(connection)
}

fn apply_relay_message(
    envelope: WireEnvelope,
    device_id: Uuid,
    execution_gate: &ExecutionGate,
    journal: &EncryptedJournal,
) -> Result<Option<OutboundEnvelope<'static>>, DaemonError> {
    if envelope.protocol != HARNESS_PROTOCOL_V1 {
        return Err(DaemonError::Challenge);
    }
    match envelope.kind.as_str() {
        "auth.ok" => Ok(None),
        "remote_command" => {
            let delivery = serde_json::from_value::<RemoteCommandDelivery>(envelope.payload)?;
            let command = delivery.command;
            let nonce_key = format!("remote-controller-nonce-{}", command.controller_device_id);
            let previous_nonce = journal
                .secret(&nonce_key)?
                .and_then(|value| value.as_slice().try_into().ok().map(u64::from_be_bytes));
            if command.command_id.to_string() != envelope.id
                || command.target_device_id != device_id
                || command.relay_receipt.is_none()
                || command
                    .verify(&delivery.controller_public_key, OffsetDateTime::now_utc())
                    .is_err()
                || previous_nonce.is_some_and(|nonce| command.nonce <= nonce)
            {
                return Err(DaemonError::RemoteCommand);
            }
            journal.put_secret(&nonce_key, &command.nonce.to_be_bytes())?;
            if !execution_gate.apply(command.action) {
                return Err(DaemonError::RemoteCommand);
            }
            info!(command_id = %command.command_id, state = execution_gate.label(), "remote command applied");
            Ok(Some(OutboundEnvelope {
                protocol: HARNESS_PROTOCOL_V1,
                kind: "remote_command.ack",
                id: command.command_id.to_string(),
                timestamp: OffsetDateTime::now_utc(),
                payload: json!({"execution_state": execution_gate.label()}),
            }))
        }
        _ => Err(DaemonError::Challenge),
    }
}

fn decode_envelope(message: Message) -> Result<WireEnvelope, DaemonError> {
    match message {
        Message::Text(raw) => serde_json::from_str(&raw).map_err(DaemonError::from),
        _ => Err(DaemonError::Challenge),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn remote_command_nonce_survives_daemon_restart() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let journal_path = directory.path().join("journal.db");
        let target_device_id = Uuid::new_v4();
        let controller_device_id = Uuid::new_v4();
        let controller = DeviceIdentity::generate();
        let now = OffsetDateTime::now_utc();
        let mut command = RemoteCommandV1::sign(
            controller.signing_key(),
            target_device_id,
            Uuid::new_v4(),
            RemoteAction::Resume,
            1,
            now - time::Duration::seconds(1),
            now + time::Duration::seconds(10),
            controller_device_id,
        )?;
        command.relay_receipt = Some("gateway-signed-receipt".into());
        let envelope = || WireEnvelope {
            protocol: HARNESS_PROTOCOL_V1.into(),
            kind: "remote_command".into(),
            id: command.command_id.to_string(),
            payload: json!({
                "command": command,
                "controller_public_key": controller.public_key(),
            }),
        };
        {
            let journal = EncryptedJournal::open(&journal_path, [3_u8; 32])?;
            let gate = ExecutionGate::new();
            let acknowledgement =
                apply_relay_message(envelope(), target_device_id, &gate, &journal)?;
            assert!(acknowledgement.is_some());
            assert_eq!(gate.label(), "running");
        }
        let restarted_journal = EncryptedJournal::open(&journal_path, [3_u8; 32])?;
        let restarted_gate = ExecutionGate::new();
        assert!(
            apply_relay_message(
                envelope(),
                target_device_id,
                &restarted_gate,
                &restarted_journal
            )
            .is_err()
        );
        assert_eq!(restarted_gate.label(), "paused");
        Ok(())
    }
}
