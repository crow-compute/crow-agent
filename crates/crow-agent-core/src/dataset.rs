use crate::backtest::CandleV1;
use arrow_array::{Array as _, ArrayRef, Int64Array, RecordBatch, StringArray, UInt8Array};
use arrow_schema::{DataType, Field, Schema};
use crow_agent_protocol::{DatasetFileV1, DatasetManifestV1};
use ed25519_dalek::SigningKey;
use parquet::{
    arrow::{ArrowWriter, arrow_reader::ParquetRecordBatchReaderBuilder},
    basic::{Compression, ZstdLevel},
    file::properties::{EnabledStatistics, WriterProperties, WriterVersion},
};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

const CANDLE_FILE: &str = "candles.parquet";
const CANDLE_INTERVAL_MILLIS: i64 = 900_000;

#[derive(Debug, Error)]
pub enum DatasetError {
    #[error("dataset filesystem operation failed")]
    Io(#[from] std::io::Error),
    #[error("dataset Arrow encoding failed")]
    Arrow(#[from] arrow_schema::ArrowError),
    #[error("dataset Parquet encoding failed")]
    Parquet(#[from] parquet::errors::ParquetError),
    #[error("dataset protocol validation failed")]
    Protocol(#[from] crow_agent_protocol::ProtocolError),
    #[error("dataset candles are invalid")]
    Candles,
    #[error("dataset column is invalid")]
    Column,
    #[error("dataset timestamp is invalid")]
    Timestamp(#[from] time::error::ComponentRange),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatasetPackage {
    pub parquet_path: PathBuf,
    pub manifest: DatasetManifestV1,
}

pub fn write_signed_dataset(
    directory: &Path,
    dataset_id: Uuid,
    version: u32,
    candles: &[CandleV1],
    signer: &SigningKey,
) -> Result<DatasetPackage, DatasetError> {
    validate_candles(candles)?;
    fs::create_dir_all(directory)?;
    let parquet_path = directory.join(CANDLE_FILE);
    let batch = candle_batch(candles)?;
    let properties = WriterProperties::builder()
        .set_writer_version(WriterVersion::PARQUET_2_0)
        .set_created_by("crow-agent-dataset-v1".into())
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .set_dictionary_enabled(false)
        .set_statistics_enabled(EnabledStatistics::None)
        .set_max_row_group_row_count(Some(65_536))
        .build();
    let mut writer = ArrowWriter::try_new(
        File::create(&parquet_path)?,
        batch.schema(),
        Some(properties),
    )?;
    writer.write(&batch)?;
    writer.close()?;

    let bytes = fs::read(&parquet_path)?;
    let digest = hex::encode(Sha256::digest(&bytes));
    let starts_at = timestamp_millis(candles.first().ok_or(DatasetError::Candles)?.open_time_ms)?;
    let ends_at = timestamp_millis(candles.last().ok_or(DatasetError::Candles)?.close_time_ms)?;
    let manifest = DatasetManifestV1::sign(
        signer,
        dataset_id,
        version,
        starts_at,
        ends_at,
        vec![DatasetFileV1 {
            path: CANDLE_FILE.into(),
            sha256: digest.clone(),
            bytes: u64::try_from(bytes.len()).map_err(|_| DatasetError::Candles)?,
        }],
        digest,
    )?;
    Ok(DatasetPackage {
        parquet_path,
        manifest,
    })
}

pub fn read_verified_dataset(
    directory: &Path,
    manifest: &DatasetManifestV1,
) -> Result<Vec<CandleV1>, DatasetError> {
    manifest.verify()?;
    if manifest.files.len() != 1 || manifest.files[0].path != CANDLE_FILE {
        return Err(DatasetError::Candles);
    }
    let path = directory.join(CANDLE_FILE);
    let bytes = fs::read(&path)?;
    let digest = hex::encode(Sha256::digest(&bytes));
    if digest != manifest.files[0].sha256
        || digest != manifest.package_sha256
        || u64::try_from(bytes.len()).map_err(|_| DatasetError::Candles)? != manifest.files[0].bytes
    {
        return Err(DatasetError::Candles);
    }
    let mut reader = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?
        .with_batch_size(65_536)
        .build()?;
    let mut candles = Vec::new();
    for batch in &mut reader {
        candles.extend(candles_from_batch(&batch?)?);
    }
    validate_candles(&candles)?;
    Ok(candles)
}

fn candle_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("symbol", DataType::Utf8, false),
        Field::new("open_time_ms", DataType::Int64, false),
        Field::new("close_time_ms", DataType::Int64, false),
        Field::new("open_micro_usdc", DataType::Int64, false),
        Field::new("high_micro_usdc", DataType::Int64, false),
        Field::new("low_micro_usdc", DataType::Int64, false),
        Field::new("close_micro_usdc", DataType::Int64, false),
        Field::new("volume_e8", DataType::Int64, false),
        Field::new("funding_micros_per_usdc", DataType::Int64, false),
        Field::new("size_decimals", DataType::UInt8, false),
    ]))
}

fn candle_batch(candles: &[CandleV1]) -> Result<RecordBatch, DatasetError> {
    let columns: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from_iter_values(
            candles.iter().map(|candle| candle.symbol.as_str()),
        )),
        Arc::new(Int64Array::from_iter_values(
            candles.iter().map(|candle| candle.open_time_ms),
        )),
        Arc::new(Int64Array::from_iter_values(
            candles.iter().map(|candle| candle.close_time_ms),
        )),
        Arc::new(Int64Array::from_iter_values(
            candles.iter().map(|candle| candle.open_micro_usdc),
        )),
        Arc::new(Int64Array::from_iter_values(
            candles.iter().map(|candle| candle.high_micro_usdc),
        )),
        Arc::new(Int64Array::from_iter_values(
            candles.iter().map(|candle| candle.low_micro_usdc),
        )),
        Arc::new(Int64Array::from_iter_values(
            candles.iter().map(|candle| candle.close_micro_usdc),
        )),
        Arc::new(Int64Array::from_iter_values(
            candles.iter().map(|candle| candle.volume_e8),
        )),
        Arc::new(Int64Array::from_iter_values(
            candles.iter().map(|candle| candle.funding_micros_per_usdc),
        )),
        Arc::new(UInt8Array::from_iter_values(
            candles.iter().map(|candle| candle.size_decimals),
        )),
    ];
    Ok(RecordBatch::try_new(candle_schema(), columns)?)
}

fn candles_from_batch(batch: &RecordBatch) -> Result<Vec<CandleV1>, DatasetError> {
    if batch.schema() != candle_schema() {
        return Err(DatasetError::Column);
    }
    let symbol = batch
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or(DatasetError::Column)?;
    let integers = (1..9)
        .map(|index| {
            batch
                .column(index)
                .as_any()
                .downcast_ref::<Int64Array>()
                .ok_or(DatasetError::Column)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let size_decimals = batch
        .column(9)
        .as_any()
        .downcast_ref::<UInt8Array>()
        .ok_or(DatasetError::Column)?;
    Ok((0..batch.num_rows())
        .map(|row| CandleV1 {
            symbol: symbol.value(row).into(),
            open_time_ms: integers[0].value(row),
            close_time_ms: integers[1].value(row),
            open_micro_usdc: integers[2].value(row),
            high_micro_usdc: integers[3].value(row),
            low_micro_usdc: integers[4].value(row),
            close_micro_usdc: integers[5].value(row),
            volume_e8: integers[6].value(row),
            funding_micros_per_usdc: integers[7].value(row),
            size_decimals: size_decimals.value(row),
        })
        .collect())
}

fn validate_candles(candles: &[CandleV1]) -> Result<(), DatasetError> {
    if candles.is_empty() {
        return Err(DatasetError::Candles);
    }
    let symbol_order = BTreeMap::from([("BTC", 0_u8), ("ETH", 1), ("SOL", 2)]);
    let mut previous_key: Option<(i64, u8)> = None;
    let mut last_open = BTreeMap::<&str, i64>::new();
    let mut instrument_decimals = BTreeMap::<&str, u8>::new();
    for candle in candles {
        let order = *symbol_order
            .get(candle.symbol.as_str())
            .ok_or(DatasetError::Candles)?;
        let key = (candle.open_time_ms, order);
        if previous_key.is_some_and(|previous| key <= previous)
            || candle.close_time_ms != candle.open_time_ms + CANDLE_INTERVAL_MILLIS - 1
            || candle.open_micro_usdc <= 0
            || candle.high_micro_usdc < candle.open_micro_usdc
            || candle.high_micro_usdc < candle.close_micro_usdc
            || candle.low_micro_usdc > candle.open_micro_usdc
            || candle.low_micro_usdc > candle.close_micro_usdc
            || candle.volume_e8 < 0
            || candle.size_decimals > 8
        {
            return Err(DatasetError::Candles);
        }
        if let Some(previous) = last_open.insert(candle.symbol.as_str(), candle.open_time_ms)
            && candle.open_time_ms != previous + CANDLE_INTERVAL_MILLIS
        {
            return Err(DatasetError::Candles);
        }
        if instrument_decimals
            .insert(candle.symbol.as_str(), candle.size_decimals)
            .is_some_and(|previous| previous != candle.size_decimals)
        {
            return Err(DatasetError::Candles);
        }
        previous_key = Some(key);
    }
    if last_open.len() != 3 {
        return Err(DatasetError::Candles);
    }
    Ok(())
}

fn timestamp_millis(value: i64) -> Result<OffsetDateTime, DatasetError> {
    Ok(OffsetDateTime::from_unix_timestamp_nanos(
        i128::from(value) * 1_000_000,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn candles() -> Vec<CandleV1> {
        let mut candles = Vec::new();
        for interval in 0..2 {
            for (symbol, price) in [
                ("BTC", 60_000_000_000),
                ("ETH", 3_000_000_000),
                ("SOL", 150_000_000),
            ] {
                let open_time_ms = interval * CANDLE_INTERVAL_MILLIS;
                candles.push(CandleV1 {
                    symbol: symbol.into(),
                    open_time_ms,
                    close_time_ms: open_time_ms + CANDLE_INTERVAL_MILLIS - 1,
                    open_micro_usdc: price,
                    high_micro_usdc: price + 10,
                    low_micro_usdc: price - 10,
                    close_micro_usdc: price,
                    volume_e8: 100,
                    funding_micros_per_usdc: 1,
                    size_decimals: match symbol {
                        "BTC" => 5,
                        "ETH" => 4,
                        _ => 2,
                    },
                });
            }
        }
        candles
    }

    #[test]
    fn signed_zstd_parquet_round_trip_is_deterministic() -> Result<(), DatasetError> {
        let directory = tempdir()?;
        let signer = SigningKey::from_bytes(&[9_u8; 32]);
        let package =
            write_signed_dataset(directory.path(), Uuid::from_u128(7), 1, &candles(), &signer)?;
        package.manifest.verify()?;
        assert_eq!(
            read_verified_dataset(directory.path(), &package.manifest)?,
            candles()
        );
        let first_hash = package.manifest.package_sha256;
        assert_eq!(
            first_hash, "5121842cd832614ffefefffe3fb00ee186a4a48b3c661a0d531513082cbcbbd0",
            "signed dataset bytes changed; update only after reviewing the package format"
        );
        let second =
            write_signed_dataset(directory.path(), Uuid::from_u128(7), 1, &candles(), &signer)?;
        assert_eq!(first_hash, second.manifest.package_sha256);
        Ok(())
    }
}
