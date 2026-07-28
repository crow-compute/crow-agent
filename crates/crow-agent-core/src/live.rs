use crate::{EncryptedJournal, HarnessApiClient, HarnessApiError, JournalError};
use async_trait::async_trait;
use crow_agent_protocol::{DeviceIdentity, ProtocolError, RunEventEnvelopeV1};
use serde_json::Value;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum DurableRunEventError {
    #[error("local encrypted event journal failed")]
    Journal(#[from] JournalError),
    #[error("run event could not be signed")]
    Protocol(#[from] ProtocolError),
    #[error("Crow event ingestion is unavailable")]
    Sink,
}

#[async_trait]
pub trait RunEventSink: Send + Sync {
    async fn append_event(&self, event: &RunEventEnvelopeV1) -> Result<String, ()>;
}

#[async_trait]
impl RunEventSink for HarnessApiClient {
    async fn append_event(&self, event: &RunEventEnvelopeV1) -> Result<String, ()> {
        HarnessApiClient::append_event(self, event)
            .await
            .map_err(|_error: HarnessApiError| ())
    }
}

pub struct DurableRunEventWriter<'a, S> {
    journal: &'a mut EncryptedJournal,
    sink: &'a S,
    identity: &'a DeviceIdentity,
    arena_id: Uuid,
    run_id: Uuid,
}

impl<S> std::fmt::Debug for DurableRunEventWriter<'_, S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DurableRunEventWriter")
            .field("arena_id", &self.arena_id)
            .field("run_id", &self.run_id)
            .finish_non_exhaustive()
    }
}

impl<'a, S> DurableRunEventWriter<'a, S>
where
    S: RunEventSink,
{
    #[must_use]
    pub fn new(
        journal: &'a mut EncryptedJournal,
        sink: &'a S,
        identity: &'a DeviceIdentity,
        arena_id: Uuid,
        run_id: Uuid,
    ) -> Self {
        Self {
            journal,
            sink,
            identity,
            arena_id,
            run_id,
        }
    }

    pub async fn flush_pending(&self) -> Result<usize, DurableRunEventError> {
        let pending = self.journal.pending_events(self.run_id)?;
        let mut acknowledged = 0;
        for event in pending {
            let receipt = self
                .sink
                .append_event(&event)
                .await
                .map_err(|()| DurableRunEventError::Sink)?;
            self.journal.acknowledge_event(event.event_id, &receipt)?;
            acknowledged += 1;
        }
        Ok(acknowledged)
    }

    pub fn put_local_secret(&self, name: &str, value: &[u8]) -> Result<(), DurableRunEventError> {
        self.journal.put_secret(name, value)?;
        Ok(())
    }

    pub async fn append(
        &mut self,
        cycle_id: Option<Uuid>,
        event_type: impl Into<String>,
        payload: Value,
        private_payload: &Value,
    ) -> Result<RunEventEnvelopeV1, DurableRunEventError> {
        self.flush_pending().await?;
        let (sequence, previous_event_sha256) = self
            .journal
            .latest_event_state(self.run_id)?
            .map_or((1, "0".repeat(64)), |(sequence, hash)| {
                (sequence.saturating_add(1), hash)
            });
        let event = RunEventEnvelopeV1::sign(
            self.identity.signing_key(),
            self.arena_id,
            self.run_id,
            cycle_id,
            sequence,
            previous_event_sha256,
            event_type.into(),
            OffsetDateTime::now_utc(),
            payload,
        )?;
        self.journal.append(&event, private_payload)?;
        let receipt = self
            .sink
            .append_event(&event)
            .await
            .map_err(|()| DurableRunEventError::Sink)?;
        self.journal.acknowledge_event(event.event_id, &receipt)?;
        Ok(event.with_server_receipt(receipt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use tempfile::tempdir;

    #[derive(Debug, Default)]
    struct RecordingSink {
        fail_once: AtomicBool,
        events: Mutex<Vec<RunEventEnvelopeV1>>,
    }

    #[async_trait]
    impl RunEventSink for RecordingSink {
        async fn append_event(&self, event: &RunEventEnvelopeV1) -> Result<String, ()> {
            if self.fail_once.swap(false, Ordering::SeqCst) {
                return Err(());
            }
            self.events.lock().map_err(|_| ())?.push(event.clone());
            Ok(format!("receipt-{}", event.event_id))
        }
    }

    #[tokio::test]
    async fn failed_ingestion_stays_pending_and_retries_before_next_event()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let identity = DeviceIdentity::generate();
        let arena_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let mut journal =
            EncryptedJournal::open(&directory.path().join("journal.db"), [31_u8; 32])?;
        let sink = RecordingSink {
            fail_once: AtomicBool::new(true),
            ..RecordingSink::default()
        };
        let mut writer =
            DurableRunEventWriter::new(&mut journal, &sink, &identity, arena_id, run_id);
        assert!(
            writer
                .append(
                    None,
                    "run_started",
                    json!({"release": "0.1.0"}),
                    &json!({"strategy": "private"}),
                )
                .await
                .is_err()
        );
        assert_eq!(writer.journal.pending_events(run_id)?.len(), 1);
        let second = writer
            .append(
                Some(Uuid::new_v4()),
                "cycle_started",
                json!({"scheduled": true}),
                &Value::Null,
            )
            .await?;
        assert_eq!(second.sequence, 2);
        assert!(writer.journal.pending_events(run_id)?.is_empty());
        let accepted = sink.events.lock().map_err(|_| "poisoned")?;
        assert_eq!(accepted.len(), 2);
        assert_eq!(accepted[0].sequence, 1);
        assert_eq!(accepted[1].sequence, 2);
        Ok(())
    }
}
