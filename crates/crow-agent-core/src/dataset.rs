use crate::backtest::CandleV1;
use arrow_array::{
    Array as _, ArrayRef, BooleanArray, Int64Array, RecordBatch, StringArray, UInt8Array,
    UInt16Array,
};
use arrow_schema::{DataType, Field, Schema};
use crow_agent_protocol::{DatasetFileV1, DatasetManifestV1, canonical_json};
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

pub const DATASET_MANIFEST_FILE: &str = "dataset-manifest-v1.json";
const CANDLE_FILE: &str = "candles.parquet";
const INSTRUMENT_FILE: &str = "instruments.parquet";
const CANDLE_INTERVAL_MILLIS: i64 = 900_000;
const FUNDING_INTERVAL_MILLIS: i64 = 3_600_000;

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
    #[error("dataset instruments are invalid")]
    Instruments,
    #[error("dataset column is invalid")]
    Column,
    #[error("dataset timestamp is invalid")]
    Timestamp(#[from] time::error::ComponentRange),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatasetPackage {
    pub candle_path: PathBuf,
    pub instrument_path: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: DatasetManifestV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstrumentV1 {
    pub symbol: String,
    pub size_decimals: u8,
    pub max_leverage: u16,
    pub is_delisted: bool,
}

pub fn write_signed_dataset(
    directory: &Path,
    dataset_id: Uuid,
    version: u32,
    candles: &[CandleV1],
    instruments: &[InstrumentV1],
    signer: &SigningKey,
) -> Result<DatasetPackage, DatasetError> {
    validate_candles(candles)?;
    validate_instruments(instruments, candles)?;
    fs::create_dir_all(directory)?;
    let candle_path = directory.join(CANDLE_FILE);
    let instrument_path = directory.join(INSTRUMENT_FILE);
    write_parquet(&candle_path, &candle_batch(candles)?)?;
    write_parquet(&instrument_path, &instrument_batch(instruments)?)?;

    let files = [CANDLE_FILE, INSTRUMENT_FILE]
        .into_iter()
        .map(|name| dataset_file(directory, name))
        .collect::<Result<Vec<_>, _>>()?;
    let package_digest = hex::encode(Sha256::digest(canonical_json(&files)?));
    let starts_at = timestamp_millis(candles.first().ok_or(DatasetError::Candles)?.open_time_ms)?;
    let ends_at = timestamp_millis(candles.last().ok_or(DatasetError::Candles)?.close_time_ms)?;
    let manifest = DatasetManifestV1::sign(
        signer,
        dataset_id,
        version,
        starts_at,
        ends_at,
        files,
        package_digest,
    )?;
    let manifest_path = directory.join(DATASET_MANIFEST_FILE);
    fs::write(&manifest_path, canonical_json(&manifest)?)?;
    Ok(DatasetPackage {
        candle_path,
        instrument_path,
        manifest_path,
        manifest,
    })
}

fn write_parquet(path: &Path, batch: &RecordBatch) -> Result<(), DatasetError> {
    let properties = WriterProperties::builder()
        .set_writer_version(WriterVersion::PARQUET_2_0)
        .set_created_by("crow-agent-dataset-v1".into())
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .set_dictionary_enabled(false)
        .set_statistics_enabled(EnabledStatistics::None)
        .set_max_row_group_row_count(Some(65_536))
        .build();
    let mut writer = ArrowWriter::try_new(File::create(path)?, batch.schema(), Some(properties))?;
    writer.write(batch)?;
    writer.close()?;
    Ok(())
}

fn dataset_file(directory: &Path, name: &str) -> Result<DatasetFileV1, DatasetError> {
    let bytes = fs::read(directory.join(name))?;
    Ok(DatasetFileV1 {
        path: name.into(),
        sha256: hex::encode(Sha256::digest(&bytes)),
        bytes: u64::try_from(bytes.len()).map_err(|_| DatasetError::Candles)?,
    })
}

pub fn read_verified_dataset(
    directory: &Path,
    manifest: &DatasetManifestV1,
) -> Result<Vec<CandleV1>, DatasetError> {
    manifest.verify()?;
    if manifest.files.len() != 2
        || manifest.files[0].path != CANDLE_FILE
        || manifest.files[1].path != INSTRUMENT_FILE
    {
        return Err(DatasetError::Candles);
    }
    for file in &manifest.files {
        let bytes = fs::read(directory.join(&file.path))?;
        if hex::encode(Sha256::digest(&bytes)) != file.sha256
            || u64::try_from(bytes.len()).map_err(|_| DatasetError::Candles)? != file.bytes
        {
            return Err(DatasetError::Candles);
        }
    }
    if hex::encode(Sha256::digest(canonical_json(&manifest.files)?)) != manifest.package_sha256 {
        return Err(DatasetError::Candles);
    }
    let mut reader =
        ParquetRecordBatchReaderBuilder::try_new(File::open(directory.join(CANDLE_FILE))?)?
            .with_batch_size(65_536)
            .build()?;
    let mut candles = Vec::new();
    for batch in &mut reader {
        candles.extend(candles_from_batch(&batch?)?);
    }
    validate_candles(&candles)?;
    let mut reader =
        ParquetRecordBatchReaderBuilder::try_new(File::open(directory.join(INSTRUMENT_FILE))?)?
            .with_batch_size(64)
            .build()?;
    let mut instruments = Vec::new();
    for batch in &mut reader {
        instruments.extend(instruments_from_batch(&batch?)?);
    }
    validate_instruments(&instruments, &candles)?;
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
        Field::new("funding_rate_e12", DataType::Int64, false),
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
            candles.iter().map(|candle| candle.funding_rate_e12),
        )),
        Arc::new(UInt8Array::from_iter_values(
            candles.iter().map(|candle| candle.size_decimals),
        )),
    ];
    Ok(RecordBatch::try_new(candle_schema(), columns)?)
}

fn instrument_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("symbol", DataType::Utf8, false),
        Field::new("size_decimals", DataType::UInt8, false),
        Field::new("max_leverage", DataType::UInt16, false),
        Field::new("is_delisted", DataType::Boolean, false),
    ]))
}

fn instrument_batch(instruments: &[InstrumentV1]) -> Result<RecordBatch, DatasetError> {
    let columns: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from_iter_values(
            instruments
                .iter()
                .map(|instrument| instrument.symbol.as_str()),
        )),
        Arc::new(UInt8Array::from_iter_values(
            instruments
                .iter()
                .map(|instrument| instrument.size_decimals),
        )),
        Arc::new(UInt16Array::from_iter_values(
            instruments.iter().map(|instrument| instrument.max_leverage),
        )),
        Arc::new(
            instruments
                .iter()
                .map(|instrument| Some(instrument.is_delisted))
                .collect::<BooleanArray>(),
        ),
    ];
    Ok(RecordBatch::try_new(instrument_schema(), columns)?)
}

fn instruments_from_batch(batch: &RecordBatch) -> Result<Vec<InstrumentV1>, DatasetError> {
    if batch.schema() != instrument_schema() {
        return Err(DatasetError::Column);
    }
    let symbol = batch
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or(DatasetError::Column)?;
    let size_decimals = batch
        .column(1)
        .as_any()
        .downcast_ref::<UInt8Array>()
        .ok_or(DatasetError::Column)?;
    let max_leverage = batch
        .column(2)
        .as_any()
        .downcast_ref::<UInt16Array>()
        .ok_or(DatasetError::Column)?;
    let is_delisted = batch
        .column(3)
        .as_any()
        .downcast_ref::<BooleanArray>()
        .ok_or(DatasetError::Column)?;
    Ok((0..batch.num_rows())
        .map(|row| InstrumentV1 {
            symbol: symbol.value(row).into(),
            size_decimals: size_decimals.value(row),
            max_leverage: max_leverage.value(row),
            is_delisted: is_delisted.value(row),
        })
        .collect())
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
            funding_rate_e12: integers[7].value(row),
            size_decimals: size_decimals.value(row),
        })
        .collect())
}

fn validate_candles(candles: &[CandleV1]) -> Result<(), DatasetError> {
    if candles.is_empty() || !candles.len().is_multiple_of(3) {
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
            || (candle.funding_rate_e12 != 0
                && candle.open_time_ms.rem_euclid(FUNDING_INTERVAL_MILLIS) != 0)
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
    for interval in candles.chunks_exact(3) {
        if interval[0].symbol != "BTC"
            || interval[1].symbol != "ETH"
            || interval[2].symbol != "SOL"
            || interval[0].open_time_ms != interval[1].open_time_ms
            || interval[0].open_time_ms != interval[2].open_time_ms
        {
            return Err(DatasetError::Candles);
        }
    }
    Ok(())
}

fn validate_instruments(
    instruments: &[InstrumentV1],
    candles: &[CandleV1],
) -> Result<(), DatasetError> {
    if instruments.len() != 3
        || instruments
            .iter()
            .map(|instrument| instrument.symbol.as_str())
            .ne(["BTC", "ETH", "SOL"])
        || instruments.iter().any(|instrument| {
            instrument.size_decimals > 8 || instrument.max_leverage == 0 || instrument.is_delisted
        })
    {
        return Err(DatasetError::Instruments);
    }
    for instrument in instruments {
        if candles
            .iter()
            .filter(|candle| candle.symbol == instrument.symbol)
            .any(|candle| candle.size_decimals != instrument.size_decimals)
        {
            return Err(DatasetError::Instruments);
        }
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
                    funding_rate_e12: if interval == 0 { 1_000_000 } else { 0 },
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

    fn instruments() -> Vec<InstrumentV1> {
        vec![
            InstrumentV1 {
                symbol: "BTC".into(),
                size_decimals: 5,
                max_leverage: 40,
                is_delisted: false,
            },
            InstrumentV1 {
                symbol: "ETH".into(),
                size_decimals: 4,
                max_leverage: 25,
                is_delisted: false,
            },
            InstrumentV1 {
                symbol: "SOL".into(),
                size_decimals: 2,
                max_leverage: 20,
                is_delisted: false,
            },
        ]
    }

    #[test]
    fn signed_zstd_parquet_round_trip_is_deterministic() -> Result<(), DatasetError> {
        let directory = tempdir()?;
        let signer = SigningKey::from_bytes(&[9_u8; 32]);
        let package = write_signed_dataset(
            directory.path(),
            Uuid::from_u128(7),
            1,
            &candles(),
            &instruments(),
            &signer,
        )?;
        package.manifest.verify()?;
        assert_eq!(
            read_verified_dataset(directory.path(), &package.manifest)?,
            candles()
        );
        let first_hash = package.manifest.package_sha256;
        assert_eq!(
            first_hash, "afc8195514729823fdb3ba1d372cdcfb8c4721baa0f6d4291c7dd3ff3c07d207",
            "signed dataset bytes changed; update only after reviewing the package format"
        );
        let second = write_signed_dataset(
            directory.path(),
            Uuid::from_u128(7),
            1,
            &candles(),
            &instruments(),
            &signer,
        )?;
        assert_eq!(first_hash, second.manifest.package_sha256);
        Ok(())
    }

    #[test]
    fn instrument_metadata_is_covered_by_the_package_digest() -> Result<(), DatasetError> {
        let directory = tempdir()?;
        let signer = SigningKey::from_bytes(&[9_u8; 32]);
        let package = write_signed_dataset(
            directory.path(),
            Uuid::from_u128(7),
            1,
            &candles(),
            &instruments(),
            &signer,
        )?;
        fs::write(&package.instrument_path, b"tampered")?;
        assert!(read_verified_dataset(directory.path(), &package.manifest).is_err());
        Ok(())
    }
}
