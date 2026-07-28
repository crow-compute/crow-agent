use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Parser, Subcommand};
use crow_agent_core::{
    CandleV1, DatasetError, InstrumentV1, TlsProviderError, install_tls_crypto_provider,
    write_signed_dataset,
};
use crow_agent_core::{DATASET_MANIFEST_FILE, read_verified_dataset};
use crow_agent_protocol::DatasetManifestV1;
use ed25519_dalek::SigningKey;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

const INFO_ENDPOINT: &str = "https://api.hyperliquid.xyz/info";
const SYMBOLS: [&str; 3] = ["BTC", "ETH", "SOL"];
const CANDLE_INTERVAL_MILLIS: i64 = 900_000;
const FUNDING_INTERVAL_MILLIS: i64 = 3_600_000;
const MAX_CANDLES_PER_SYMBOL: i64 = 5_000;
const MAX_FUNDING_PAGES: usize = 32;

#[derive(Debug, Parser)]
#[command(
    name = "crow-dataset-publisher",
    about = "Build signed deterministic Crow historical datasets"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Publish {
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        dataset_id: Uuid,
        #[arg(long)]
        version: u32,
        #[arg(long)]
        start_time_ms: i64,
        #[arg(long)]
        end_time_ms: i64,
        #[arg(long)]
        signing_key_file: PathBuf,
    },
    PublicKey {
        #[arg(long)]
        signing_key_file: PathBuf,
    },
    Verify {
        #[arg(long)]
        dataset_directory: PathBuf,
        #[arg(long)]
        expected_public_key_file: PathBuf,
    },
}

#[derive(Debug, Error)]
enum PublisherError {
    #[error("invalid publication window")]
    Window,
    #[error("signing key must contain exactly 32 raw bytes")]
    SigningKey,
    #[error("Hyperliquid request failed")]
    Request(#[from] reqwest::Error),
    #[error("Hyperliquid returned HTTP {0}")]
    Http(StatusCode),
    #[error("Hyperliquid response was incomplete or inconsistent")]
    SourceData,
    #[error("fixed-point decimal is invalid")]
    Decimal,
    #[error("dataset packaging failed")]
    Dataset(#[from] DatasetError),
    #[error("filesystem operation failed")]
    Io(#[from] std::io::Error),
    #[error("dataset manifest JSON is invalid")]
    Json(#[from] serde_json::Error),
    #[error("TLS provider initialization failed")]
    TlsProvider(#[from] TlsProviderError),
}

#[derive(Debug, Serialize)]
struct InfoRequest<'a, T> {
    #[serde(rename = "type")]
    request_type: &'a str,
    #[serde(flatten)]
    body: T,
}

#[derive(Debug, Serialize)]
struct EmptyBody {}

#[derive(Debug, Serialize)]
struct CandleBody<'a> {
    req: CandleRequest<'a>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CandleRequest<'a> {
    coin: &'a str,
    interval: &'a str,
    start_time: i64,
    end_time: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FundingBody<'a> {
    coin: &'a str,
    start_time: i64,
    end_time: i64,
}

#[derive(Debug, Deserialize)]
struct MetaResponse {
    universe: Vec<MetaAsset>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetaAsset {
    name: String,
    sz_decimals: u8,
    max_leverage: u16,
    #[serde(default)]
    is_delisted: bool,
}

#[derive(Debug, Deserialize)]
struct CandleResponse {
    #[serde(rename = "t")]
    open_time_ms: i64,
    #[serde(rename = "T")]
    close_time_ms: i64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "i")]
    interval: String,
    #[serde(rename = "o")]
    open: String,
    #[serde(rename = "c")]
    close: String,
    #[serde(rename = "h")]
    high: String,
    #[serde(rename = "l")]
    low: String,
    #[serde(rename = "v")]
    volume: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FundingResponse {
    coin: String,
    funding_rate: String,
    time: i64,
}

#[derive(Debug)]
struct HyperliquidInfo {
    client: Client,
}

impl HyperliquidInfo {
    fn new() -> Result<Self, PublisherError> {
        Ok(Self {
            client: Client::builder()
                .user_agent("crow-dataset-publisher/0.1.0")
                .https_only(true)
                .build()?,
        })
    }

    async fn post<T: Serialize, R: DeserializeOwned>(&self, body: &T) -> Result<R, PublisherError> {
        let response = self.client.post(INFO_ENDPOINT).json(body).send().await?;
        if !response.status().is_success() {
            return Err(PublisherError::Http(response.status()));
        }
        Ok(response.json().await?)
    }

    async fn instruments(&self) -> Result<Vec<InstrumentV1>, PublisherError> {
        let response: MetaResponse = self
            .post(&InfoRequest {
                request_type: "meta",
                body: EmptyBody {},
            })
            .await?;
        let by_symbol = response
            .universe
            .into_iter()
            .map(|asset| (asset.name.clone(), asset))
            .collect::<BTreeMap<_, _>>();
        SYMBOLS
            .into_iter()
            .map(|symbol| {
                let asset = by_symbol.get(symbol).ok_or(PublisherError::SourceData)?;
                if asset.is_delisted {
                    return Err(PublisherError::SourceData);
                }
                Ok(InstrumentV1 {
                    symbol: symbol.into(),
                    size_decimals: asset.sz_decimals,
                    max_leverage: asset.max_leverage,
                    is_delisted: asset.is_delisted,
                })
            })
            .collect()
    }

    async fn candles(
        &self,
        symbol: &str,
        start_time_ms: i64,
        end_time_ms: i64,
    ) -> Result<Vec<CandleResponse>, PublisherError> {
        self.post(&InfoRequest {
            request_type: "candleSnapshot",
            body: CandleBody {
                req: CandleRequest {
                    coin: symbol,
                    interval: "15m",
                    start_time: start_time_ms,
                    end_time: end_time_ms - 1,
                },
            },
        })
        .await
    }

    async fn funding(
        &self,
        symbol: &str,
        start_time_ms: i64,
        end_time_ms: i64,
    ) -> Result<Vec<FundingResponse>, PublisherError> {
        let mut cursor = start_time_ms;
        let mut result = Vec::new();
        for _ in 0..MAX_FUNDING_PAGES {
            if cursor >= end_time_ms {
                break;
            }
            let page: Vec<FundingResponse> = self
                .post(&InfoRequest {
                    request_type: "fundingHistory",
                    body: FundingBody {
                        coin: symbol,
                        start_time: cursor,
                        end_time: end_time_ms - 1,
                    },
                })
                .await?;
            if page.is_empty() {
                break;
            }
            let last_time = page
                .last()
                .map(|funding| funding.time)
                .ok_or(PublisherError::SourceData)?;
            if last_time < cursor {
                return Err(PublisherError::SourceData);
            }
            result.extend(
                page.into_iter()
                    .filter(|funding| funding.time < end_time_ms),
            );
            cursor = last_time.checked_add(1).ok_or(PublisherError::SourceData)?;
        }
        if cursor < end_time_ms && result.len() >= MAX_FUNDING_PAGES * 500 {
            return Err(PublisherError::SourceData);
        }
        Ok(result)
    }
}

fn validate_window(start_time_ms: i64, end_time_ms: i64) -> Result<usize, PublisherError> {
    if start_time_ms < 0
        || end_time_ms <= start_time_ms
        || start_time_ms.rem_euclid(FUNDING_INTERVAL_MILLIS) != 0
        || end_time_ms.rem_euclid(FUNDING_INTERVAL_MILLIS) != 0
        || (end_time_ms - start_time_ms).rem_euclid(CANDLE_INTERVAL_MILLIS) != 0
    {
        return Err(PublisherError::Window);
    }
    let intervals = (end_time_ms - start_time_ms) / CANDLE_INTERVAL_MILLIS;
    if !(2..=MAX_CANDLES_PER_SYMBOL).contains(&intervals) {
        return Err(PublisherError::Window);
    }
    usize::try_from(intervals).map_err(|_| PublisherError::Window)
}

fn read_signing_key(path: &Path) -> Result<SigningKey, PublisherError> {
    let bytes = Zeroizing::new(fs::read(path)?);
    let seed: &[u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| PublisherError::SigningKey)?;
    Ok(SigningKey::from_bytes(seed))
}

fn parse_scaled(value: &str, scale: u32) -> Result<i64, PublisherError> {
    let (negative, unsigned) = value
        .strip_prefix('-')
        .map_or((false, value), |rest| (true, rest));
    if unsigned.is_empty() || unsigned.starts_with('+') {
        return Err(PublisherError::Decimal);
    }
    let mut parts = unsigned.split('.');
    let whole = parts.next().ok_or(PublisherError::Decimal)?;
    let fraction = parts.next().unwrap_or("");
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > usize::try_from(scale).map_err(|_| PublisherError::Decimal)?
    {
        return Err(PublisherError::Decimal);
    }
    let factor = 10_i128.checked_pow(scale).ok_or(PublisherError::Decimal)?;
    let whole = whole.parse::<i128>().map_err(|_| PublisherError::Decimal)?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<i128>()
            .map_err(|_| PublisherError::Decimal)?
            .checked_mul(
                10_i128
                    .checked_pow(
                        scale
                            .checked_sub(
                                u32::try_from(fraction.len())
                                    .map_err(|_| PublisherError::Decimal)?,
                            )
                            .ok_or(PublisherError::Decimal)?,
                    )
                    .ok_or(PublisherError::Decimal)?,
            )
            .ok_or(PublisherError::Decimal)?
    };
    let scaled = whole
        .checked_mul(factor)
        .and_then(|value| value.checked_add(fraction))
        .ok_or(PublisherError::Decimal)?;
    i64::try_from(if negative { -scaled } else { scaled }).map_err(|_| PublisherError::Decimal)
}

fn parse_scaled_rounded(value: &str, scale: u32) -> Result<i64, PublisherError> {
    let (negative, unsigned) = value
        .strip_prefix('-')
        .map_or((false, value), |rest| (true, rest));
    let mut parts = unsigned.split('.');
    let whole = parts.next().ok_or(PublisherError::Decimal)?;
    let fraction = parts.next().unwrap_or("");
    let scale = usize::try_from(scale).map_err(|_| PublisherError::Decimal)?;
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() <= scale
    {
        return parse_scaled(
            value,
            u32::try_from(scale).map_err(|_| PublisherError::Decimal)?,
        );
    }
    if fraction.len() > 18 {
        return Err(PublisherError::Decimal);
    }
    let retained = &fraction[..scale];
    let factor = 10_i128
        .checked_pow(u32::try_from(scale).map_err(|_| PublisherError::Decimal)?)
        .ok_or(PublisherError::Decimal)?;
    let whole = whole.parse::<i128>().map_err(|_| PublisherError::Decimal)?;
    let retained = if retained.is_empty() {
        0
    } else {
        retained
            .parse::<i128>()
            .map_err(|_| PublisherError::Decimal)?
    };
    let round_up = fraction
        .as_bytes()
        .get(scale)
        .is_some_and(|digit| *digit >= b'5');
    let scaled = whole
        .checked_mul(factor)
        .and_then(|value| value.checked_add(retained))
        .and_then(|value| value.checked_add(i128::from(round_up)))
        .ok_or(PublisherError::Decimal)?;
    i64::try_from(if negative { -scaled } else { scaled }).map_err(|_| PublisherError::Decimal)
}

fn normalize_symbol(
    symbol: &str,
    size_decimals: u8,
    expected_intervals: usize,
    start_time_ms: i64,
    candles: Vec<CandleResponse>,
    funding: Vec<FundingResponse>,
) -> Result<Vec<CandleV1>, PublisherError> {
    if candles.len() != expected_intervals {
        return Err(PublisherError::SourceData);
    }
    let expected_funding = expected_intervals
        .checked_mul(15)
        .and_then(|minutes| minutes.checked_div(60))
        .ok_or(PublisherError::SourceData)?;
    let mut funding_by_open = BTreeMap::new();
    for record in funding {
        let open_time = record.time.div_euclid(FUNDING_INTERVAL_MILLIS) * FUNDING_INTERVAL_MILLIS;
        if record.coin != symbol
            || open_time < start_time_ms
            || funding_by_open
                .insert(open_time, parse_scaled(&record.funding_rate, 12)?)
                .is_some()
        {
            return Err(PublisherError::SourceData);
        }
    }
    if funding_by_open.len() != expected_funding {
        return Err(PublisherError::SourceData);
    }
    let expected_funding_times = (0..expected_funding)
        .map(|index| {
            Ok::<i64, PublisherError>(
                start_time_ms
                    + i64::try_from(index).map_err(|_| PublisherError::SourceData)?
                        * FUNDING_INTERVAL_MILLIS,
            )
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    if funding_by_open.keys().copied().collect::<BTreeSet<_>>() != expected_funding_times {
        return Err(PublisherError::SourceData);
    }
    candles
        .into_iter()
        .enumerate()
        .map(|(index, candle)| {
            let open_time_ms = start_time_ms
                + i64::try_from(index).map_err(|_| PublisherError::SourceData)?
                    * CANDLE_INTERVAL_MILLIS;
            if candle.symbol != symbol
                || candle.interval != "15m"
                || candle.open_time_ms != open_time_ms
                || candle.close_time_ms != open_time_ms + CANDLE_INTERVAL_MILLIS - 1
            {
                return Err(PublisherError::SourceData);
            }
            Ok(CandleV1 {
                symbol: symbol.into(),
                open_time_ms,
                close_time_ms: candle.close_time_ms,
                open_micro_usdc: parse_scaled(&candle.open, 6)?,
                high_micro_usdc: parse_scaled(&candle.high, 6)?,
                low_micro_usdc: parse_scaled(&candle.low, 6)?,
                close_micro_usdc: parse_scaled(&candle.close, 6)?,
                volume_e8: parse_scaled_rounded(&candle.volume, 8)?,
                funding_rate_e12: funding_by_open.get(&open_time_ms).copied().unwrap_or(0),
                size_decimals,
            })
        })
        .collect()
}

async fn publish(
    output: &Path,
    dataset_id: Uuid,
    version: u32,
    start_time_ms: i64,
    end_time_ms: i64,
    signing_key_file: &Path,
) -> Result<(), PublisherError> {
    let expected_intervals = validate_window(start_time_ms, end_time_ms)?;
    if version == 0 {
        return Err(PublisherError::Window);
    }
    if output.exists() && fs::read_dir(output)?.next().is_some() {
        return Err(PublisherError::SourceData);
    }
    let signing_key = read_signing_key(signing_key_file)?;
    let info = HyperliquidInfo::new()?;
    let instruments = info.instruments().await?;
    let mut by_symbol = BTreeMap::new();
    for instrument in &instruments {
        let (candles, funding) = tokio::try_join!(
            info.candles(&instrument.symbol, start_time_ms, end_time_ms),
            info.funding(&instrument.symbol, start_time_ms, end_time_ms)
        )?;
        by_symbol.insert(
            instrument.symbol.clone(),
            normalize_symbol(
                &instrument.symbol,
                instrument.size_decimals,
                expected_intervals,
                start_time_ms,
                candles,
                funding,
            )?,
        );
    }
    let mut candles = Vec::with_capacity(expected_intervals * SYMBOLS.len());
    for index in 0..expected_intervals {
        for symbol in SYMBOLS {
            candles.push(
                by_symbol
                    .get(symbol)
                    .and_then(|series| series.get(index))
                    .cloned()
                    .ok_or(PublisherError::SourceData)?,
            );
        }
    }
    let package = write_signed_dataset(
        output,
        dataset_id,
        version,
        &candles,
        &instruments,
        &signing_key,
    )?;
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "dataset_id": package.manifest.dataset_id,
            "version": package.manifest.version,
            "starts_at": package.manifest.starts_at.to_string(),
            "ends_at": package.manifest.ends_at.to_string(),
            "rows": candles.len(),
            "package_sha256": package.manifest.package_sha256,
            "signer_public_key": package.manifest.signer_public_key,
        }))?
    );
    Ok(())
}

fn verify(dataset_directory: &Path, expected_public_key_file: &Path) -> Result<(), PublisherError> {
    let manifest = serde_json::from_slice::<DatasetManifestV1>(&fs::read(
        dataset_directory.join(DATASET_MANIFEST_FILE),
    )?)?;
    let expected_public_key = fs::read_to_string(expected_public_key_file)?;
    if manifest.signer_public_key != expected_public_key.trim() {
        return Err(PublisherError::SourceData);
    }
    let candles = read_verified_dataset(dataset_directory, &manifest)?;
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "dataset_id": manifest.dataset_id,
            "version": manifest.version,
            "starts_at": manifest.starts_at.to_string(),
            "ends_at": manifest.ends_at.to_string(),
            "rows": candles.len(),
            "package_sha256": manifest.package_sha256,
            "signer_public_key": manifest.signer_public_key,
            "verified": true,
        }))?
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), PublisherError> {
    install_tls_crypto_provider()?;
    match Cli::parse().command {
        Command::Publish {
            output,
            dataset_id,
            version,
            start_time_ms,
            end_time_ms,
            signing_key_file,
        } => {
            publish(
                &output,
                dataset_id,
                version,
                start_time_ms,
                end_time_ms,
                &signing_key_file,
            )
            .await
        }
        Command::PublicKey { signing_key_file } => {
            let signing_key = read_signing_key(&signing_key_file)?;
            println!(
                "{}",
                URL_SAFE_NO_PAD.encode(signing_key.verifying_key().as_bytes())
            );
            Ok(())
        }
        Command::Verify {
            dataset_directory,
            expected_public_key_file,
        } => verify(&dataset_directory, &expected_public_key_file),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candle(index: i64) -> CandleResponse {
        CandleResponse {
            open_time_ms: index * CANDLE_INTERVAL_MILLIS,
            close_time_ms: (index + 1) * CANDLE_INTERVAL_MILLIS - 1,
            symbol: "BTC".into(),
            interval: "15m".into(),
            open: "60000.0".into(),
            close: "60001.0".into(),
            high: "60002.0".into(),
            low: "59999.0".into(),
            volume: "1.25".into(),
        }
    }

    #[test]
    fn fixed_point_decimal_parser_is_exact() {
        assert_eq!(parse_scaled("63320.0", 6).ok(), Some(63_320_000_000));
        assert_eq!(parse_scaled("77.37412", 8).ok(), Some(7_737_412_000));
        assert_eq!(parse_scaled("0.0000125", 12).ok(), Some(12_500_000));
        assert_eq!(parse_scaled("-0.00000125", 12).ok(), Some(-1_250_000));
        assert_eq!(
            parse_scaled_rounded("1166084.0600000001", 8).ok(),
            Some(116_608_406_000_000)
        );
        assert_eq!(
            parse_scaled_rounded("1.000000005", 8).ok(),
            Some(100_000_001)
        );
        assert!(parse_scaled("1.0000001", 6).is_err());
        assert!(parse_scaled("1e-5", 12).is_err());
    }

    #[test]
    fn publication_window_is_hour_aligned_and_api_bounded() {
        assert_eq!(validate_window(0, 86_400_000).ok(), Some(96));
        assert!(validate_window(900_000, 86_400_000).is_err());
        assert!(validate_window(0, (MAX_CANDLES_PER_SYMBOL + 1) * CANDLE_INTERVAL_MILLIS).is_err());
    }

    #[test]
    fn normalization_requires_complete_funding_and_places_it_once_per_hour()
    -> Result<(), PublisherError> {
        let normalized = normalize_symbol(
            "BTC",
            5,
            4,
            0,
            (0..4).map(candle).collect(),
            vec![FundingResponse {
                coin: "BTC".into(),
                funding_rate: "0.0000125".into(),
                time: 6,
            }],
        )?;
        assert_eq!(normalized.len(), 4);
        assert_eq!(normalized[0].funding_rate_e12, 12_500_000);
        assert!(
            normalized[1..]
                .iter()
                .all(|candle| candle.funding_rate_e12 == 0)
        );
        assert!(
            normalize_symbol("BTC", 5, 4, 0, (0..4).map(candle).collect(), Vec::new(),).is_err()
        );
        Ok(())
    }
}
