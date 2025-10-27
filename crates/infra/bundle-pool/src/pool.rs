use alloy_primitives::map::HashMap;
use tips_audit::{BundleEvent, DropReason};
use tips_core::BundleWithMetadata;
use tokio::sync::mpsc;
use tracing::warn;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum Action {
    Included,
    Dropped,
}

#[derive(Debug, Clone)]
pub struct ProcessedBundle {
    pub bundle_uuid: Uuid,
    pub action: Action,
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
}

pub struct InMemoryBundlePool {
    bundles: HashMap<Uuid, BundleWithMetadata>,
    audit_log: mpsc::UnboundedSender<BundleEvent>,
    builder_id: String,
}

impl InMemoryBundlePool {
    pub fn new(audit_log: mpsc::UnboundedSender<BundleEvent>, builder_id: String) -> Self {
        InMemoryBundlePool {
            bundles: Default::default(),
            audit_log,
            builder_id,
        }
    }
}

impl BundleStore for InMemoryBundlePool {
    fn add_bundle(&mut self, bundle: BundleWithMetadata) {
        self.bundles.insert(*bundle.uuid(), bundle);
    }

    fn get_bundles(&self) -> Vec<BundleWithMetadata> {
        self.bundles.values().cloned().collect()
    }

    fn built_flashblock(
        &mut self,
        block_number: u64,
        flashblock_index: u64,
        processed: Vec<ProcessedBundle>,
    ) {
        for p in &processed {
            let event = match p.action {
                Action::Included => BundleEvent::BuilderIncluded {
                    bundle_id: p.bundle_uuid,
                    builder: self.builder_id.clone(),
                    block_number,
                    flashblock_index,
                },
                Action::Dropped => BundleEvent::Dropped {
                    bundle_id: p.bundle_uuid,
                    reason: DropReason::Reverted,
                },
            };

            if let Err(e) = self.audit_log.send(event) {
                warn!(error = %e, "Failed to send event to audit log");
            }
        }

        for p in processed {
            self.bundles.remove(&p.bundle_uuid);
        }
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
