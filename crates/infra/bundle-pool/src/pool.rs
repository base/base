use alloy_primitives::TxHash;
use alloy_primitives::map::HashMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex};
use tips_audit::{BundleEvent, DropReason};
use tips_core::BundleWithMetadata;
use tokio::sync::mpsc;
use tracing::warn;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum Action {
    Included,
    Skipped,
    Dropped,
}

#[derive(Debug, Clone)]
pub struct ProcessedBundle {
    pub bundle_uuid: Uuid,
    pub action: Action,
}

impl ProcessedBundle {
    pub fn new(bundle_uuid: Uuid, action: Action) -> Self {
        Self {
            bundle_uuid,
            action,
        }
    }
}

pub trait BundleStore {
    fn add_bundle(&mut self, bundle: BundleWithMetadata);
    fn get_bundles(&self) -> Vec<BundleWithMetadata>;
    fn built_flashblock(
        &mut self,
        block_number: u64,
        flashblock_index: u64,
        processed: Vec<ProcessedBundle>,
    );
    fn on_new_block(&mut self, number: u64, hash: TxHash);
}

struct BundleData {
    flashblocks_in_block: HashMap<u64, Vec<ProcessedBundle>>,
    bundles: HashMap<Uuid, BundleWithMetadata>,
}

#[derive(Clone)]
pub struct InMemoryBundlePool {
    inner: Arc<Mutex<BundleData>>,
    audit_log: mpsc::UnboundedSender<BundleEvent>,
    builder_id: String,
}

impl Debug for InMemoryBundlePool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryBundlePool")
            .field("builder_id", &self.builder_id)
            .field("bundle_count", &self.inner.lock().unwrap().bundles.len())
            .finish()
    }
}

impl InMemoryBundlePool {
    pub fn new(audit_log: mpsc::UnboundedSender<BundleEvent>, builder_id: String) -> Self {
        InMemoryBundlePool {
            inner: Arc::new(Mutex::new(BundleData {
                flashblocks_in_block: Default::default(),
                bundles: Default::default(),
            })),
            audit_log,
            builder_id,
        }
    }
}

impl BundleStore for InMemoryBundlePool {
    fn add_bundle(&mut self, bundle: BundleWithMetadata) {
        let mut inner = self.inner.lock().unwrap();
        inner.bundles.insert(*bundle.uuid(), bundle);
    }

    fn get_bundles(&self) -> Vec<BundleWithMetadata> {
        let inner = self.inner.lock().unwrap();
        inner.bundles.values().cloned().collect()
    }

    fn built_flashblock(
        &mut self,
        block_number: u64,
        flashblock_index: u64,
        processed: Vec<ProcessedBundle>,
    ) {
        let mut inner = self.inner.lock().unwrap();

        for p in &processed {
            let event = match p.action {
                Action::Included => Some(BundleEvent::BuilderIncluded {
                    bundle_id: p.bundle_uuid,
                    builder: self.builder_id.clone(),
                    block_number,
                    flashblock_index,
                }),
                Action::Dropped => Some(BundleEvent::Dropped {
                    bundle_id: p.bundle_uuid,
                    reason: DropReason::Reverted,
                }),
                _ => None,
            };

            if let Some(event) = event
                && let Err(e) = self.audit_log.send(event)
            {
                warn!(error = %e, "Failed to send event to audit log");
            }
        }

        for p in processed.iter() {
            inner.bundles.remove(&p.bundle_uuid);
        }

        let flashblocks_for_block = inner.flashblocks_in_block.entry(block_number).or_default();
        flashblocks_for_block.extend(processed);
    }

    fn on_new_block(&mut self, number: u64, hash: TxHash) {
        let mut inner = self.inner.lock().unwrap();

        let flashblocks_for_block = inner.flashblocks_in_block.entry(number).or_default();
        for p in flashblocks_for_block.iter() {
            let event = match p.action {
                Action::Included => Some(BundleEvent::BlockIncluded {
                    bundle_id: p.bundle_uuid,
                    block_number: number,
                    block_hash: hash,
                }),
                _ => None,
            };

            if let Some(event) = event
                && let Err(e) = self.audit_log.send(event)
            {
                warn!(error = %e, "Failed to send event to audit log");
            }
        }
        inner.flashblocks_in_block.remove(&number);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_signer_local::PrivateKeySigner;
    use tips_audit::BundleEvent;
    use tips_core::test_utils::{create_test_bundle, create_transaction};

    #[tokio::test]
    async fn test_operations() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let t1 = create_transaction(alice.clone(), 1, bob.address());
        let t2 = create_transaction(alice.clone(), 2, bob.address());
        let t3 = create_transaction(alice, 3, bob.address());

        let (event_tx, _event_rx) = mpsc::unbounded_channel::<BundleEvent>();
        let mut pool = InMemoryBundlePool::new(event_tx, "test-builder".to_string());
        let bundle1 = create_test_bundle(vec![t1], None, None, None);
        let bundle2 = create_test_bundle(vec![t2], None, None, None);
        let bundle3 = create_test_bundle(vec![t3], None, None, None);

        let uuid1 = *bundle1.uuid();
        let uuid2 = *bundle2.uuid();
        let uuid3 = *bundle3.uuid();

        pool.add_bundle(bundle1);
        pool.add_bundle(bundle2);
        pool.add_bundle(bundle3);

        let bundles = pool.get_bundles();
        assert_eq!(bundles.len(), 3);

        pool.built_flashblock(
            1,
            0,
            vec![
                ProcessedBundle {
                    bundle_uuid: uuid1,
                    action: Action::Included,
                },
                ProcessedBundle {
                    bundle_uuid: uuid2,
                    action: Action::Dropped,
                },
            ],
        );

        let bundles = pool.get_bundles();
        assert_eq!(bundles.len(), 1);
        assert_eq!(*bundles[0].uuid(), uuid3);
    }

    #[tokio::test]
    async fn test_with_audit() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let t1 = create_transaction(alice.clone(), 1, bob.address());
        let t2 = create_transaction(alice.clone(), 2, bob.address());
        let t3 = create_transaction(alice, 3, bob.address());

        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<BundleEvent>();
        let mut pool = InMemoryBundlePool::new(event_tx, "test-builder".to_string());

        let bundle1 = create_test_bundle(vec![t1], None, None, None);
        let bundle2 = create_test_bundle(vec![t2], None, None, None);
        let bundle3 = create_test_bundle(vec![t3], None, None, None);

        let uuid1 = *bundle1.uuid();
        let uuid2 = *bundle2.uuid();
        let uuid3 = *bundle3.uuid();

        pool.add_bundle(bundle1);
        pool.add_bundle(bundle2);
        pool.add_bundle(bundle3);

        let bundles = pool.get_bundles();
        assert_eq!(bundles.len(), 3);

        pool.built_flashblock(
            100,
            5,
            vec![
                ProcessedBundle {
                    bundle_uuid: uuid1,
                    action: Action::Included,
                },
                ProcessedBundle {
                    bundle_uuid: uuid2,
                    action: Action::Dropped,
                },
            ],
        );

        let event1 = event_rx.recv().await.unwrap();
        let event2 = event_rx.recv().await.unwrap();

        match &event1 {
            BundleEvent::BuilderIncluded {
                bundle_id,
                builder,
                block_number,
                flashblock_index,
            } => {
                assert_eq!(*bundle_id, uuid1);
                assert_eq!(builder, "test-builder");
                assert_eq!(*block_number, 100);
                assert_eq!(*flashblock_index, 5);
            }
            _ => panic!("Expected BuilderIncluded event"),
        }

        match &event2 {
            BundleEvent::Dropped {
                bundle_id,
                reason: _,
            } => {
                assert_eq!(*bundle_id, uuid2);
            }
            _ => panic!("Expected Dropped event"),
        }

        let bundles = pool.get_bundles();
        assert_eq!(bundles.len(), 1);
        assert_eq!(*bundles[0].uuid(), uuid3);
    }
}
