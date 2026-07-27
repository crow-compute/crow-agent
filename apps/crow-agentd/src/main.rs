use clap::{Parser, Subcommand};
use crow_agent_protocol::{ArenaManifestV1, DeviceIdentity, HARNESS_PROTOCOL_V1};
use futures_util::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use thiserror::Error;
use time::OffsetDateTime;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

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
    /// Print only the public device identity derived from systemd credentials.
    DevicePublic,
    /// Maintain the outbound Crow control connection.
    Run { config: PathBuf },
}

#[derive(Debug, Deserialize)]
struct DaemonConfig {
    device_id: Uuid,
    relay_url: String,
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
    timestamp: OffsetDateTime,
    payload: Value,
}

#[derive(Debug, Error)]
enum DaemonError {
    #[error("configuration or credential file is unavailable")]
    Io(#[from] std::io::Error),
    #[error("configuration is invalid")]
    Json(#[from] serde_json::Error),
    #[error("device signing credential must be exactly 32 raw bytes")]
    DeviceCredential,
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
}

#[tokio::main]
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
            let manifest = serde_json::from_slice::<ArenaManifestV1>(&fs::read(path)?)?;
            manifest.validate().map_err(|_| DaemonError::Manifest)?;
            println!("valid {} {}", manifest.arena_id, manifest.manifest_version);
        }
        Command::DevicePublic => {
            let identity = load_identity()?;
            println!("{}", identity.public_key());
        }
        Command::Run { config } => {
            let config = serde_json::from_slice::<DaemonConfig>(&fs::read(config)?)?;
            let identity = load_identity()?;
            run_relay(&config, &identity).await?;
        }
    }
    Ok(())
}

fn load_identity() -> Result<DeviceIdentity, DaemonError> {
    let directory =
        std::env::var_os("CREDENTIALS_DIRECTORY").ok_or(DaemonError::CredentialDirectory)?;
    let seed = Zeroizing::new(fs::read(Path::new(&directory).join("device-signing-seed"))?);
    let seed: [u8; 32] = seed
        .as_slice()
        .try_into()
        .map_err(|_| DaemonError::DeviceCredential)?;
    Ok(DeviceIdentity::from_seed(&seed))
}

async fn run_relay(config: &DaemonConfig, identity: &DeviceIdentity) -> Result<(), DaemonError> {
    let url = Url::parse(&config.relay_url).map_err(|_| DaemonError::RelayUrl)?;
    if url.scheme() != "wss" {
        return Err(DaemonError::RelayUrl);
    }
    let mut backoff = Duration::from_secs(1);
    loop {
        match relay_session(config, identity).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                warn!(error = %error, "relay session ended; no new decisions are permitted");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

async fn relay_session(
    config: &DaemonConfig,
    identity: &DeviceIdentity,
) -> Result<(), DaemonError> {
    let (mut connection, _) = connect_async(&config.relay_url).await?;
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
            "signature": identity.sign_bytes(nonce.as_bytes())
        }),
    };
    connection
        .send(Message::Text(serde_json::to_string(&auth)?.into()))
        .await?;
    info!(device_id = %config.device_id, "outbound relay authentication sent");
    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let envelope = OutboundEnvelope {
                    protocol: HARNESS_PROTOCOL_V1,
                    kind: "heartbeat",
                    id: Uuid::new_v4().to_string(),
                    timestamp: OffsetDateTime::now_utc(),
                    payload: json!({"device_id": config.device_id, "active_run": null}),
                };
                connection.send(Message::Text(serde_json::to_string(&envelope)?.into())).await?;
            }
            message = connection.next() => {
                match message {
                    Some(Ok(Message::Text(raw))) => {
                        let envelope = serde_json::from_str::<WireEnvelope>(&raw)?;
                        if envelope.protocol != HARNESS_PROTOCOL_V1 {
                            return Err(DaemonError::Challenge);
                        }
                        if envelope.kind == "shutdown" {
                            info!("relay requested shutdown");
                            return Ok(());
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

fn decode_envelope(message: Message) -> Result<WireEnvelope, DaemonError> {
    match message {
        Message::Text(raw) => serde_json::from_str(&raw).map_err(DaemonError::from),
        _ => Err(DaemonError::Challenge),
    }
}
