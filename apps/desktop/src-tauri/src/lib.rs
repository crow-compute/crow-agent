use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crow_agent_core::{
    AgentVersionRecipient, CompanionActionV1, CompanionRequestV1, CompanionResponseV1,
    DeviceAuthorizationClient, DeviceAuthorizationError, DeviceAuthorizationSession,
    DeviceEncryptionKey, DeviceTokens, EncryptedJournal, MAX_COMPANION_MESSAGE_BYTES,
    REQUIRED_STRATEGY_TOOLS, StrategyBundleV1, decode_device_encryption_public_key,
    hyperliquid_api_wallet_address, open_agent_version, seal_agent_version,
};
use crow_agent_protocol::{
    AgentVersionEnvelopeV1, DeviceIdentity, HARNESS_PROTOCOL_V1, RemoteAction, RemoteCommandV1,
    SignedArenaManifestV1, sha256,
};
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, Name,
    tokio::{Stream as LocalSocketStream, prelude::*},
};
use keyring::{Entry, Error as KeyringError};
use rand_core::{OsRng, RngCore as _};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};
use tauri::{Manager as _, State};
use tauri_plugin_shell::{
    ShellExt as _,
    process::{CommandChild, CommandEvent},
};
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::Mutex as AsyncMutex;
use url::Url;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

const CREDENTIAL_SERVICE: &str = "ai.crowcompute.agent";
const CREDENTIAL_VAULT_ACCOUNT: &str = "desktop-credential-vault-v1";
const SIGNING_SEED_ACCOUNT: &str = "device-signing-seed";
const ENCRYPTION_SECRET_ACCOUNT: &str = "device-encryption-secret";
const ACCESS_TOKEN_ACCOUNT: &str = "device-access-token";
const ACCESS_EXPIRES_AT_ACCOUNT: &str = "device-access-expires-at";
const REFRESH_TOKEN_ACCOUNT: &str = "device-refresh-token";
const DEVICE_ID_ACCOUNT: &str = "device-id";
const CONTROLLER_NONCE_ACCOUNT: &str = "controller-nonce";
const COMPANION_SECRET_ACCOUNT: &str = "companion-ipc-secret";
const JOURNAL_KEY_ACCOUNT: &str = "journal-key";
const HYPERLIQUID_API_WALLET_ACCOUNT: &str = "hyperliquid-api-wallet-key";
const PRODUCTION_API_ORIGIN: &str = "https://api.crowcompute.ai";
const PRODUCTION_RELAY_URL: &str = "wss://api.crowcompute.ai/harness/v1/connect";
const HYPERLIQUID_TESTNET_API_URL: &str = "https://app.hyperliquid-testnet.xyz/API";
const COMPANION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const MAX_DESKTOP_REFRESH_TOKEN_BYTES: usize = 512;
const CREDENTIAL_VAULT_VERSION: u8 = 1;

#[derive(Default, Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
struct DesktopCredentialVaultV1 {
    version: u8,
    signing_seed: Option<String>,
    encryption_secret: Option<String>,
    access_token: Option<String>,
    access_expires_at: Option<i64>,
    refresh_token: Option<String>,
    device_id: Option<String>,
    controller_nonce: u64,
    companion_secret: Option<String>,
    journal_key: Option<String>,
    hyperliquid_api_wallet_key: Option<String>,
}

impl fmt::Debug for DesktopCredentialVaultV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DesktopCredentialVaultV1")
            .field("version", &self.version)
            .field("signing_seed_present", &self.signing_seed.is_some())
            .field(
                "encryption_secret_present",
                &self.encryption_secret.is_some(),
            )
            .field("access_token_present", &self.access_token.is_some())
            .field("refresh_token_present", &self.refresh_token.is_some())
            .field("device_id_present", &self.device_id.is_some())
            .field("controller_nonce", &self.controller_nonce)
            .field("companion_secret_present", &self.companion_secret.is_some())
            .field("journal_key_present", &self.journal_key.is_some())
            .field(
                "hyperliquid_api_wallet_key_present",
                &self.hyperliquid_api_wallet_key.is_some(),
            )
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct CredentialVaultState {
    value: Option<DesktopCredentialVaultV1>,
    unavailable: bool,
}

static CREDENTIAL_VAULT: OnceLock<Mutex<CredentialVaultState>> = OnceLock::new();

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentStatus {
    protocol: &'static str,
    execution_boundary: &'static str,
    daemon: String,
    active_run: Option<String>,
    device_authorized: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicDeviceAuthorization {
    user_code: String,
    verification_uri: String,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthorizedDevice {
    device_id: String,
    #[serde(with = "time::serde::rfc3339")]
    access_expires_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all(serialize = "camelCase", deserialize = "snake_case"))]
struct RemoteDevice {
    id: String,
    device_label: String,
    platform: String,
    state: String,
    last_seen_at: Option<String>,
    encryption_public_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all(serialize = "camelCase", deserialize = "snake_case"))]
struct RemoteRun {
    id: String,
    arena_id: String,
    device_id: String,
    status: String,
    client_release: String,
    started_at: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteState {
    devices: Vec<RemoteDevice>,
    runs: Vec<RemoteRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all(serialize = "camelCase", deserialize = "snake_case"))]
struct PublicArena {
    id: String,
    mode: String,
    manifest: Value,
    state: String,
    starts_at: String,
    ends_at: String,
    tickets_enabled: bool,
    manifest_sha256: String,
    signer_public_key: String,
    signature: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicArenaState {
    arenas: Vec<PublicArena>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteCommandResult {
    command_id: String,
    action: String,
    accepted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all(serialize = "camelCase", deserialize = "snake_case"))]
struct AgentVersionSummary {
    id: String,
    agent_id: String,
    version: u32,
    model_id: String,
    configuration_sha256: String,
    created_at: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentVersionState {
    versions: Vec<AgentVersionSummary>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HyperliquidWalletSetup {
    address: String,
    approval_url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalRunSummary {
    run_id: String,
    arena_id: String,
    state: String,
    started_at: String,
    latest_at: String,
    arena_starts_at: Option<String>,
    arena_ends_at: Option<String>,
    decision_interval_seconds: Option<u32>,
    event_count: u64,
    cycle_count: u64,
    order_count: u64,
    fill_count: u64,
    all_receipted: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalRunEvent {
    sequence: u64,
    cycle_id: Option<String>,
    event_type: String,
    occurred_at: String,
    receipted: bool,
    details: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalRunJournal {
    runs: Vec<LocalRunSummary>,
    selected_run_id: Option<String>,
    events: Vec<LocalRunEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalArenaSchedule {
    starts_at: String,
    ends_at: String,
    decision_interval_seconds: u32,
}

#[derive(Debug)]
enum DesktopError {
    Credential,
    Authorization,
    NoAuthorization,
    Browser,
    Network,
    RemoteCommand,
    Companion,
    AgentVersion,
    Arena,
    Venue,
    VenueAccount,
    VenueCollateral,
    Journal,
}

impl DesktopError {
    const fn code(&self) -> &'static str {
        match self {
            Self::Credential => "credential_store_unavailable",
            Self::Authorization => "device_authorization_failed",
            Self::NoAuthorization => "device_authorization_not_started",
            Self::Browser => "browser_open_failed",
            Self::Network => "device_api_unavailable",
            Self::RemoteCommand => "remote_command_rejected",
            Self::Companion => "local_companion_unavailable",
            Self::AgentVersion => "agent_version_invalid",
            Self::Arena => "arena_operation_failed",
            Self::Venue => "hyperliquid_api_wallet_unavailable",
            Self::VenueAccount => "hyperliquid_account_state_unavailable",
            Self::VenueCollateral => "hyperliquid_testnet_collateral_required",
            Self::Journal => "local_journal_unavailable",
        }
    }
}

struct DesktopState {
    pending_authorization: Mutex<Option<DeviceAuthorizationSession>>,
    command_nonce: Mutex<()>,
    companion_nonce: Mutex<u64>,
    companion_request_lock: AsyncMutex<()>,
    companion_transition_lock: AsyncMutex<()>,
    companion_credentials: Mutex<Option<Arc<CompanionCredentials>>>,
    companion_credentials_unavailable: AtomicBool,
    companion_child: Mutex<Option<CommandChild>>,
    companion_slot: Arc<Mutex<CompanionSlot>>,
    authorization_status: Mutex<Option<bool>>,
    device_tokens: AsyncMutex<Option<Arc<DeviceTokens>>>,
    device_tokens_unavailable: AtomicBool,
}

#[derive(Default)]
struct CompanionSlot {
    generation: u64,
    spawned: bool,
}

struct CompanionCredentials {
    secret: Zeroizing<[u8; 32]>,
    ipc_name: String,
}

impl DesktopState {
    fn new() -> Self {
        Self {
            pending_authorization: Mutex::new(None),
            command_nonce: Mutex::new(()),
            companion_nonce: Mutex::new(0),
            companion_request_lock: AsyncMutex::new(()),
            companion_transition_lock: AsyncMutex::new(()),
            companion_credentials: Mutex::new(None),
            companion_credentials_unavailable: AtomicBool::new(false),
            companion_child: Mutex::new(None),
            companion_slot: Arc::new(Mutex::new(CompanionSlot::default())),
            // Never touch the OS credential store during startup or background
            // polling. Direct-distribution alpha binaries are ad-hoc signed,
            // so macOS may require approval after an update. Credential access
            // is deliberately unlocked only by an explicit user action.
            authorization_status: Mutex::new(Some(false)),
            device_tokens: AsyncMutex::new(None),
            device_tokens_unavailable: AtomicBool::new(false),
        }
    }

    fn companion_credentials(&self) -> Result<Arc<CompanionCredentials>, DesktopError> {
        if self
            .companion_credentials_unavailable
            .load(Ordering::SeqCst)
        {
            return Err(DesktopError::Credential);
        }
        let mut slot = self
            .companion_credentials
            .lock()
            .map_err(|_| DesktopError::Credential)?;
        if let Some(credentials) = slot.as_ref() {
            return Ok(Arc::clone(credentials));
        }
        let secret = match load_or_create_companion_secret() {
            Ok(secret) => secret,
            Err(error) => {
                self.companion_credentials_unavailable
                    .store(true, Ordering::SeqCst);
                return Err(error);
            }
        };
        let digest = sha256(secret.as_ref());
        let credentials = Arc::new(CompanionCredentials {
            secret,
            ipc_name: format!("crow-agent-{}", hex::encode(&digest[..12])),
        });
        *slot = Some(Arc::clone(&credentials));
        Ok(credentials)
    }

    fn device_authorized(&self) -> bool {
        let Ok(status) = self.authorization_status.lock() else {
            return false;
        };
        status.unwrap_or(false)
    }

    async fn cache_device_tokens(&self, tokens: DeviceTokens) {
        self.device_tokens_unavailable
            .store(false, Ordering::SeqCst);
        if let Ok(mut status) = self.authorization_status.lock() {
            *status = Some(true);
        }
        *self.device_tokens.lock().await = Some(Arc::new(tokens));
    }

    async fn device_tokens(&self) -> Result<Arc<DeviceTokens>, DesktopError> {
        // Gate every background API poll on one cached authorization check so
        // concurrent startup requests cannot each trigger Keychain UI.
        if !self.device_authorized() {
            return Err(DesktopError::NoAuthorization);
        }
        if self.device_tokens_unavailable.load(Ordering::SeqCst) {
            return Err(DesktopError::Credential);
        }
        let mut slot = self.device_tokens.lock().await;
        // Requests can pass the first check before one of them acquires this
        // lock. Recheck after serialization so one denial suppresses queued
        // and subsequent credential reads.
        if self.device_tokens_unavailable.load(Ordering::SeqCst) {
            return Err(DesktopError::Credential);
        }
        if let Some(tokens) = slot.as_ref()
            && tokens.access_expires_at > OffsetDateTime::now_utc() + time::Duration::minutes(1)
        {
            return Ok(Arc::clone(tokens));
        }
        match load_or_rotate_desktop_tokens().await {
            Ok(tokens) => {
                let tokens = Arc::new(tokens);
                *slot = Some(Arc::clone(&tokens));
                Ok(tokens)
            }
            Err(error) => {
                if matches!(error, DesktopError::Credential) {
                    self.device_tokens_unavailable.store(true, Ordering::SeqCst);
                }
                Err(error)
            }
        }
    }

    fn launch_companion(&self, app: &tauri::AppHandle) -> Result<(), DesktopError> {
        let credentials = self.companion_credentials()?;
        let Some(generation) = claim_companion_slot(&self.companion_slot)? else {
            return Ok(());
        };
        let Ok(command) = app.shell().sidecar("crow-agentd") else {
            release_companion_slot(&self.companion_slot, generation)?;
            return Err(DesktopError::Companion);
        };
        let Ok((mut events, mut child)) = command
            .args(["companion", "--ipc-name", &credentials.ipc_name])
            .spawn()
        else {
            release_companion_slot(&self.companion_slot, generation)?;
            return Err(DesktopError::Companion);
        };
        if child.write(credentials.secret.as_ref()).is_err() {
            let _ = child.kill();
            release_companion_slot(&self.companion_slot, generation)?;
            return Err(DesktopError::Companion);
        }
        let Ok(mut slot) = self.companion_child.lock() else {
            let _ = child.kill();
            release_companion_slot(&self.companion_slot, generation)?;
            return Err(DesktopError::Companion);
        };
        *slot = Some(child);
        let companion_slot = Arc::clone(&self.companion_slot);
        tauri::async_runtime::spawn(async move {
            while let Some(event) = events.recv().await {
                if matches!(event, CommandEvent::Error(_) | CommandEvent::Terminated(_)) {
                    let _ = release_companion_slot(&companion_slot, generation);
                    break;
                }
            }
        });
        Ok(())
    }

    async fn launch_desktop_run(
        &self,
        app: &tauri::AppHandle,
        config_path: &Path,
        credential_frame: &Zeroizing<Vec<u8>>,
    ) -> Result<(), DesktopError> {
        let credentials = self.companion_credentials()?;
        let config_path = config_path.to_str().ok_or(DesktopError::Companion)?;
        // Reserve the single companion slot before terminating the idle
        // process. Status polling continues while this command awaits; leaving
        // the slot false here lets a poll spawn a second listener on the same
        // socket and hide the live run's authenticated status response.
        let generation = reserve_companion_slot(&self.companion_slot)?;
        if let Ok(mut slot) = self.companion_child.lock()
            && let Some(child) = slot.take()
        {
            let _ = child.kill();
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let Ok(command) = app.shell().sidecar("crow-agentd") else {
            release_companion_slot(&self.companion_slot, generation)?;
            return Err(DesktopError::Companion);
        };
        let Ok((mut events, mut child)) = command
            .args([
                "desktop-run",
                config_path,
                "--ipc-name",
                &credentials.ipc_name,
            ])
            .spawn()
        else {
            release_companion_slot(&self.companion_slot, generation)?;
            return Err(DesktopError::Companion);
        };
        if child.write(credential_frame.as_ref()).is_err() {
            let _ = child.kill();
            release_companion_slot(&self.companion_slot, generation)?;
            return Err(DesktopError::Companion);
        }
        let Ok(mut slot) = self.companion_child.lock() else {
            let _ = child.kill();
            release_companion_slot(&self.companion_slot, generation)?;
            return Err(DesktopError::Companion);
        };
        *slot = Some(child);
        let companion_slot = Arc::clone(&self.companion_slot);
        tauri::async_runtime::spawn(async move {
            while let Some(event) = events.recv().await {
                if matches!(event, CommandEvent::Error(_) | CommandEvent::Terminated(_)) {
                    let _ = release_companion_slot(&companion_slot, generation);
                    break;
                }
            }
        });
        Ok(())
    }

    fn stop_owned_companion(&self) {
        let _ = invalidate_companion_slot(&self.companion_slot);
        if let Ok(mut slot) = self.companion_child.lock()
            && let Some(child) = slot.take()
        {
            let _ = child.kill();
        }
    }

    async fn companion_request(
        &self,
        action: CompanionActionV1,
    ) -> Result<CompanionResponseV1, DesktopError> {
        // Nonces are ordered by issuance, so the matching IPC requests must
        // remain ordered through delivery and response verification as well.
        // The WebView status poll can overlap a user command or authorization
        // refresh; without this lock, nonce N+1 may reach the daemon before N
        // and cause the earlier request to fail closed as a replay.
        let _request_guard = self.companion_request_lock.lock().await;
        let credentials = self.companion_credentials()?;
        let nonce = next_companion_nonce(&self.companion_nonce)?;
        let request = CompanionRequestV1::sign(&credentials.secret, nonce, action)
            .map_err(|_| DesktopError::Companion)?;
        tokio::time::timeout(
            COMPANION_TIMEOUT,
            send_companion_request(&credentials.ipc_name, &credentials.secret, &request),
        )
        .await
        .map_err(|_| DesktopError::Companion)?
    }
}

#[tauri::command]
async fn get_agent_status(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
) -> Result<AgentStatus, String> {
    let device_authorized = state.device_authorized();
    if !device_authorized {
        return Ok(AgentStatus {
            protocol: HARNESS_PROTOCOL_V1,
            execution_boundary: "local_device",
            daemon: "stopped".into(),
            active_run: None,
            device_authorized,
        });
    }
    let mut response = state
        .companion_request(CompanionActionV1::Status)
        .await
        .ok();
    if response.is_none() && state.launch_companion(&app).is_ok() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        response = state
            .companion_request(CompanionActionV1::Status)
            .await
            .ok();
    }
    Ok(AgentStatus {
        protocol: HARNESS_PROTOCOL_V1,
        execution_boundary: "local_device",
        daemon: response
            .as_ref()
            .map_or_else(|| "stopped".into(), |value| value.execution_state.clone()),
        active_run: response
            .and_then(|value| value.active_run)
            .map(|run_id| run_id.to_string()),
        device_authorized,
    })
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn get_local_run_journal(
    app: tauri::AppHandle,
    run_id: Option<String>,
    state: State<'_, DesktopState>,
) -> Result<LocalRunJournal, String> {
    if !state.device_authorized() {
        return Err(DesktopError::NoAuthorization.code().to_owned());
    }
    let runtime_directory =
        desktop_runtime_directory(&app).map_err(|_| DesktopError::Journal.code().to_owned())?;
    let path = runtime_directory.join("state/journal.db");
    if !path.exists() {
        return Ok(LocalRunJournal {
            runs: Vec::new(),
            selected_run_id: None,
            events: Vec::new(),
        });
    }
    let key =
        load_secret_32(JOURNAL_KEY_ACCOUNT).map_err(|_| DesktopError::Journal.code().to_owned())?;
    let journal =
        EncryptedJournal::open(&path, *key).map_err(|_| DesktopError::Journal.code().to_owned())?;
    let public_events = journal
        .public_events()
        .map_err(|_| DesktopError::Journal.code().to_owned())?;
    let schedules = load_local_arena_schedules(&runtime_directory, &public_events);
    Ok(build_local_run_journal(
        &public_events,
        run_id.as_deref(),
        &schedules,
    ))
}

fn load_local_arena_schedules(
    runtime_directory: &Path,
    public_events: &[crow_agent_protocol::RunEventEnvelopeV1],
) -> BTreeMap<Uuid, LocalArenaSchedule> {
    public_events
        .iter()
        .map(|event| event.arena_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|arena_id| {
            let bytes = fs::read(runtime_directory.join(format!("arena-{arena_id}.json"))).ok()?;
            verified_local_arena_schedule(&bytes, arena_id).map(|schedule| (arena_id, schedule))
        })
        .collect()
}

fn verified_local_arena_schedule(
    bytes: &[u8],
    expected_arena_id: Uuid,
) -> Option<LocalArenaSchedule> {
    let signed = serde_json::from_slice::<SignedArenaManifestV1>(bytes).ok()?;
    signed.verify().ok()?;
    if signed.manifest.arena_id != expected_arena_id {
        return None;
    }
    Some(LocalArenaSchedule {
        starts_at: signed
            .manifest
            .starts_at
            .format(&time::format_description::well_known::Rfc3339)
            .ok()?,
        ends_at: signed
            .manifest
            .ends_at
            .format(&time::format_description::well_known::Rfc3339)
            .ok()?,
        decision_interval_seconds: signed.manifest.decision_interval_seconds,
    })
}

fn build_local_run_journal(
    public_events: &[crow_agent_protocol::RunEventEnvelopeV1],
    requested_run_id: Option<&str>,
    schedules: &BTreeMap<Uuid, LocalArenaSchedule>,
) -> LocalRunJournal {
    let mut runs = Vec::<LocalRunSummary>::new();
    for event in public_events {
        let run_id = event.run_id.to_string();
        let occurred_at = event.occurred_at.to_string();
        let summary_index = if let Some(index) =
            runs.iter().position(|summary| summary.run_id == run_id)
        {
            index
        } else {
            let schedule = schedules.get(&event.arena_id);
            runs.push(LocalRunSummary {
                run_id: run_id.clone(),
                arena_id: event.arena_id.to_string(),
                state: "running".into(),
                started_at: occurred_at.clone(),
                latest_at: occurred_at.clone(),
                arena_starts_at: schedule.map(|value| value.starts_at.clone()),
                arena_ends_at: schedule.map(|value| value.ends_at.clone()),
                decision_interval_seconds: schedule.map(|value| value.decision_interval_seconds),
                event_count: 0,
                cycle_count: 0,
                order_count: 0,
                fill_count: 0,
                all_receipted: true,
            });
            runs.len() - 1
        };
        let summary = &mut runs[summary_index];
        summary.event_count += 1;
        summary.latest_at = occurred_at;
        summary.all_receipted &= event.server_receipt.is_some();
        match event.event_type.as_str() {
            "run_started" | "run_resumed" => summary.state = "running".into(),
            "run_paused" => summary.state = "paused".into(),
            "run_stopped" => summary.state = "stopped".into(),
            "cycle_started" => summary.cycle_count += 1,
            "order_submitted" => summary.order_count += 1,
            "fill" => {
                summary.fill_count += event
                    .payload
                    .get("fills")
                    .and_then(Value::as_array)
                    .map_or(0, |fills| u64::try_from(fills.len()).unwrap_or(u64::MAX));
            }
            _ => {}
        }
    }
    runs.sort_by(|left, right| right.latest_at.cmp(&left.latest_at));
    let selected_run_id = requested_run_id
        .filter(|requested| runs.iter().any(|summary| summary.run_id == *requested))
        .map(str::to_owned)
        .or_else(|| runs.first().map(|summary| summary.run_id.clone()));
    let events = selected_run_id
        .as_deref()
        .map_or_else(Vec::new, |selected| {
            public_events
                .iter()
                .filter(|event| event.run_id.to_string() == selected)
                .map(|event| LocalRunEvent {
                    sequence: event.sequence,
                    cycle_id: event.cycle_id.map(|cycle_id| cycle_id.to_string()),
                    event_type: event.event_type.clone(),
                    occurred_at: event.occurred_at.to_string(),
                    receipted: event.server_receipt.is_some(),
                    details: sanitize_journal_payload(&event.event_type, &event.payload),
                })
                .collect()
        });
    LocalRunJournal {
        runs,
        selected_run_id,
        events,
    }
}

fn sanitize_journal_payload(event_type: &str, payload: &Value) -> Value {
    let permitted = matches!(
        event_type,
        "run_started"
            | "run_paused"
            | "run_resumed"
            | "run_stopped"
            | "handoff_snapshot"
            | "cycle_started"
            | "cycle_completed"
            | "cycle_failed"
            | "cycle_missed"
            | "proposal"
            | "policy_outcome"
            | "order_submitted"
            | "venue_acknowledgement"
            | "fill"
            | "funding"
            | "reconciliation"
            | "portfolio_snapshot"
    );
    if !permitted {
        return json!({});
    }
    bounded_public_value(payload, 0)
}

fn bounded_public_value(value: &Value, depth: usize) -> Value {
    if depth >= 5 {
        return Value::Null;
    }
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
        Value::String(value) => Value::String(value.chars().take(256).collect()),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .take(50)
                .map(|value| bounded_public_value(value, depth + 1))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .filter(|(key, _)| !private_journal_field(key))
                .take(64)
                .map(|(key, value)| (key.clone(), bounded_public_value(value, depth + 1)))
                .collect(),
        ),
    }
}

fn private_journal_field(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "raw",
        "prompt",
        "transcript",
        "instruction",
        "strategy",
        "secret",
        "private",
        "credential",
        "authorization",
        "ciphertext",
        "signature",
        "hash",
        "receipt",
        "nonce",
        "address",
        "wallet",
    ]
    .iter()
    .any(|blocked| key.contains(blocked))
        || matches!(
            key.as_str(),
            "token"
                | "access_token"
                | "refresh_token"
                | "api_key"
                | "private_key"
                | "oid"
                | "cloid"
                | "tid"
                | "client_order_id"
                | "event_id"
                | "device_id"
        )
}

#[tauri::command]
async fn unlock_device_credentials(
    state: State<'_, DesktopState>,
) -> Result<AuthorizedDevice, String> {
    let has_refresh_token = read_credential_vault(|vault| {
        Ok(vault
            .refresh_token
            .as_ref()
            .is_some_and(|value| !value.is_empty()))
    })
    .map_err(|error| error.code().to_owned())?;
    if !has_refresh_token {
        return Err(DesktopError::NoAuthorization.code().to_owned());
    }
    if let Ok(mut status) = state.authorization_status.lock() {
        *status = Some(true);
    }
    state
        .device_tokens_unavailable
        .store(false, Ordering::SeqCst);
    match state.device_tokens().await {
        Ok(tokens) => Ok(AuthorizedDevice {
            device_id: tokens.device_id.to_string(),
            access_expires_at: tokens.access_expires_at,
        }),
        Err(error) => {
            if let Ok(mut status) = state.authorization_status.lock() {
                *status = Some(false);
            }
            Err(error.code().to_owned())
        }
    }
}

#[tauri::command]
async fn send_local_command(
    action: String,
    state: State<'_, DesktopState>,
) -> Result<AgentStatus, String> {
    let action = match action.as_str() {
        "pause" => CompanionActionV1::Pause,
        "resume" => CompanionActionV1::Resume,
        "stop" => CompanionActionV1::Stop,
        _ => return Err(DesktopError::Companion.code().into()),
    };
    let response = state
        .companion_request(action)
        .await
        .map_err(|error| error.code().to_owned())?;
    if !response.accepted {
        return Err(DesktopError::Companion.code().into());
    }
    Ok(AgentStatus {
        protocol: HARNESS_PROTOCOL_V1,
        execution_boundary: "local_device",
        daemon: response.execution_state,
        active_run: response.active_run.map(|run_id| run_id.to_string()),
        device_authorized: state.device_authorized(),
    })
}

#[tauri::command]
async fn begin_device_authorization(
    device_label: String,
    state: State<'_, DesktopState>,
) -> Result<PublicDeviceAuthorization, String> {
    let identity = load_or_create_identity().map_err(|error| error.code().to_owned())?;
    let encryption_key =
        load_or_create_encryption_key().map_err(|error| error.code().to_owned())?;
    let encryption_public_key = URL_SAFE_NO_PAD.encode(encryption_key.public_key());
    let client = DeviceAuthorizationClient::production()
        .map_err(|_| DesktopError::Authorization.code().to_owned())?;
    let session = client
        .start(
            &device_label,
            std::env::consts::OS,
            &identity,
            &encryption_public_key,
        )
        .await
        .map_err(|_| DesktopError::Authorization.code().to_owned())?;
    open::that_detached(session.verification_uri.as_str())
        .map_err(|_| DesktopError::Browser.code().to_owned())?;
    let public = PublicDeviceAuthorization {
        user_code: session.user_code.clone(),
        verification_uri: session.verification_uri.to_string(),
        expires_at: session.expires_at,
    };
    let mut pending = state
        .pending_authorization
        .lock()
        .map_err(|_| DesktopError::Authorization.code().to_owned())?;
    *pending = Some(session);
    Ok(public)
}

#[tauri::command]
async fn complete_device_authorization(
    state: State<'_, DesktopState>,
) -> Result<AuthorizedDevice, String> {
    let session = state
        .pending_authorization
        .lock()
        .map_err(|_| DesktopError::Authorization.code().to_owned())?
        .take()
        .ok_or_else(|| DesktopError::NoAuthorization.code().to_owned())?;
    let identity = load_or_create_identity().map_err(|error| error.code().to_owned())?;
    let client = DeviceAuthorizationClient::production()
        .map_err(|_| DesktopError::Authorization.code().to_owned())?;
    match client.exchange(&session, &identity).await {
        Ok(tokens) => {
            let authorized = AuthorizedDevice {
                device_id: tokens.device_id.to_string(),
                access_expires_at: tokens.access_expires_at,
            };
            store_tokens(&tokens).map_err(|error| error.code().to_owned())?;
            state.cache_device_tokens(tokens).await;
            Ok(authorized)
        }
        Err(error @ (DeviceAuthorizationError::Pending | DeviceAuthorizationError::Request(_))) => {
            if let Ok(mut pending) = state.pending_authorization.lock() {
                *pending = Some(session);
            }
            let code = match error {
                DeviceAuthorizationError::Pending => "device_authorization_pending",
                _ => DesktopError::Authorization.code(),
            };
            Err(code.to_owned())
        }
        Err(_) => Err(DesktopError::Authorization.code().to_owned()),
    }
}

#[tauri::command]
async fn get_remote_state(state: State<'_, DesktopState>) -> Result<RemoteState, String> {
    let token = state
        .device_tokens()
        .await
        .map_err(|error| error.code().to_owned())?;
    let devices = device_api::<Value>(
        reqwest::Method::GET,
        "/api/v1/harness/remote-devices",
        &token.access_token,
        None,
    )
    .await
    .map_err(|error| error.code().to_owned())?;
    let runs = device_api::<Value>(
        reqwest::Method::GET,
        "/api/v1/harness/runs",
        &token.access_token,
        None,
    )
    .await
    .map_err(|error| error.code().to_owned())?;
    let devices =
        serde_json::from_value::<Vec<RemoteDevice>>(array_field_or_empty(&devices, "devices"))
            .map_err(|_| DesktopError::Network.code().to_owned())?;
    let runs = serde_json::from_value::<Vec<RemoteRun>>(array_field_or_empty(&runs, "runs"))
        .map_err(|_| DesktopError::Network.code().to_owned())?;
    Ok(RemoteState { devices, runs })
}

#[tauri::command]
async fn get_public_arenas() -> Result<PublicArenaState, String> {
    Ok(PublicArenaState {
        arenas: fetch_public_arenas()
            .await
            .map_err(|error| error.code().to_owned())?,
    })
}

#[tauri::command]
async fn get_agent_versions(state: State<'_, DesktopState>) -> Result<AgentVersionState, String> {
    let token = state
        .device_tokens()
        .await
        .map_err(|error| error.code().to_owned())?;
    let versions = list_agent_version_envelopes(&token.access_token)
        .await
        .map_err(|error| error.code().to_owned())?;
    Ok(AgentVersionState {
        versions: versions.iter().map(agent_version_summary).collect(),
    })
}

#[tauri::command]
async fn create_agent_version(
    name: String,
    model_id: String,
    system_instructions: String,
    state: State<'_, DesktopState>,
) -> Result<AgentVersionSummary, String> {
    let token = state
        .device_tokens()
        .await
        .map_err(|error| error.code().to_owned())?;
    let devices = device_api::<Value>(
        reqwest::Method::GET,
        "/api/v1/harness/remote-devices",
        &token.access_token,
        None,
    )
    .await
    .map_err(|error| error.code().to_owned())?;
    let devices =
        serde_json::from_value::<Vec<RemoteDevice>>(array_field_or_empty(&devices, "devices"))
            .map_err(|_| DesktopError::AgentVersion.code().to_owned())?;
    let recipients = devices
        .into_iter()
        .filter(|device| device.state == "active")
        .map(|device| {
            Ok(AgentVersionRecipient {
                device_id: uuid::Uuid::parse_str(&device.id)
                    .map_err(|_| DesktopError::AgentVersion)?,
                encryption_public_key: decode_device_encryption_public_key(
                    &device.encryption_public_key,
                )
                .map_err(|_| DesktopError::AgentVersion)?,
            })
        })
        .collect::<Result<Vec<_>, DesktopError>>()
        .map_err(|error| error.code().to_owned())?;
    let version_id = Uuid::new_v4();
    let bundle = StrategyBundleV1 {
        protocol: HARNESS_PROTOCOL_V1.into(),
        version_id,
        model_id,
        name,
        system_instructions,
        tools: REQUIRED_STRATEGY_TOOLS.map(str::to_owned).to_vec(),
        created_at: OffsetDateTime::now_utc(),
    };
    let envelope = seal_agent_version(&bundle, Uuid::new_v4(), 1, &recipients)
        .map_err(|_| DesktopError::AgentVersion.code().to_owned())?;
    let _: Value = device_api(
        reqwest::Method::POST,
        "/api/v1/harness/agent-versions",
        &token.access_token,
        Some(
            serde_json::to_value(&envelope)
                .map_err(|_| DesktopError::AgentVersion.code().to_owned())?,
        ),
    )
    .await
    .map_err(|error| error.code().to_owned())?;
    Ok(agent_version_summary(&envelope))
}

#[tauri::command]
fn prepare_hyperliquid_wallet() -> Result<HyperliquidWalletSetup, String> {
    let key =
        load_or_create_hyperliquid_api_wallet_key().map_err(|error| error.code().to_owned())?;
    let address =
        hyperliquid_api_wallet_address(&key).map_err(|_| DesktopError::Venue.code().to_owned())?;
    open::that_detached(HYPERLIQUID_TESTNET_API_URL)
        .map_err(|_| DesktopError::Browser.code().to_owned())?;
    Ok(HyperliquidWalletSetup {
        address,
        approval_url: HYPERLIQUID_TESTNET_API_URL.into(),
    })
}

#[tauri::command]
async fn enroll_arena(
    arena_id: String,
    agent_version_id: String,
    model_id: String,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    let arena_id = Uuid::parse_str(&arena_id).map_err(|_| DesktopError::Arena.code().to_owned())?;
    let agent_version_id = Uuid::parse_str(&agent_version_id)
        .map_err(|_| DesktopError::AgentVersion.code().to_owned())?;
    let token = state
        .device_tokens()
        .await
        .map_err(|error| error.code().to_owned())?;
    enroll_arena_with_token(arena_id, agent_version_id, &model_id, &token.access_token)
        .await
        .map_err(|error| error.code().to_owned())
}

#[tauri::command]
#[allow(clippy::too_many_lines)]
async fn start_local_arena(
    app: tauri::AppHandle,
    state: State<'_, DesktopState>,
    arena_id: String,
    agent_version_id: String,
    execution_account: String,
    handoff_snapshot: Option<Value>,
) -> Result<AgentStatus, String> {
    let _transition_guard = state.companion_transition_lock.lock().await;
    if state
        .companion_request(CompanionActionV1::Status)
        .await
        .ok()
        .and_then(|response| response.active_run)
        .is_some()
    {
        return Err(DesktopError::Arena.code().into());
    }
    if !valid_execution_account(&execution_account) {
        return Err(DesktopError::Venue.code().into());
    }
    let api_wallet_key =
        load_or_create_hyperliquid_api_wallet_key().map_err(|error| error.code().to_owned())?;
    let venue = crow_agent_core::HyperliquidVenue::connect_testnet(&api_wallet_key)
        .await
        .map_err(|_| DesktopError::VenueAccount.code().to_owned())?;
    let account = venue
        .account_snapshot(&execution_account)
        .await
        .map_err(|_| DesktopError::VenueAccount.code().to_owned())?;
    validate_launch_account(&account).map_err(|error| error.code().to_owned())?;
    let arena_id = Uuid::parse_str(&arena_id).map_err(|_| DesktopError::Arena.code().to_owned())?;
    let agent_version_id = Uuid::parse_str(&agent_version_id)
        .map_err(|_| DesktopError::AgentVersion.code().to_owned())?;
    let token = state
        .device_tokens()
        .await
        .map_err(|error| error.code().to_owned())?;
    let arenas = fetch_public_arenas()
        .await
        .map_err(|error| error.code().to_owned())?;
    let arena = arenas
        .into_iter()
        .find(|arena| arena.id == arena_id.to_string())
        .ok_or_else(|| DesktopError::Arena.code().to_owned())?;
    let signed = SignedArenaManifestV1::from_signed_value(
        arena.manifest,
        arena.manifest_sha256,
        arena.signer_public_key,
        arena.signature,
    )
    .map_err(|_| DesktopError::Arena.code().to_owned())?;
    let envelope = list_agent_version_envelopes(&token.access_token)
        .await
        .map_err(|error| error.code().to_owned())?
        .into_iter()
        .find(|version| version.version_id == agent_version_id)
        .ok_or_else(|| DesktopError::AgentVersion.code().to_owned())?;
    let device_id = Uuid::parse_str(
        &load_password(DEVICE_ID_ACCOUNT).map_err(|error| error.code().to_owned())?,
    )
    .map_err(|_| DesktopError::Credential.code().to_owned())?;
    let device_encryption_key =
        load_or_create_encryption_key().map_err(|error| error.code().to_owned())?;
    let strategy = open_agent_version(&envelope, device_id, &device_encryption_key)
        .map_err(|_| DesktopError::AgentVersion.code().to_owned())?;
    if strategy.model_id != envelope.model_id
        || !signed
            .manifest
            .eligible_models
            .iter()
            .any(|model| model == &strategy.model_id)
    {
        return Err(DesktopError::AgentVersion.code().into());
    }
    enroll_arena_with_token(
        arena_id,
        agent_version_id,
        &strategy.model_id,
        &token.access_token,
    )
    .await
    .map_err(|error| error.code().to_owned())?;
    let identity = load_or_create_identity().map_err(|error| error.code().to_owned())?;
    let runner_tokens = DeviceAuthorizationClient::new(&api_origin())
        .map_err(|_| DesktopError::Authorization.code().to_owned())?
        .fork(&token.access_token, &identity)
        .await
        .map_err(|_| DesktopError::Authorization.code().to_owned())?;
    if runner_tokens.device_id != device_id {
        return Err(DesktopError::Authorization.code().into());
    }

    let runtime_directory =
        desktop_runtime_directory(&app).map_err(|error| error.code().to_owned())?;
    let state_directory = runtime_directory.join("state");
    create_private_directory(&runtime_directory)
        .and_then(|()| create_private_directory(&state_directory))
        .map_err(|error| error.code().to_owned())?;
    let manifest_path = runtime_directory.join(format!("arena-{arena_id}.json"));
    write_runtime_json(&manifest_path, &signed).map_err(|error| error.code().to_owned())?;
    let handoff_path = if let Some(snapshot) = handoff_snapshot {
        let path = runtime_directory.join(format!("handoff-{arena_id}.json"));
        write_runtime_json(&path, &snapshot).map_err(|error| error.code().to_owned())?;
        Some(path)
    } else {
        None
    };
    let config_path = runtime_directory.join(format!("run-{arena_id}.json"));
    let config = json!({
        "device_id": device_id,
        "relay_url": PRODUCTION_RELAY_URL,
        "api_origin": api_origin(),
        "state_directory": state_directory,
        "live_arena": {
            "arena_manifest": manifest_path,
            "handoff_snapshot": handoff_path,
            "agent_version_id": agent_version_id,
            "execution_account": execution_account.to_ascii_lowercase(),
            "model_id": strategy.model_id,
            "client_release": env!("CARGO_PKG_VERSION"),
        }
    });
    write_runtime_json(&config_path, &config).map_err(|error| error.code().to_owned())?;
    let credential_frame = desktop_credential_frame(&state, &runner_tokens.refresh_token)
        .map_err(|error| error.code().to_owned())?;
    state
        .launch_desktop_run(&app, &config_path, &credential_frame)
        .await
        .map_err(|error| error.code().to_owned())?;
    for _ in 0..75 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if let Ok(response) = state.companion_request(CompanionActionV1::Status).await
            && response.active_run.is_some()
        {
            return Ok(AgentStatus {
                protocol: HARNESS_PROTOCOL_V1,
                execution_boundary: "local_device",
                daemon: response.execution_state,
                active_run: response.active_run.map(|run_id| run_id.to_string()),
                device_authorized: true,
            });
        }
    }
    state.stop_owned_companion();
    Err(DesktopError::Companion.code().into())
}

fn validate_launch_account(account: &crow_agent_core::AccountSnapshot) -> Result<(), DesktopError> {
    if account.equity_micro_usdc <= 0 || account.withdrawable_micro_usdc <= 0 {
        return Err(DesktopError::VenueCollateral);
    }
    Ok(())
}

#[tauri::command]
async fn send_remote_command(
    target_device_id: String,
    run_id: String,
    action: String,
    state: State<'_, DesktopState>,
) -> Result<RemoteCommandResult, String> {
    let target = uuid::Uuid::parse_str(&target_device_id)
        .map_err(|_| DesktopError::RemoteCommand.code().to_owned())?;
    let run = uuid::Uuid::parse_str(&run_id)
        .map_err(|_| DesktopError::RemoteCommand.code().to_owned())?;
    let controller = load_password(DEVICE_ID_ACCOUNT)
        .and_then(|value| uuid::Uuid::parse_str(&value).map_err(|_| DesktopError::Credential))
        .map_err(|error| error.code().to_owned())?;
    let remote_action = match action.as_str() {
        "pause" => RemoteAction::Pause,
        "resume" => RemoteAction::Resume,
        "stop" => RemoteAction::Stop,
        _ => return Err(DesktopError::RemoteCommand.code().to_owned()),
    };
    let nonce = {
        let _guard = state
            .command_nonce
            .lock()
            .map_err(|_| DesktopError::RemoteCommand.code().to_owned())?;
        next_controller_nonce().map_err(|error| error.code().to_owned())?
    };
    let identity = load_or_create_identity().map_err(|error| error.code().to_owned())?;
    let issued_at = OffsetDateTime::now_utc();
    let command = RemoteCommandV1::sign(
        identity.signing_key(),
        target,
        run,
        remote_action,
        nonce,
        issued_at,
        issued_at + time::Duration::seconds(5),
        controller,
    )
    .map_err(|_| DesktopError::RemoteCommand.code().to_owned())?;
    let token = state
        .device_tokens()
        .await
        .map_err(|error| error.code().to_owned())?;
    let response = device_api::<Value>(
        reqwest::Method::POST,
        "/api/v1/harness/remote-commands",
        &token.access_token,
        Some(
            serde_json::to_value(&command)
                .map_err(|_| DesktopError::RemoteCommand.code().to_owned())?,
        ),
    )
    .await
    .map_err(|error| error.code().to_owned())?;
    let accepted = response
        .get("relay_receipt")
        .and_then(Value::as_str)
        .is_some();
    if !accepted {
        return Err(DesktopError::RemoteCommand.code().to_owned());
    }
    Ok(RemoteCommandResult {
        command_id: command.command_id.to_string(),
        action,
        accepted,
    })
}

fn credential_vault() -> &'static Mutex<CredentialVaultState> {
    CREDENTIAL_VAULT.get_or_init(|| Mutex::new(CredentialVaultState::default()))
}

fn load_credential_vault(state: &mut CredentialVaultState) -> Result<(), DesktopError> {
    if state.unavailable {
        return Err(DesktopError::Credential);
    }
    if state.value.is_some() {
        return Ok(());
    }
    let Ok(entry) = Entry::new(CREDENTIAL_SERVICE, CREDENTIAL_VAULT_ACCOUNT) else {
        state.unavailable = true;
        return Err(DesktopError::Credential);
    };
    let value = match entry.get_secret() {
        Ok(encoded) => {
            let encoded = Zeroizing::new(encoded);
            let Ok(value) = serde_json::from_slice::<DesktopCredentialVaultV1>(&encoded) else {
                state.unavailable = true;
                return Err(DesktopError::Credential);
            };
            if value.version != CREDENTIAL_VAULT_VERSION {
                state.unavailable = true;
                return Err(DesktopError::Credential);
            }
            value
        }
        Err(KeyringError::NoEntry) => {
            let mut value = DesktopCredentialVaultV1::default();
            value.version = CREDENTIAL_VAULT_VERSION;
            value
        }
        Err(_) => {
            state.unavailable = true;
            return Err(DesktopError::Credential);
        }
    };
    state.value = Some(value);
    Ok(())
}

fn persist_credential_vault(state: &mut CredentialVaultState) -> Result<(), DesktopError> {
    let Some(value) = state.value.as_ref() else {
        return Err(DesktopError::Credential);
    };
    let encoded = Zeroizing::new(serde_json::to_vec(value).map_err(|_| DesktopError::Credential)?);
    let result = Entry::new(CREDENTIAL_SERVICE, CREDENTIAL_VAULT_ACCOUNT)
        .and_then(|entry| entry.set_secret(encoded.as_ref()));
    if result.is_err() {
        state.unavailable = true;
        return Err(DesktopError::Credential);
    }
    Ok(())
}

fn read_credential_vault<R>(
    operation: impl FnOnce(&DesktopCredentialVaultV1) -> Result<R, DesktopError>,
) -> Result<R, DesktopError> {
    let mut state = credential_vault()
        .lock()
        .map_err(|_| DesktopError::Credential)?;
    load_credential_vault(&mut state)?;
    let value = state.value.as_ref().ok_or(DesktopError::Credential)?;
    operation(value)
}

fn update_credential_vault<R>(
    operation: impl FnOnce(&mut DesktopCredentialVaultV1) -> Result<R, DesktopError>,
) -> Result<R, DesktopError> {
    let mut state = credential_vault()
        .lock()
        .map_err(|_| DesktopError::Credential)?;
    load_credential_vault(&mut state)?;
    let value = state.value.as_mut().ok_or(DesktopError::Credential)?;
    let result = operation(value)?;
    persist_credential_vault(&mut state)?;
    Ok(result)
}

fn decode_vault_secret(value: &str) -> Result<Zeroizing<[u8; 32]>, DesktopError> {
    let decoded = Zeroizing::new(
        URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| DesktopError::Credential)?,
    );
    let secret = decoded
        .as_slice()
        .try_into()
        .map_err(|_| DesktopError::Credential)?;
    Ok(Zeroizing::new(secret))
}

fn load_or_create_identity() -> Result<DeviceIdentity, DesktopError> {
    if let Some(encoded) = read_credential_vault(|vault| Ok(vault.signing_seed.clone()))? {
        let seed = decode_vault_secret(&encoded)?;
        return Ok(DeviceIdentity::from_seed(&seed));
    }
    let identity = DeviceIdentity::generate();
    let seed = Zeroizing::new(identity.seed());
    update_credential_vault(|vault| {
        vault.signing_seed = Some(URL_SAFE_NO_PAD.encode(seed.as_ref()));
        Ok(())
    })?;
    Ok(identity)
}

fn load_or_create_encryption_key() -> Result<DeviceEncryptionKey, DesktopError> {
    if let Some(encoded) = read_credential_vault(|vault| Ok(vault.encryption_secret.clone()))? {
        return Ok(DeviceEncryptionKey::from_secret(*decode_vault_secret(
            &encoded,
        )?));
    }
    let encryption_key = DeviceEncryptionKey::generate();
    let secret = encryption_key.secret_bytes();
    update_credential_vault(|vault| {
        vault.encryption_secret = Some(URL_SAFE_NO_PAD.encode(secret.as_ref()));
        Ok(())
    })?;
    Ok(encryption_key)
}

fn store_tokens(tokens: &DeviceTokens) -> Result<(), DesktopError> {
    update_credential_vault(|vault| {
        vault.access_token = Some(tokens.access_token.to_string());
        vault.access_expires_at = Some(tokens.access_expires_at.unix_timestamp());
        vault.refresh_token = Some(tokens.refresh_token.to_string());
        vault.device_id = Some(tokens.device_id.to_string());
        Ok(())
    })
}

async fn load_or_rotate_desktop_tokens() -> Result<DeviceTokens, DesktopError> {
    // Load the refresh token first and short-circuit when access is denied. A
    // tuple containing all four reads would evaluate every Keychain request.
    let refresh_token = Zeroizing::new(load_password(REFRESH_TOKEN_ACCOUNT)?);
    if let (Ok(access_token), Ok(device_id), Ok(expires_at)) = (
        load_password(ACCESS_TOKEN_ACCOUNT),
        load_password(DEVICE_ID_ACCOUNT),
        load_password(ACCESS_EXPIRES_AT_ACCOUNT),
    ) && let (Ok(device_id), Ok(timestamp)) =
        (Uuid::parse_str(&device_id), expires_at.parse::<i64>())
        && let Ok(access_expires_at) = OffsetDateTime::from_unix_timestamp(timestamp)
        && access_expires_at > OffsetDateTime::now_utc() + time::Duration::minutes(1)
    {
        return Ok(DeviceTokens {
            device_id,
            access_token: Zeroizing::new(access_token),
            refresh_token,
            access_expires_at,
        });
    }
    if !refresh_token.starts_with("crow_device_refresh_") {
        return Err(DesktopError::Credential);
    }
    let identity = load_or_create_identity()?;
    let client =
        DeviceAuthorizationClient::new(&api_origin()).map_err(|_| DesktopError::Authorization)?;
    let tokens = client
        .rotate(&refresh_token, &identity)
        .await
        .map_err(|_| DesktopError::Authorization)?;
    store_tokens(&tokens)?;
    Ok(tokens)
}

async fn device_api<T: DeserializeOwned>(
    method: reqwest::Method,
    path: &str,
    access_token: &Zeroizing<String>,
    body: Option<Value>,
) -> Result<T, DesktopError> {
    let endpoint = Url::parse(&api_origin())
        .and_then(|origin| origin.join(path))
        .map_err(|_| DesktopError::Network)?;
    let client = reqwest::Client::builder()
        .https_only(endpoint.scheme() == "https")
        .build()
        .map_err(|_| DesktopError::Network)?;
    let mut request = client
        .request(method, endpoint)
        .bearer_auth(access_token.as_str());
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.map_err(|_| DesktopError::Network)?;
    if !response.status().is_success() {
        return Err(DesktopError::RemoteCommand);
    }
    response
        .json::<T>()
        .await
        .map_err(|_| DesktopError::Network)
}

async fn device_api_empty(
    method: reqwest::Method,
    path: &str,
    access_token: &Zeroizing<String>,
    body: Value,
) -> Result<(), DesktopError> {
    let endpoint = Url::parse(&api_origin())
        .and_then(|origin| origin.join(path))
        .map_err(|_| DesktopError::Network)?;
    let response = reqwest::Client::builder()
        .https_only(endpoint.scheme() == "https")
        .build()
        .map_err(|_| DesktopError::Network)?
        .request(method, endpoint)
        .bearer_auth(access_token.as_str())
        .json(&body)
        .send()
        .await
        .map_err(|_| DesktopError::Network)?;
    if !response.status().is_success() {
        return Err(DesktopError::Arena);
    }
    Ok(())
}

fn array_field_or_empty(payload: &Value, field: &str) -> Value {
    match payload.get(field) {
        None | Some(Value::Null) => json!([]),
        Some(value) => value.clone(),
    }
}

async fn fetch_public_arenas() -> Result<Vec<PublicArena>, DesktopError> {
    let endpoint = Url::parse(&api_origin())
        .and_then(|origin| origin.join("/api/v1/harness/arenas"))
        .map_err(|_| DesktopError::Network)?;
    let response = reqwest::Client::builder()
        .https_only(endpoint.scheme() == "https")
        .build()
        .map_err(|_| DesktopError::Network)?
        .get(endpoint)
        .send()
        .await
        .map_err(|_| DesktopError::Network)?;
    if response.status() != StatusCode::OK {
        return Err(DesktopError::Network);
    }
    let payload = response
        .json::<Value>()
        .await
        .map_err(|_| DesktopError::Network)?;
    serde_json::from_value::<Vec<PublicArena>>(array_field_or_empty(&payload, "arenas"))
        .map_err(|_| DesktopError::Network)
}

async fn list_agent_version_envelopes(
    access_token: &Zeroizing<String>,
) -> Result<Vec<AgentVersionEnvelopeV1>, DesktopError> {
    let payload = device_api::<Value>(
        reqwest::Method::GET,
        "/api/v1/harness/agent-versions",
        access_token,
        None,
    )
    .await?;
    serde_json::from_value::<Vec<AgentVersionEnvelopeV1>>(array_field_or_empty(
        &payload, "versions",
    ))
    .map_err(|_| DesktopError::AgentVersion)
}

async fn enroll_arena_with_token(
    arena_id: Uuid,
    agent_version_id: Uuid,
    model_id: &str,
    access_token: &Zeroizing<String>,
) -> Result<(), DesktopError> {
    device_api_empty(
        reqwest::Method::POST,
        &format!("/api/v1/harness/device/arenas/{arena_id}/enrollments"),
        access_token,
        json!({
            "agent_version_id": agent_version_id,
            "model_id": model_id,
        }),
    )
    .await
}

fn agent_version_summary(envelope: &AgentVersionEnvelopeV1) -> AgentVersionSummary {
    let created_at = match envelope
        .created_at
        .format(&time::format_description::well_known::Rfc3339)
    {
        Ok(value) => value,
        Err(_) => envelope.created_at.unix_timestamp().to_string(),
    };
    AgentVersionSummary {
        id: envelope.version_id.to_string(),
        agent_id: envelope.agent_id.to_string(),
        version: envelope.version,
        model_id: envelope.model_id.clone(),
        configuration_sha256: envelope.configuration_sha256.clone(),
        created_at,
    }
}

fn load_or_create_hyperliquid_api_wallet_key() -> Result<Zeroizing<[u8; 32]>, DesktopError> {
    if let Some(encoded) =
        read_credential_vault(|vault| Ok(vault.hyperliquid_api_wallet_key.clone()))?
    {
        let key = decode_vault_secret(&encoded)?;
        hyperliquid_api_wallet_address(&key).map_err(|_| DesktopError::Venue)?;
        return Ok(key);
    }
    for _ in 0..64 {
        let mut key = Zeroizing::new([0_u8; 32]);
        OsRng.fill_bytes(key.as_mut());
        if hyperliquid_api_wallet_address(&key).is_ok() {
            update_credential_vault(|vault| {
                vault.hyperliquid_api_wallet_key = Some(URL_SAFE_NO_PAD.encode(key.as_ref()));
                Ok(())
            })?;
            return Ok(key);
        }
    }
    Err(DesktopError::Venue)
}

fn vault_secret_for_account(vault: &DesktopCredentialVaultV1, account: &str) -> Option<String> {
    match account {
        SIGNING_SEED_ACCOUNT => vault.signing_seed.clone(),
        ENCRYPTION_SECRET_ACCOUNT => vault.encryption_secret.clone(),
        COMPANION_SECRET_ACCOUNT => vault.companion_secret.clone(),
        JOURNAL_KEY_ACCOUNT => vault.journal_key.clone(),
        HYPERLIQUID_API_WALLET_ACCOUNT => vault.hyperliquid_api_wallet_key.clone(),
        _ => None,
    }
}

fn set_vault_secret_for_account(
    vault: &mut DesktopCredentialVaultV1,
    account: &str,
    encoded: String,
) -> Result<(), DesktopError> {
    match account {
        SIGNING_SEED_ACCOUNT => vault.signing_seed = Some(encoded),
        ENCRYPTION_SECRET_ACCOUNT => vault.encryption_secret = Some(encoded),
        COMPANION_SECRET_ACCOUNT => vault.companion_secret = Some(encoded),
        JOURNAL_KEY_ACCOUNT => vault.journal_key = Some(encoded),
        HYPERLIQUID_API_WALLET_ACCOUNT => vault.hyperliquid_api_wallet_key = Some(encoded),
        _ => return Err(DesktopError::Credential),
    }
    Ok(())
}

fn load_secret_32(account: &str) -> Result<Zeroizing<[u8; 32]>, DesktopError> {
    let encoded = read_credential_vault(|vault| {
        vault_secret_for_account(vault, account).ok_or(DesktopError::Credential)
    })?;
    decode_vault_secret(&encoded)
}

fn load_or_create_secret_32(account: &str) -> Result<Zeroizing<[u8; 32]>, DesktopError> {
    if let Some(encoded) =
        read_credential_vault(|vault| Ok(vault_secret_for_account(vault, account)))?
    {
        return decode_vault_secret(&encoded);
    }
    let mut value = Zeroizing::new([0_u8; 32]);
    OsRng.fill_bytes(value.as_mut());
    update_credential_vault(|vault| {
        set_vault_secret_for_account(vault, account, URL_SAFE_NO_PAD.encode(value.as_ref()))
    })?;
    Ok(value)
}

fn desktop_runtime_directory(app: &tauri::AppHandle) -> Result<PathBuf, DesktopError> {
    app.path()
        .app_data_dir()
        .map(|path| path.join("runtime"))
        .map_err(|_| DesktopError::Companion)
}

fn create_private_directory(path: &Path) -> Result<(), DesktopError> {
    fs::create_dir_all(path).map_err(|_| DesktopError::Companion)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| DesktopError::Companion)?;
    }
    Ok(())
}

fn write_runtime_json(path: &Path, value: &impl Serialize) -> Result<(), DesktopError> {
    let encoded = serde_json::to_vec(value).map_err(|_| DesktopError::Companion)?;
    fs::write(path, encoded).map_err(|_| DesktopError::Companion)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| DesktopError::Companion)?;
    }
    Ok(())
}

fn desktop_credential_frame(
    state: &DesktopState,
    runner_refresh_token: &Zeroizing<String>,
) -> Result<Zeroizing<Vec<u8>>, DesktopError> {
    let companion_credentials = state.companion_credentials()?;
    let signing_seed = load_secret_32(SIGNING_SEED_ACCOUNT)?;
    let encryption_secret = load_secret_32(ENCRYPTION_SECRET_ACCOUNT)?;
    let journal_key = load_or_create_secret_32(JOURNAL_KEY_ACCOUNT)?;
    let api_wallet_key = load_or_create_hyperliquid_api_wallet_key()?;
    if runner_refresh_token.len() > MAX_DESKTOP_REFRESH_TOKEN_BYTES
        || runner_refresh_token.len() > usize::from(u16::MAX)
    {
        return Err(DesktopError::Credential);
    }
    let length = u16::try_from(runner_refresh_token.len()).map_err(|_| DesktopError::Credential)?;
    let mut frame = Zeroizing::new(Vec::with_capacity(32 * 5 + 2 + runner_refresh_token.len()));
    frame.extend_from_slice(companion_credentials.secret.as_ref());
    frame.extend_from_slice(signing_seed.as_ref());
    frame.extend_from_slice(encryption_secret.as_ref());
    frame.extend_from_slice(journal_key.as_ref());
    frame.extend_from_slice(api_wallet_key.as_ref());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(runner_refresh_token.as_bytes());
    Ok(frame)
}

fn valid_execution_account(value: &str) -> bool {
    value.len() == 42
        && value.starts_with("0x")
        && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn next_controller_nonce() -> Result<u64, DesktopError> {
    next_persisted_nonce(CONTROLLER_NONCE_ACCOUNT)
}

fn next_companion_nonce(nonce: &Mutex<u64>) -> Result<u64, DesktopError> {
    let mut nonce = nonce.lock().map_err(|_| DesktopError::Companion)?;
    *nonce = nonce.checked_add(1).ok_or(DesktopError::Companion)?;
    Ok(*nonce)
}

fn claim_companion_slot(slot: &Mutex<CompanionSlot>) -> Result<Option<u64>, DesktopError> {
    let mut slot = slot.lock().map_err(|_| DesktopError::Companion)?;
    if slot.spawned {
        return Ok(None);
    }
    slot.generation = slot
        .generation
        .checked_add(1)
        .ok_or(DesktopError::Companion)?;
    slot.spawned = true;
    Ok(Some(slot.generation))
}

fn reserve_companion_slot(slot: &Mutex<CompanionSlot>) -> Result<u64, DesktopError> {
    let mut slot = slot.lock().map_err(|_| DesktopError::Companion)?;
    slot.generation = slot
        .generation
        .checked_add(1)
        .ok_or(DesktopError::Companion)?;
    slot.spawned = true;
    Ok(slot.generation)
}

fn release_companion_slot(
    slot: &Mutex<CompanionSlot>,
    generation: u64,
) -> Result<bool, DesktopError> {
    let mut slot = slot.lock().map_err(|_| DesktopError::Companion)?;
    if slot.generation != generation {
        return Ok(false);
    }
    slot.spawned = false;
    Ok(true)
}

fn invalidate_companion_slot(slot: &Mutex<CompanionSlot>) -> Result<(), DesktopError> {
    let mut slot = slot.lock().map_err(|_| DesktopError::Companion)?;
    slot.generation = slot
        .generation
        .checked_add(1)
        .ok_or(DesktopError::Companion)?;
    slot.spawned = false;
    Ok(())
}

fn next_persisted_nonce(account: &str) -> Result<u64, DesktopError> {
    if account != CONTROLLER_NONCE_ACCOUNT {
        return Err(DesktopError::RemoteCommand);
    }
    update_credential_vault(|vault| {
        let next = vault
            .controller_nonce
            .checked_add(1)
            .ok_or(DesktopError::RemoteCommand)?;
        vault.controller_nonce = next;
        Ok(next)
    })
}

fn load_or_create_companion_secret() -> Result<Zeroizing<[u8; 32]>, DesktopError> {
    load_or_create_secret_32(COMPANION_SECRET_ACCOUNT)
}

fn local_socket_name(label: &str) -> Result<Name<'static>, DesktopError> {
    if GenericNamespaced::is_supported() {
        label
            .to_ns_name::<GenericNamespaced>()
            .map(Name::into_owned)
            .map_err(|_| DesktopError::Companion)
    } else {
        std::env::temp_dir()
            .join(format!("{label}.sock"))
            .to_fs_name::<GenericFilePath>()
            .map(Name::into_owned)
            .map_err(|_| DesktopError::Companion)
    }
}

async fn send_companion_request(
    ipc_name: &str,
    secret: &[u8; 32],
    request: &CompanionRequestV1,
) -> Result<CompanionResponseV1, DesktopError> {
    let name = local_socket_name(ipc_name)?;
    let connection = LocalSocketStream::connect(name)
        .await
        .map_err(|_| DesktopError::Companion)?;
    let mut encoded = serde_json::to_vec(request).map_err(|_| DesktopError::Companion)?;
    encoded.push(b'\n');
    let mut sender = &connection;
    sender
        .write_all(&encoded)
        .await
        .map_err(|_| DesktopError::Companion)?;
    let reader = BufReader::new(&connection);
    let mut reader = reader.take((MAX_COMPANION_MESSAGE_BYTES + 1) as u64);
    let mut raw = String::new();
    let bytes_read = reader
        .read_line(&mut raw)
        .await
        .map_err(|_| DesktopError::Companion)?;
    if bytes_read == 0 || bytes_read > MAX_COMPANION_MESSAGE_BYTES {
        return Err(DesktopError::Companion);
    }
    let response = serde_json::from_str::<CompanionResponseV1>(raw.trim_end())
        .map_err(|_| DesktopError::Companion)?;
    response
        .verify(secret, request)
        .map_err(|_| DesktopError::Companion)?;
    Ok(response)
}

fn load_password(account: &str) -> Result<String, DesktopError> {
    read_credential_vault(|vault| {
        let value = match account {
            ACCESS_TOKEN_ACCOUNT => vault.access_token.clone(),
            ACCESS_EXPIRES_AT_ACCOUNT => vault.access_expires_at.map(|value| value.to_string()),
            REFRESH_TOKEN_ACCOUNT => vault.refresh_token.clone(),
            DEVICE_ID_ACCOUNT => vault.device_id.clone(),
            CONTROLLER_NONCE_ACCOUNT => Some(vault.controller_nonce.to_string()),
            _ => None,
        };
        value.ok_or(DesktopError::Credential)
    })
}

fn api_origin() -> String {
    std::env::var("CROW_API_ORIGIN").unwrap_or_else(|_| PRODUCTION_API_ORIGIN.into())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    if let Err(error) = crow_agent_core::install_tls_crypto_provider() {
        eprintln!("Crow Agent TLS initialization failed: {error}");
        std::process::exit(1);
    }
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            app.manage(DesktopState::new());
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.minimize();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_agent_status,
            get_local_run_journal,
            unlock_device_credentials,
            send_local_command,
            begin_device_authorization,
            complete_device_authorization,
            get_remote_state,
            get_public_arenas,
            get_agent_versions,
            create_agent_version,
            prepare_hyperliquid_wallet,
            enroll_arena,
            start_local_arena,
            send_remote_command
        ])
        .build(tauri::generate_context!())
        .unwrap_or_else(|error| {
            eprintln!("Crow Agent desktop runtime failed: {error}");
            std::process::exit(1);
        });
    app.run(|handle, event| {
        if should_show_main_window(&event)
            && let Some(window) = handle.get_webview_window("main")
        {
            let _ = window.show();
            let _ = window.unminimize();
            let _ = window.set_focus();
        }
    });
}

#[cfg(target_os = "macos")]
fn should_show_main_window(event: &tauri::RunEvent) -> bool {
    matches!(
        event,
        tauri::RunEvent::Ready | tauri::RunEvent::Reopen { .. }
    )
}

#[cfg(not(target_os = "macos"))]
fn should_show_main_window(event: &tauri::RunEvent) -> bool {
    matches!(event, tauri::RunEvent::Ready)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn companion_poll_nonce_is_monotonic_without_keychain_io() {
        let nonce = Mutex::new(0);
        assert_eq!(
            next_companion_nonce(&nonce).map_err(|error| error.code()),
            Ok(1)
        );
        assert_eq!(
            next_companion_nonce(&nonce).map_err(|error| error.code()),
            Ok(2)
        );
    }

    #[tokio::test]
    async fn startup_polling_stays_locked_without_credential_reads() {
        let state = DesktopState::new();
        assert!(!state.device_authorized());
        assert!(matches!(
            state.device_tokens().await,
            Err(DesktopError::NoAuthorization)
        ));
        assert!(state.device_tokens.lock().await.is_none());
    }

    #[test]
    fn desktop_run_transition_blocks_idle_companion_race() -> Result<(), DesktopError> {
        let slot = Mutex::new(CompanionSlot::default());
        reserve_companion_slot(&slot)?;
        assert!(claim_companion_slot(&slot)?.is_none());
        assert!(slot.lock().map_err(|_| DesktopError::Companion)?.spawned);
        Ok(())
    }

    #[test]
    fn retired_companion_cannot_release_new_generation() -> Result<(), DesktopError> {
        let slot = Mutex::new(CompanionSlot::default());
        let idle_generation =
            claim_companion_slot(&slot).and_then(|value| value.ok_or(DesktopError::Companion))?;
        let run_generation = reserve_companion_slot(&slot)?;

        assert!(!release_companion_slot(&slot, idle_generation)?);
        assert!(slot.lock().map_err(|_| DesktopError::Companion)?.spawned);
        assert!(release_companion_slot(&slot, run_generation)?);
        assert!(!slot.lock().map_err(|_| DesktopError::Companion)?.spawned);
        Ok(())
    }

    #[test]
    fn arena_launch_requires_positive_verified_collateral() {
        let account = crow_agent_core::AccountSnapshot {
            venue_time_ms: 1,
            equity_micro_usdc: 1_001_473_289,
            withdrawable_micro_usdc: 1_001_473_289,
            positions: BTreeMap::new(),
        };
        assert!(validate_launch_account(&account).is_ok());
        let empty = crow_agent_core::AccountSnapshot {
            equity_micro_usdc: 0,
            withdrawable_micro_usdc: 0,
            ..account
        };
        assert!(matches!(
            validate_launch_account(&empty),
            Err(DesktopError::VenueCollateral)
        ));
    }

    #[test]
    fn remote_state_accepts_backend_snake_case_payloads() -> Result<(), serde_json::Error> {
        let device = serde_json::from_value::<RemoteDevice>(json!({
            "id": Uuid::nil().to_string(),
            "device_label": "Crow desktop",
            "platform": "macos",
            "state": "active",
            "last_seen_at": "2026-07-29T01:41:40.17766Z",
            "encryption_public_key": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "signing_public_key": "ignored by the desktop inventory",
            "inference_api_key_id": Uuid::nil().to_string(),
            "created_at": "2026-07-29T01:31:41.825668Z"
        }))?;
        assert_eq!(device.device_label, "Crow desktop");
        assert_eq!(device.platform, "macos");

        let runs = serde_json::from_value::<Vec<RemoteRun>>(json!([]))?;
        assert!(runs.is_empty());
        let null_runs = serde_json::from_value::<Vec<RemoteRun>>(array_field_or_empty(
            &json!({"runs": null}),
            "runs",
        ))?;
        assert!(null_runs.is_empty());
        Ok(())
    }

    #[test]
    fn credential_vault_debug_redacts_every_secret() {
        let mut vault = DesktopCredentialVaultV1::default();
        vault.version = CREDENTIAL_VAULT_VERSION;
        vault.signing_seed = Some("signing-sentinel".into());
        vault.refresh_token = Some("refresh-sentinel".into());
        vault.hyperliquid_api_wallet_key = Some("wallet-sentinel".into());
        let debug = format!("{vault:?}");
        assert!(!debug.contains("signing-sentinel"));
        assert!(!debug.contains("refresh-sentinel"));
        assert!(!debug.contains("wallet-sentinel"));
        assert!(debug.contains("signing_seed_present: true"));
    }

    #[test]
    fn journal_payload_view_is_bounded_and_redacts_private_fields() {
        let sanitized = sanitize_journal_payload(
            "fill",
            &json!({
                "fills": [{
                    "coin": "BTC",
                    "px": "118000",
                    "fee": "0.02",
                    "oid": 42,
                    "wallet_address": "0xprivate",
                    "raw_transcript": "private reasoning",
                    "api_key": "private key"
                }],
                "device_signature": "private signature"
            }),
        );
        assert_eq!(sanitized["fills"][0]["coin"], "BTC");
        assert_eq!(sanitized["fills"][0]["fee"], "0.02");
        assert!(sanitized["fills"][0].get("oid").is_none());
        assert!(sanitized["fills"][0].get("wallet_address").is_none());
        assert!(sanitized["fills"][0].get("raw_transcript").is_none());
        assert!(sanitized["fills"][0].get("api_key").is_none());
        assert!(sanitized.get("device_signature").is_none());
        assert_eq!(
            sanitize_journal_payload(
                "inference_receipt",
                &json!({"receipt_id": "not-for-webview"})
            ),
            json!({})
        );
        let decision = sanitize_journal_payload(
            "proposal",
            &json!({
                "action": "hold",
                "decision_summary": "BTC, ETH, and SOL signals are mixed, so no compliant entry is justified.",
                "proposal": null,
                "raw_reasoning": "private chain of thought",
                "strategy_instructions": "private strategy"
            }),
        );
        assert_eq!(decision["action"], "hold");
        assert_eq!(
            decision["decision_summary"],
            "BTC, ETH, and SOL signals are mixed, so no compliant entry is justified."
        );
        assert!(decision.get("raw_reasoning").is_none());
        assert!(decision.get("strategy_instructions").is_none());
        assert_eq!(
            sanitize_journal_payload(
                "cycle_failed",
                &json!({
                    "stage": "model_decision",
                    "reason": "receipt_binding_failed",
                    "order_submitted": false,
                    "raw_response": "private"
                })
            ),
            json!({
                "stage": "model_decision",
                "reason": "receipt_binding_failed",
                "order_submitted": false
            })
        );
    }

    #[test]
    fn journal_summary_counts_trade_activity_and_lifecycle()
    -> Result<(), Box<dyn std::error::Error>> {
        let identity = DeviceIdentity::generate();
        let arena_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let started = crow_agent_protocol::RunEventEnvelopeV1::sign(
            identity.signing_key(),
            arena_id,
            run_id,
            None,
            1,
            "0".repeat(64),
            "run_started".into(),
            OffsetDateTime::now_utc(),
            json!({"mode": "hyperliquid_testnet"}),
        )?;
        let mut fill = crow_agent_protocol::RunEventEnvelopeV1::sign(
            identity.signing_key(),
            arena_id,
            run_id,
            Some(Uuid::new_v4()),
            2,
            started.event_sha256.clone(),
            "fill".into(),
            OffsetDateTime::now_utc(),
            json!({"fills": [{"coin": "BTC"}, {"coin": "ETH"}]}),
        )?;
        fill.server_receipt = Some("server-receipt".into());
        let paused = crow_agent_protocol::RunEventEnvelopeV1::sign(
            identity.signing_key(),
            arena_id,
            run_id,
            None,
            3,
            fill.event_sha256.clone(),
            "run_paused".into(),
            OffsetDateTime::now_utc(),
            json!({"source": "authenticated_control"}),
        )?;

        let events = [started, fill, paused];
        let journal = build_local_run_journal(&events, Some(&run_id.to_string()), &BTreeMap::new());
        assert_eq!(journal.runs.len(), 1);
        assert_eq!(journal.runs[0].state, "paused");
        assert_eq!(journal.runs[0].fill_count, 2);
        assert!(!journal.runs[0].all_receipted);
        assert_eq!(journal.events.len(), 3);
        assert_eq!(journal.events[1].details["fills"][0]["coin"], "BTC");
        Ok(())
    }

    #[test]
    fn journal_schedule_requires_the_exact_signed_arena_manifest()
    -> Result<(), Box<dyn std::error::Error>> {
        let identity = DeviceIdentity::generate();
        let arena_id = Uuid::new_v4();
        let manifest = crow_agent_protocol::ArenaManifestV1 {
            protocol: HARNESS_PROTOCOL_V1.into(),
            arena_id,
            manifest_version: 1,
            mode: crow_agent_protocol::ArenaMode::HyperliquidTestnet,
            starts_at: time::macros::datetime!(2026-07-30 04:15 UTC),
            ends_at: time::macros::datetime!(2026-07-30 04:45 UTC),
            decision_interval_seconds: 900,
            symbols: crow_agent_protocol::ALLOWED_SYMBOLS
                .map(str::to_owned)
                .to_vec(),
            eligible_models: vec![crow_agent_protocol::ALLOWED_MODELS[0].into()],
            dataset_sha256: None,
            required_client_version: "0.1.14".into(),
            risk_rules: crow_agent_protocol::RiskRulesV1::default(),
            execution: crow_agent_protocol::ExecutionAssumptionsV1 {
                half_spread_bps: 2,
                slippage_bps: 3,
                taker_fee_bps: 5,
            },
            scoring: crow_agent_protocol::ScoringWeightsV1::default(),
            penalties: crow_agent_protocol::PenaltyRulesV1::default(),
            ticket: crow_agent_protocol::TicketConfigV1::default(),
        };
        let signed = SignedArenaManifestV1::sign(manifest, identity.signing_key())?;
        let encoded = serde_json::to_vec(&signed)?;
        let schedule =
            verified_local_arena_schedule(&encoded, arena_id).ok_or("missing schedule")?;
        assert_eq!(schedule.starts_at, "2026-07-30T04:15:00Z");
        assert_eq!(schedule.ends_at, "2026-07-30T04:45:00Z");
        assert_eq!(schedule.decision_interval_seconds, 900);
        assert!(verified_local_arena_schedule(&encoded, Uuid::new_v4()).is_none());

        let mut mutated = serde_json::to_value(signed)?;
        mutated["manifest"]["starts_at"] = json!("2026-07-30T04:16:00Z");
        assert!(verified_local_arena_schedule(&serde_json::to_vec(&mutated)?, arena_id).is_none());
        Ok(())
    }
}
