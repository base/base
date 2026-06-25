//! RPC-fed event reader.
//!
//! `RpcEventReader` implements [`EventReader`] by receiving [`Event`]s from a
//! tokio mpsc channel. The other side of the channel is fed by the audit
//! service's RPC handler (`AuditArchiverRpc::persist_batched_bundle_event`),
//! which performs in-memory dedup via a moka cache before forwarding events
//! into this reader.
//!
//! Because RPC delivery is at-most-once and there is no broker offset to
//! commit, [`commit`](RpcEventReader::commit) is a no-op.

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::reader::{Event, EventReader};

/// Reads bundle audit events from an in-process mpsc channel fed by the audit
/// RPC handler.
#[derive(Debug)]
pub struct RpcEventReader {
    event_rx: mpsc::Receiver<Event>,
}

impl RpcEventReader {
    /// Creates a new RPC event reader that pulls events from `event_rx`.
    pub const fn new(event_rx: mpsc::Receiver<Event>) -> Self {
        Self { event_rx }
    }
}

#[async_trait]
impl EventReader for RpcEventReader {
    async fn read_event(&mut self) -> Result<Event> {
        self.event_rx.recv().await.ok_or_else(|| anyhow::anyhow!("RPC event channel closed"))
    }

    async fn commit(&mut self) -> Result<()> {
        // RPC delivery is at-most-once; no offset to commit.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use base_bundles::{BundleExtensions, test_utils::create_bundle_from_txn_data};
    use tokio::sync::mpsc;
    use uuid::Uuid;

    use super::*;
    use crate::types::BundleEvent;

    fn sample_event(key: &str) -> Event {
        let bundle = create_bundle_from_txn_data();
        let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());
        Event {
            key: key.to_string(),
            event: BundleEvent::Received { bundle_id, bundle: Box::new(bundle) },
            timestamp: 0,
        }
    }

    #[tokio::test]
    async fn read_event_returns_sent_event() {
        let (tx, rx) = mpsc::channel(8);
        let mut reader = RpcEventReader::new(rx);

        let event = sample_event("received-0xabc");
        tx.send(event.clone()).await.unwrap();

        let got = reader.read_event().await.unwrap();
        assert_eq!(got.key, event.key, "key should round-trip through the channel");
    }

    #[tokio::test]
    async fn read_event_errors_when_channel_closed() {
        let (tx, rx) = mpsc::channel::<Event>(1);
        let mut reader = RpcEventReader::new(rx);
        drop(tx);

        let err = reader.read_event().await.expect_err("closed channel should error");
        assert!(
            err.to_string().contains("RPC event channel closed"),
            "error should mention closed channel, got: {err}",
        );
    }

    #[tokio::test]
    async fn commit_is_noop() {
        let (_tx, rx) = mpsc::channel::<Event>(1);
        let mut reader = RpcEventReader::new(rx);
        reader.commit().await.expect("commit should be a no-op success");
    }
}
