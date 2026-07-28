use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Parser, Subcommand};
use crow_agent_core::{
    BacktestEngine, CompanionActionV1, CompanionIpcError, CompanionRequestV1, CompanionResponseV1,
    DeviceAuthorizationClient, DeviceAuthorizationError, DeviceEncryptionKey, EncryptedJournal,
    MAX_COMPANION_MESSAGE_BYTES, ScheduledProposal, TlsProviderError, install_tls_crypto_provider,
    read_verified_dataset,
};
use crow_agent_protocol::{
    DatasetManifestV1, DeviceIdentity, HARNESS_PROTOCOL_V1, RemoteAction, RemoteCommandV1,
    SignedArenaManifestV1, canonical_json, sha256,
};
use futures_util::{SinkExt as _, StreamExt as _};
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, ListenerOptions, Name,
    tokio::{Stream as LocalSocketStream, prelude::*},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
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

mod live_run;
mod soak;

const REFRESH_TOKEN_SECRET: &str = "device-refresh-token";
const DEVICE_ID_SECRET: &str = "device-id";
const EXECUTION_RUNNING: u8 = 0;
const EXECUTION_PAUSED: u8 = 1;
const EXECUTION_STOPPED: u8 = 2;
const MAX_DESKTOP_REFRESH_TOKEN_BYTES: usize = 512;

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
        #[arg(long, default_value_t = 1_800)]
        duration_seconds: u64,
        #[arg(long, default_value_t = 900)]
        interval_seconds: u64,
    },
    /// Run as the desktop application's authenticated background companion.
    Companion {
        #[arg(long)]
        ipc_name: String,
    },
    /// Run a desktop-provisioned arena and authenticated local control socket.
    DesktopRun {
        config: PathBuf,
        #[arg(long)]
        ipc_name: String,
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
    #[serde(default)]
    live_arena: Option<live_run::LiveArenaConfig>,
}

struct DesktopCredentials {
    companion_secret: Zeroizing<[u8; 32]>,
    signing_seed: Zeroizing<[u8; 32]>,
    encryption_secret: Zeroizing<[u8; 32]>,
    journal_key: Zeroizing<[u8; 32]>,
    api_wallet_key: Zeroizing<[u8; 32]>,
    refresh_token: Zeroizing<String>,
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
pub(crate) struct ExecutionGate {
    state: Arc<AtomicU8>,
}

impl ExecutionGate {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(EXECUTION_PAUSED)),
        }
    }

    pub(crate) fn apply(&self, action: RemoteAction) -> bool {
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

    pub(crate) fn label(&self) -> &'static str {
        match self.state.load(Ordering::SeqCst) {
            EXECUTION_RUNNING => "running",
            EXECUTION_STOPPED => "stopped",
            _ => "paused",
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.state.load(Ordering::SeqCst) == EXECUTION_RUNNING
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
    #[error("Hyperliquid API wallet credential must be exactly 32 raw bytes")]
    ApiWalletCredential,
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
    #[error("desktop companion authentication failed")]
    Companion(#[from] CompanionIpcError),
    #[error("headless component soak failed")]
    Soak(#[from] soak::SoakError),
    #[error("live arena runtime failed closed")]
    LiveRun(#[from] live_run::LiveRunError),
    #[error("TLS provider initialization failed")]
    TlsProvider(#[from] TlsProviderError),
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<(), DaemonError> {
    install_tls_crypto_provider()?;
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
        Command::Companion { ipc_name } => {
            run_companion(&ipc_name).await?;
        }
        Command::DesktopRun { config, ipc_name } => {
            let config = serde_json::from_slice::<DaemonConfig>(&fs::read(config)?)?;
            let prepared_live = config
                .live_arena
                .clone()
                .map(live_run::prepare)
                .transpose()?
                .ok_or(DaemonError::Manifest)?;
            let credentials = read_desktop_credentials()?;
            let identity = DeviceIdentity::from_seed(&credentials.signing_seed);
            let device_encryption_key =
                DeviceEncryptionKey::from_secret(*credentials.encryption_secret);
            fs::create_dir_all(&config.state_directory)?;
            let journal = EncryptedJournal::open(
                &config.state_directory.join("journal.db"),
                *credentials.journal_key,
            )?;
            let execution_gate = ExecutionGate::new();
            let active_run = Arc::new(Mutex::new(None));
            tokio::select! {
                result = run_companion_listener(
                    &ipc_name,
                    &credentials.companion_secret,
                    &execution_gate,
                    &active_run,
                ) => result?,
                result = run_relay(
                    &config,
                    &identity,
                    &journal,
                    *credentials.journal_key,
                    Zeroizing::new(credentials.refresh_token.to_string()),
                    &device_encryption_key,
                    Some(&prepared_live),
                    Some(&credentials.api_wallet_key),
                    &execution_gate,
                    &active_run,
                ) => result?,
            }
        }
        Command::Run { config } => {
            let config = serde_json::from_slice::<DaemonConfig>(&fs::read(config)?)?;
            let prepared_live = config
                .live_arena
                .clone()
                .map(live_run::prepare)
                .transpose()?;
            let identity = load_identity()?;
            fs::create_dir_all(&config.state_directory)?;
            let journal_key = load_credential_32("journal-key", DaemonError::JournalCredential)?;
            let device_encryption_key = DeviceEncryptionKey::from_secret(load_credential_32(
                "device-encryption-secret",
                DaemonError::EncryptionCredential,
            )?);
            let journal =
                EncryptedJournal::open(&config.state_directory.join("journal.db"), journal_key)?;
            let refresh_token = load_refresh_token(&journal)?;
            let api_wallet_key = if prepared_live.is_some() {
                Some(Zeroizing::new(load_credential_32(
                    "hyperliquid-api-wallet-key",
                    DaemonError::ApiWalletCredential,
                )?))
            } else {
                None
            };
            let execution_gate = ExecutionGate::new();
            let active_run = Arc::new(Mutex::new(None));
            Box::pin(run_relay(
                &config,
                &identity,
                &journal,
                journal_key,
                refresh_token,
                &device_encryption_key,
                prepared_live.as_ref(),
                api_wallet_key.as_ref(),
                &execution_gate,
                &active_run,
            ))
            .await?;
        }
    }
    Ok(())
}

fn local_socket_name(label: &str) -> Result<Name<'static>, std::io::Error> {
    if GenericNamespaced::is_supported() {
        label
            .to_ns_name::<GenericNamespaced>()
            .map(Name::into_owned)
    } else {
        std::env::temp_dir()
            .join(format!("{label}.sock"))
            .to_fs_name::<GenericFilePath>()
            .map(Name::into_owned)
    }
}

async fn run_companion(ipc_name: &str) -> Result<(), DaemonError> {
    let mut secret = Zeroizing::new([0_u8; 32]);
    std::io::stdin().lock().read_exact(secret.as_mut())?;
    let execution_gate = ExecutionGate::new();
    let active_run = Arc::new(Mutex::new(None));
    run_companion_listener(ipc_name, &secret, &execution_gate, &active_run).await
}

async fn run_companion_listener(
    ipc_name: &str,
    secret: &[u8; 32],
    execution_gate: &ExecutionGate,
    active_run: &Arc<Mutex<Option<Uuid>>>,
) -> Result<(), DaemonError> {
    let name = local_socket_name(ipc_name)?;
    let listener = ListenerOptions::new()
        .name(name)
        .try_overwrite(true)
        .create_tokio()?;
    let mut highest_nonce = 0_u64;
    info!("desktop companion ready on authenticated local IPC");

    loop {
        let connection = listener.accept().await?;
        if let Err(error) = handle_companion_connection(
            connection,
            secret,
            &mut highest_nonce,
            execution_gate,
            active_run,
        )
        .await
        {
            warn!(error = %error, "rejected desktop companion request");
        }
    }
}

async fn handle_companion_connection(
    connection: LocalSocketStream,
    secret: &[u8; 32],
    highest_nonce: &mut u64,
    execution_gate: &ExecutionGate,
    active_run: &Arc<Mutex<Option<Uuid>>>,
) -> Result<(), DaemonError> {
    let reader = BufReader::new(&connection);
    let mut reader = reader.take((MAX_COMPANION_MESSAGE_BYTES + 1) as u64);
    let mut raw = String::new();
    let bytes_read = reader.read_line(&mut raw).await?;
    if bytes_read == 0 || bytes_read > MAX_COMPANION_MESSAGE_BYTES {
        return Err(DaemonError::RemoteCommand);
    }
    let request = serde_json::from_str::<CompanionRequestV1>(raw.trim_end())?;
    let response =
        apply_companion_request(&request, secret, highest_nonce, execution_gate, active_run)?;
    let mut encoded = serde_json::to_vec(&response)?;
    encoded.push(b'\n');
    let mut sender = &connection;
    sender.write_all(&encoded).await?;
    Ok(())
}

fn apply_companion_request(
    request: &CompanionRequestV1,
    secret: &[u8; 32],
    highest_nonce: &mut u64,
    execution_gate: &ExecutionGate,
    active_run: &Arc<Mutex<Option<Uuid>>>,
) -> Result<CompanionResponseV1, DaemonError> {
    request.verify(secret)?;
    if request.nonce <= *highest_nonce {
        return Err(DaemonError::RemoteCommand);
    }
    *highest_nonce = request.nonce;

    let accepted = match request.action {
        CompanionActionV1::Status => true,
        CompanionActionV1::Pause => execution_gate.apply(RemoteAction::Pause),
        CompanionActionV1::Resume => execution_gate.apply(RemoteAction::Resume),
        CompanionActionV1::Stop => execution_gate.apply(RemoteAction::Stop),
    };
    CompanionResponseV1::sign(
        secret,
        request.request_id,
        request.nonce,
        accepted,
        execution_gate.label(),
        *active_run.lock().map_err(|_| DaemonError::RemoteCommand)?,
        (!accepted).then(|| "invalid_transition".into()),
    )
    .map_err(DaemonError::from)
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

fn read_desktop_credentials() -> Result<DesktopCredentials, DaemonError> {
    let mut reader = std::io::stdin().lock();
    read_desktop_credentials_from(&mut reader)
}

fn read_desktop_credentials_from(
    reader: &mut impl Read,
) -> Result<DesktopCredentials, DaemonError> {
    let companion_secret = read_secret_32(reader)?;
    let signing_seed = read_secret_32(reader)?;
    let encryption_secret = read_secret_32(reader)?;
    let journal_key = read_secret_32(reader)?;
    let api_wallet_key = read_secret_32(reader)?;
    let mut length = [0_u8; 2];
    reader.read_exact(&mut length)?;
    let length = usize::from(u16::from_be_bytes(length));
    if length == 0 || length > MAX_DESKTOP_REFRESH_TOKEN_BYTES {
        return Err(DaemonError::DeviceTokenCredential);
    }
    let mut raw = Zeroizing::new(vec![0_u8; length]);
    reader.read_exact(&mut raw)?;
    let refresh_token = Zeroizing::new(
        String::from_utf8(raw.to_vec()).map_err(|_| DaemonError::DeviceTokenCredential)?,
    );
    if !refresh_token.starts_with("crow_device_refresh_") {
        return Err(DaemonError::DeviceTokenCredential);
    }
    Ok(DesktopCredentials {
        companion_secret,
        signing_seed,
        encryption_secret,
        journal_key,
        api_wallet_key,
        refresh_token,
    })
}

fn read_secret_32(reader: &mut impl Read) -> Result<Zeroizing<[u8; 32]>, DaemonError> {
    let mut value = Zeroizing::new([0_u8; 32]);
    reader.read_exact(value.as_mut())?;
    Ok(value)
}

#[allow(clippy::too_many_arguments)]
async fn run_relay(
    config: &DaemonConfig,
    identity: &DeviceIdentity,
    journal: &EncryptedJournal,
    journal_key: [u8; 32],
    mut refresh_token: Zeroizing<String>,
    device_encryption_key: &DeviceEncryptionKey,
    live_arena: Option<&live_run::PreparedLiveArena>,
    api_wallet_key: Option<&Zeroizing<[u8; 32]>>,
    execution_gate: &ExecutionGate,
    active_run: &Arc<Mutex<Option<Uuid>>>,
) -> Result<(), DaemonError> {
    let url = Url::parse(&config.relay_url).map_err(|_| DaemonError::RelayUrl)?;
    if url.scheme() != "wss" {
        return Err(DaemonError::RelayUrl);
    }
    let authorization = DeviceAuthorizationClient::new(&config.api_origin)?;
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
        let relay = relay_session(
            config,
            identity,
            journal,
            &tokens.access_token,
            tokens.access_expires_at,
            execution_gate,
            active_run,
        );
        let session_result = if let (Some(live_arena), Some(api_wallet_key)) =
            (live_arena, api_wallet_key)
            && execution_gate.label() != "stopped"
        {
            tokio::select! {
                result = relay => result,
                result = live_run::run_session(
                    live_arena,
                    &config.api_origin,
                    &config.state_directory,
                    journal_key,
                    api_wallet_key,
                    config.device_id,
                    device_encryption_key,
                    &tokens.access_token,
                    identity,
                    execution_gate,
                    active_run,
                ) => match result {
                    Ok(live_run::LiveSessionOutcome::Stopped) => Ok(()),
                    Err(error) => Err(DaemonError::LiveRun(error)),
                },
            }
        } else {
            relay.await
        };
        match session_result {
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
    active_run: &Arc<std::sync::Mutex<Option<Uuid>>>,
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
                if let Err(error) = connection.close(None).await {
                    warn!(error = %error, "relay close handshake failed during token rotation");
                }
                // Give the relay handler time to release distributed device
                // ownership before the next scoped token reconnects.
                tokio::time::sleep(Duration::from_millis(250)).await;
                return Ok(());
            }
            _ = heartbeat.tick() => {
                let active_run = *active_run
                    .lock()
                    .map_err(|_| DaemonError::RemoteCommand)?;
                let envelope = OutboundEnvelope {
                    protocol: HARNESS_PROTOCOL_V1,
                    kind: "heartbeat",
                    id: Uuid::new_v4().to_string(),
                    timestamp: OffsetDateTime::now_utc(),
                    payload: json!({
                        "device_id": config.device_id,
                        "active_run": active_run,
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
                        let command_run = *active_run
                            .lock()
                            .map_err(|_| DaemonError::RemoteCommand)?;
                        if let Some(acknowledgement) = apply_relay_message(
                            envelope,
                            config.device_id,
                            execution_gate,
                            journal,
                            command_run,
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
    active_run: Option<Uuid>,
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
                || active_run != Some(command.run_id)
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
            let acknowledgement = apply_relay_message(
                envelope(),
                target_device_id,
                &gate,
                &journal,
                Some(command.run_id),
            )?;
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
                &restarted_journal,
                Some(command.run_id),
            )
            .is_err()
        );
        assert_eq!(restarted_gate.label(), "paused");
        Ok(())
    }

    #[test]
    fn companion_ipc_authenticates_and_rejects_replay() -> Result<(), Box<dyn std::error::Error>> {
        let secret = [19_u8; 32];
        let gate = ExecutionGate::new();
        let run_id = Uuid::new_v4();
        let active_run = Arc::new(Mutex::new(Some(run_id)));
        let mut highest_nonce = 0;
        let request = CompanionRequestV1::sign(&secret, 1, CompanionActionV1::Resume)?;
        let response =
            apply_companion_request(&request, &secret, &mut highest_nonce, &gate, &active_run)?;
        response.verify(&secret, &request)?;
        assert!(response.accepted);
        assert_eq!(response.execution_state, "running");
        assert_eq!(response.active_run, Some(run_id));
        assert!(
            apply_companion_request(&request, &secret, &mut highest_nonce, &gate, &active_run,)
                .is_err()
        );

        let mut tampered = CompanionRequestV1::sign(&secret, 2, CompanionActionV1::Pause)?;
        tampered.action = CompanionActionV1::Stop;
        assert!(
            apply_companion_request(&tampered, &secret, &mut highest_nonce, &gate, &active_run,)
                .is_err()
        );
        assert_eq!(highest_nonce, 1);
        Ok(())
    }

    #[test]
    fn desktop_credential_frame_is_bounded_and_preserves_field_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let token = b"crow_device_refresh_runner";
        let mut frame = Vec::new();
        for value in 1_u8..=5 {
            frame.extend_from_slice(&[value; 32]);
        }
        frame.extend_from_slice(&u16::try_from(token.len())?.to_be_bytes());
        frame.extend_from_slice(token);
        let credentials = read_desktop_credentials_from(&mut std::io::Cursor::new(frame.clone()))?;
        assert_eq!(*credentials.companion_secret, [1_u8; 32]);
        assert_eq!(*credentials.signing_seed, [2_u8; 32]);
        assert_eq!(*credentials.encryption_secret, [3_u8; 32]);
        assert_eq!(*credentials.journal_key, [4_u8; 32]);
        assert_eq!(*credentials.api_wallet_key, [5_u8; 32]);
        assert_eq!(
            credentials.refresh_token.as_str(),
            "crow_device_refresh_runner"
        );

        frame.truncate(32 * 5);
        frame.extend_from_slice(&u16::MAX.to_be_bytes());
        assert!(read_desktop_credentials_from(&mut std::io::Cursor::new(frame)).is_err());
        Ok(())
    }
}
