use crow_agent_protocol::{DatasetFileV1, DatasetManifestV1, DeviceIdentity};
use time::macros::datetime;
use uuid::Uuid;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let identity = DeviceIdentity::from_seed(&[9_u8; 32]);
    let manifest = DatasetManifestV1::sign(
        identity.signing_key(),
        Uuid::from_u128(6),
        1,
        datetime!(2026-07-01 00:00 UTC),
        datetime!(2026-07-01 00:30 UTC),
        vec![DatasetFileV1 {
            path: "candles.parquet".into(),
            sha256: "a".repeat(64),
            bytes: 4_096,
        }],
        "a".repeat(64),
    )?;
    println!("{}", serde_json::to_string(&manifest)?);
    Ok(())
}
