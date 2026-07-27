use crow_agent_protocol::{DeviceIdentity, RunEventEnvelopeV1};
use serde_json::json;
use time::macros::datetime;
use uuid::Uuid;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let identity = DeviceIdentity::from_seed(&[7_u8; 32]);
    let event = RunEventEnvelopeV1::sign(
        identity.signing_key(),
        Uuid::from_u128(1),
        Uuid::from_u128(2),
        Some(Uuid::from_u128(3)),
        1,
        "0".repeat(64),
        "cycle_started".into(),
        datetime!(2026-07-01 00:00 UTC),
        json!({"symbol": "BTC", "cycle": 1}),
    )?;
    println!("{}", serde_json::to_string(&event)?);
    Ok(())
}
