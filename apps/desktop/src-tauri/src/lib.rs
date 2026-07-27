use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crow_agent_core::{
    DeviceAuthorizationClient, DeviceAuthorizationError, DeviceAuthorizationSession,
    DeviceEncryptionKey, DeviceTokens,
};
use crow_agent_protocol::{DeviceIdentity, HARNESS_PROTOCOL_V1, RemoteAction, RemoteCommandV1};
use keyring::{Entry, Error as KeyringError};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::sync::Mutex;
use tauri::State;
use time::OffsetDateTime;
use url::Url;
use zeroize::Zeroizing;

const CREDENTIAL_SERVICE: &str = "ai.crowcompute.agent";
const SIGNING_SEED_ACCOUNT: &str = "device-signing-seed";
const ENCRYPTION_SECRET_ACCOUNT: &str = "device-encryption-secret";
const ACCESS_TOKEN_ACCOUNT: &str = "device-access-token";
const REFRESH_TOKEN_ACCOUNT: &str = "device-refresh-token";
const DEVICE_ID_ACCOUNT: &str = "device-id";
const CONTROLLER_NONCE_ACCOUNT: &str = "controller-nonce";
const PRODUCTION_API_ORIGIN: &str = "https://api.crowcompute.ai";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentStatus {
    protocol: &'static str,
    execution_boundary: &'static str,
    daemon: &'static str,
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteCommandResult {
    command_id: String,
    action: String,
    accepted: bool,
}

#[derive(Debug)]
enum DesktopError {
    Credential,
    Authorization,
    NoAuthorization,
    Browser,
    Network,
    RemoteCommand,
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
        }
    }
}

#[derive(Default)]
struct DesktopState {
    pending_authorization: Mutex<Option<DeviceAuthorizationSession>>,
    command_nonce: Mutex<()>,
}

#[tauri::command]
fn get_agent_status() -> AgentStatus {
    AgentStatus {
        protocol: HARNESS_PROTOCOL_V1,
        execution_boundary: "local_device",
        daemon: "stopped",
        active_run: None,
        device_authorized: credential_exists(REFRESH_TOKEN_ACCOUNT),
    }
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
async fn get_remote_state() -> Result<RemoteState, String> {
    let token = rotate_desktop_tokens()
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
    let devices = serde_json::from_value::<Vec<RemoteDevice>>(
        devices.get("devices").cloned().unwrap_or_else(|| json!([])),
    )
    .map_err(|_| DesktopError::Network.code().to_owned())?;
    let runs = serde_json::from_value::<Vec<RemoteRun>>(
        runs.get("runs").cloned().unwrap_or_else(|| json!([])),
    )
    .map_err(|_| DesktopError::Network.code().to_owned())?;
    Ok(RemoteState { devices, runs })
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
    let token = rotate_desktop_tokens()
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

fn load_or_create_identity() -> Result<DeviceIdentity, DesktopError> {
    let entry = Entry::new(CREDENTIAL_SERVICE, SIGNING_SEED_ACCOUNT)
        .map_err(|_| DesktopError::Credential)?;
    match entry.get_secret() {
        Ok(stored) => {
            let stored = Zeroizing::new(stored);
            let seed: [u8; 32] = stored
                .as_slice()
                .try_into()
                .map_err(|_| DesktopError::Credential)?;
            let seed = Zeroizing::new(seed);
            Ok(DeviceIdentity::from_seed(&seed))
        }
        Err(KeyringError::NoEntry) => {
            let identity = DeviceIdentity::generate();
            let seed = Zeroizing::new(identity.seed());
            entry
                .set_secret(seed.as_ref())
                .map_err(|_| DesktopError::Credential)?;
            Ok(identity)
        }
        Err(_) => Err(DesktopError::Credential),
    }
}

fn load_or_create_encryption_key() -> Result<DeviceEncryptionKey, DesktopError> {
    let entry = Entry::new(CREDENTIAL_SERVICE, ENCRYPTION_SECRET_ACCOUNT)
        .map_err(|_| DesktopError::Credential)?;
    match entry.get_secret() {
        Ok(stored) => {
            let stored = Zeroizing::new(stored);
            let secret: [u8; 32] = stored
                .as_slice()
                .try_into()
                .map_err(|_| DesktopError::Credential)?;
            Ok(DeviceEncryptionKey::from_secret(secret))
        }
        Err(KeyringError::NoEntry) => {
            let encryption_key = DeviceEncryptionKey::generate();
            let secret = encryption_key.secret_bytes();
            entry
                .set_secret(secret.as_ref())
                .map_err(|_| DesktopError::Credential)?;
            Ok(encryption_key)
        }
        Err(_) => Err(DesktopError::Credential),
    }
}

fn store_tokens(tokens: &DeviceTokens) -> Result<(), DesktopError> {
    store_password(ACCESS_TOKEN_ACCOUNT, tokens.access_token.as_str())?;
    store_password(REFRESH_TOKEN_ACCOUNT, tokens.refresh_token.as_str())?;
    store_password(DEVICE_ID_ACCOUNT, &tokens.device_id.to_string())
}

async fn rotate_desktop_tokens() -> Result<DeviceTokens, DesktopError> {
    let refresh_token = Zeroizing::new(load_password(REFRESH_TOKEN_ACCOUNT)?);
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
    if response.status() != StatusCode::OK && response.status() != StatusCode::ACCEPTED {
        return Err(DesktopError::RemoteCommand);
    }
    response
        .json::<T>()
        .await
        .map_err(|_| DesktopError::Network)
}

fn next_controller_nonce() -> Result<u64, DesktopError> {
    let previous = load_password(CONTROLLER_NONCE_ACCOUNT)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let next = previous.checked_add(1).ok_or(DesktopError::RemoteCommand)?;
    store_password(CONTROLLER_NONCE_ACCOUNT, &next.to_string())?;
    Ok(next)
}

fn load_password(account: &str) -> Result<String, DesktopError> {
    Entry::new(CREDENTIAL_SERVICE, account)
        .and_then(|entry| entry.get_password())
        .map_err(|_| DesktopError::Credential)
}

fn api_origin() -> String {
    std::env::var("CROW_API_ORIGIN").unwrap_or_else(|_| PRODUCTION_API_ORIGIN.into())
}

fn store_password(account: &str, value: &str) -> Result<(), DesktopError> {
    Entry::new(CREDENTIAL_SERVICE, account)
        .and_then(|entry| entry.set_password(value))
        .map_err(|_| DesktopError::Credential)
}

fn credential_exists(account: &str) -> bool {
    Entry::new(CREDENTIAL_SERVICE, account)
        .and_then(|entry| entry.get_password())
        .is_ok_and(|value| !value.is_empty())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(DesktopState::default())
        .invoke_handler(tauri::generate_handler![
            get_agent_status,
            begin_device_authorization,
            complete_device_authorization,
            get_remote_state,
            send_remote_command
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|error| {
            eprintln!("Crow Agent desktop runtime failed: {error}");
            std::process::exit(1);
        });
}
