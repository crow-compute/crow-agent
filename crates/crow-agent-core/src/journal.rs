use crate::crypto::{BundleCiphertext, CryptoError, decrypt_bundle, encrypt_bundle};
use crow_agent_protocol::RunEventEnvelopeV1;
use rusqlite::{Connection, OptionalExtension as _, params};
use serde_json::Value;
use std::path::Path;
use thiserror::Error;
use zeroize::Zeroizing;

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("journal database failed")]
    Database(#[from] rusqlite::Error),
    #[error("journal encryption failed")]
    Crypto(#[from] CryptoError),
    #[error("journal payload is invalid")]
    Payload(#[from] serde_json::Error),
    #[error("event signature or hash is invalid")]
    Event,
    #[error("event sequence or previous hash is invalid")]
    Sequence,
}

#[derive(Debug)]
pub struct EncryptedJournal {
    connection: Connection,
    key: Zeroizing<[u8; 32]>,
}

impl EncryptedJournal {
    pub fn open(path: &Path, key: [u8; 32]) -> Result<Self, JournalError> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS run_events (
                run_id TEXT NOT NULL,
                sequence INTEGER NOT NULL,
                event_sha256 TEXT NOT NULL UNIQUE,
                previous_event_sha256 TEXT NOT NULL,
                public_envelope TEXT NOT NULL,
                private_nonce TEXT NOT NULL,
                private_ciphertext TEXT NOT NULL,
                PRIMARY KEY(run_id, sequence)
             );
             CREATE TABLE IF NOT EXISTS local_secrets (
                name TEXT PRIMARY KEY,
                nonce TEXT NOT NULL,
                ciphertext TEXT NOT NULL,
                updated_at INTEGER NOT NULL DEFAULT (unixepoch())
             );",
        )?;
        let has_server_receipt = connection
            .prepare("PRAGMA table_info(run_events)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|column| column == "server_receipt");
        if !has_server_receipt {
            connection.execute("ALTER TABLE run_events ADD COLUMN server_receipt TEXT", [])?;
        }
        Ok(Self {
            connection,
            key: Zeroizing::new(key),
        })
    }

    pub fn append(
        &mut self,
        event: &RunEventEnvelopeV1,
        private_payload: &Value,
    ) -> Result<(), JournalError> {
        event.verify().map_err(|_| JournalError::Event)?;
        let previous: Option<(i64, String)> = self
            .connection
            .query_row(
                "SELECT sequence,event_sha256 FROM run_events
                 WHERE run_id=?1 ORDER BY sequence DESC LIMIT 1",
                [event.run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match previous {
            None if event.sequence != 1 || event.previous_event_sha256 != "0".repeat(64) => {
                return Err(JournalError::Sequence);
            }
            Some((sequence, hash))
                if event.sequence != u64::try_from(sequence).unwrap_or(u64::MAX) + 1
                    || event.previous_event_sha256 != hash =>
            {
                return Err(JournalError::Sequence);
            }
            _ => {}
        }
        let aad = event.event_sha256.as_bytes();
        let encrypted = encrypt_bundle(&self.key, &serde_json::to_vec(private_payload)?, aad)?;
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO run_events(
                run_id,sequence,event_sha256,previous_event_sha256,public_envelope,
                private_nonce,private_ciphertext
             ) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                event.run_id.to_string(),
                i64::try_from(event.sequence).map_err(|_| JournalError::Sequence)?,
                event.event_sha256,
                event.previous_event_sha256,
                serde_json::to_string(event)?,
                encrypted.nonce,
                encrypted.ciphertext,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn latest_event_state(
        &self,
        run_id: uuid::Uuid,
    ) -> Result<Option<(u64, String)>, JournalError> {
        let row: Option<(i64, String)> = self
            .connection
            .query_row(
                "SELECT sequence,event_sha256 FROM run_events
                 WHERE run_id=?1 ORDER BY sequence DESC LIMIT 1",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        row.map(|(sequence, hash)| {
            u64::try_from(sequence)
                .map(|sequence| (sequence, hash))
                .map_err(|_| JournalError::Sequence)
        })
        .transpose()
    }

    pub fn event_count(&self, run_id: uuid::Uuid, event_type: &str) -> Result<u64, JournalError> {
        let count = self.connection.query_row(
            "SELECT COUNT(*) FROM run_events
             WHERE run_id=?1
               AND json_extract(public_envelope,'$.event_type')=?2",
            params![run_id.to_string(), event_type],
            |row| row.get::<_, i64>(0),
        )?;
        u64::try_from(count).map_err(|_| JournalError::Sequence)
    }

    pub fn pending_events(
        &self,
        run_id: uuid::Uuid,
    ) -> Result<Vec<RunEventEnvelopeV1>, JournalError> {
        let mut statement = self.connection.prepare(
            "SELECT public_envelope FROM run_events
             WHERE run_id=?1 AND server_receipt IS NULL ORDER BY sequence",
        )?;
        let rows = statement.query_map([run_id.to_string()], |row| row.get::<_, String>(0))?;
        let mut events = Vec::new();
        for row in rows {
            let event = serde_json::from_str::<RunEventEnvelopeV1>(&row?)?;
            event.verify().map_err(|_| JournalError::Event)?;
            events.push(event);
        }
        Ok(events)
    }

    pub fn acknowledge_event(
        &self,
        event_id: uuid::Uuid,
        server_receipt: &str,
    ) -> Result<(), JournalError> {
        if server_receipt.trim().is_empty() {
            return Err(JournalError::Sequence);
        }
        let existing: Option<Option<String>> = self
            .connection
            .query_row(
                "SELECT server_receipt FROM run_events
                 WHERE json_extract(public_envelope,'$.event_id')=?1",
                [event_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        match existing {
            None => return Err(JournalError::Sequence),
            Some(Some(receipt)) if receipt != server_receipt => {
                return Err(JournalError::Sequence);
            }
            Some(Some(_)) => return Ok(()),
            Some(None) => {}
        }
        let changed = self.connection.execute(
            "UPDATE run_events SET server_receipt=?1
             WHERE json_extract(public_envelope,'$.event_id')=?2
               AND server_receipt IS NULL",
            params![server_receipt, event_id.to_string()],
        )?;
        if changed != 1 {
            return Err(JournalError::Sequence);
        }
        Ok(())
    }

    pub fn private_payload(&self, event_sha256: &str) -> Result<Option<Value>, JournalError> {
        let row: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT private_nonce,private_ciphertext FROM run_events
                 WHERE event_sha256=?1",
                [event_sha256],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        row.map(|(nonce, ciphertext)| {
            let plaintext = decrypt_bundle(
                &self.key,
                &BundleCiphertext { nonce, ciphertext },
                event_sha256.as_bytes(),
            )?;
            serde_json::from_slice(&plaintext).map_err(JournalError::from)
        })
        .transpose()
    }

    pub fn put_secret(&self, name: &str, secret: &[u8]) -> Result<(), JournalError> {
        if name.is_empty() {
            return Err(JournalError::Sequence);
        }
        let encrypted = encrypt_bundle(&self.key, secret, name.as_bytes())?;
        self.connection.execute(
            "INSERT INTO local_secrets(name,nonce,ciphertext,updated_at)
             VALUES(?1,?2,?3,unixepoch())
             ON CONFLICT(name) DO UPDATE SET
                nonce=excluded.nonce,ciphertext=excluded.ciphertext,updated_at=unixepoch()",
            params![name, encrypted.nonce, encrypted.ciphertext],
        )?;
        Ok(())
    }

    pub fn secret(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, JournalError> {
        let row: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT nonce,ciphertext FROM local_secrets WHERE name=?1",
                [name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(row
            .map(|(nonce, ciphertext)| {
                decrypt_bundle(
                    &self.key,
                    &BundleCiphertext { nonce, ciphertext },
                    name.as_bytes(),
                )
            })
            .transpose()?)
    }

    pub fn delete_secret(&self, name: &str) -> Result<(), JournalError> {
        if name.is_empty() {
            return Err(JournalError::Sequence);
        }
        self.connection
            .execute("DELETE FROM local_secrets WHERE name=?1", [name])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crow_agent_protocol::DeviceIdentity;
    use serde_json::json;
    use tempfile::tempdir;
    use time::OffsetDateTime;
    use uuid::Uuid;

    #[test]
    fn private_payload_round_trips_without_plaintext_storage()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let path = directory.path().join("journal.db");
        let identity = DeviceIdentity::generate();
        let event = RunEventEnvelopeV1::sign(
            identity.signing_key(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            None,
            1,
            "0".repeat(64),
            "decision".into(),
            OffsetDateTime::now_utc(),
            json!({"action": "hold"}),
        )?;
        let mut journal = EncryptedJournal::open(&path, [7_u8; 32])?;
        journal.append(&event, &json!({"raw_prompt": "secret strategy"}))?;
        assert_eq!(
            journal.private_payload(&event.event_sha256)?,
            Some(json!({"raw_prompt": "secret strategy"}))
        );
        let raw = std::fs::read(path)?;
        assert!(
            !raw.windows(b"secret strategy".len())
                .any(|value| value == b"secret strategy")
        );
        Ok(())
    }

    #[test]
    fn rotating_device_token_is_encrypted_at_rest() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let path = directory.path().join("journal.db");
        let journal = EncryptedJournal::open(&path, [11_u8; 32])?;
        journal.put_secret("device-refresh-token", b"crow_device_refresh_secret")?;
        let stored = journal
            .secret("device-refresh-token")?
            .ok_or("encrypted token was not stored")?;
        assert_eq!(stored.as_slice(), b"crow_device_refresh_secret".as_slice());
        drop(journal);
        let database = std::fs::read(path)?;
        assert!(
            !database
                .windows(b"crow_device_refresh_secret".len())
                .any(|value| value == b"crow_device_refresh_secret")
        );
        Ok(())
    }

    #[test]
    fn latest_event_state_recovers_sequence_and_hash() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let path = directory.path().join("journal.db");
        let identity = DeviceIdentity::generate();
        let run_id = Uuid::new_v4();
        let event = RunEventEnvelopeV1::sign(
            identity.signing_key(),
            Uuid::new_v4(),
            run_id,
            None,
            1,
            "0".repeat(64),
            "decision".into(),
            OffsetDateTime::now_utc(),
            json!({"action": "hold"}),
        )?;
        let mut journal = EncryptedJournal::open(&path, [13_u8; 32])?;
        assert_eq!(journal.latest_event_state(run_id)?, None);
        journal.append(&event, &json!({}))?;
        assert_eq!(
            journal.latest_event_state(run_id)?,
            Some((1, event.event_sha256))
        );
        Ok(())
    }

    #[test]
    fn event_count_recovers_completed_cycles() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let path = directory.path().join("journal.db");
        let identity = DeviceIdentity::generate();
        let arena_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let first = RunEventEnvelopeV1::sign(
            identity.signing_key(),
            arena_id,
            run_id,
            Some(Uuid::new_v4()),
            1,
            "0".repeat(64),
            "cycle_started".into(),
            OffsetDateTime::now_utc(),
            json!({"scheduled": true}),
        )?;
        let second = RunEventEnvelopeV1::sign(
            identity.signing_key(),
            arena_id,
            run_id,
            None,
            2,
            first.event_sha256.clone(),
            "run_resumed".into(),
            OffsetDateTime::now_utc(),
            json!({"source": "authenticated_control"}),
        )?;
        let mut journal = EncryptedJournal::open(&path, [17_u8; 32])?;
        journal.append(&first, &json!({}))?;
        journal.append(&second, &json!({}))?;
        assert_eq!(journal.event_count(run_id, "cycle_started")?, 1);
        assert_eq!(journal.event_count(run_id, "proposal")?, 0);
        Ok(())
    }

    #[test]
    fn pending_events_are_ordered_and_acknowledged_idempotently()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let path = directory.path().join("journal.db");
        let identity = DeviceIdentity::generate();
        let arena_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let first = RunEventEnvelopeV1::sign(
            identity.signing_key(),
            arena_id,
            run_id,
            None,
            1,
            "0".repeat(64),
            "run_started".into(),
            OffsetDateTime::now_utc(),
            json!({"release": "0.1.0"}),
        )?;
        let second = RunEventEnvelopeV1::sign(
            identity.signing_key(),
            arena_id,
            run_id,
            Some(Uuid::new_v4()),
            2,
            first.event_sha256.clone(),
            "cycle_started".into(),
            OffsetDateTime::now_utc(),
            json!({"scheduled": true}),
        )?;
        let mut journal = EncryptedJournal::open(&path, [23_u8; 32])?;
        journal.append(&first, &json!({"private": "first"}))?;
        journal.append(&second, &json!({"private": "second"}))?;
        assert_eq!(journal.pending_events(run_id)?, vec![first.clone(), second]);
        journal.acknowledge_event(first.event_id, "gateway-receipt-1")?;
        journal.acknowledge_event(first.event_id, "gateway-receipt-1")?;
        assert_eq!(journal.pending_events(run_id)?.len(), 1);
        assert!(
            journal
                .acknowledge_event(first.event_id, "different-receipt")
                .is_err()
        );
        Ok(())
    }
}
