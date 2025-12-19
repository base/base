use super::interfaces::event_source::EventSource;
use crate::domain::{events::MempoolEvent, mempool::Mempool};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

pub struct MempoolEngine<T: Mempool> {
    mempool: Arc<RwLock<T>>,
    event_source: Arc<dyn EventSource>,
}

impl<T: Mempool> MempoolEngine<T> {
    pub fn new(mempool: Arc<RwLock<T>>, event_source: Arc<dyn EventSource>) -> MempoolEngine<T> {
        Self {
            mempool,
            event_source,
        }
    }

    pub fn get_mempool(&self) -> Arc<RwLock<T>> {
        Arc::clone(&self.mempool)
    }

    pub async fn run(&self) {
        loop {
            if let Err(err) = self.process_next().await {
                warn!(error = %err, "Mempool engine error, continuing");
            }
        }
    }

    pub async fn process_next(&self) -> anyhow::Result<()> {
        let event = self.event_source.receive().await?;
        self.handle_event(event).await
    }

    async fn handle_event(&self, event: MempoolEvent) -> anyhow::Result<()> {
        info!(
            event = ?event,
            "Mempool engine handling event"
        );
        match event {
            MempoolEvent::UserOpAdded { user_op } => {
                self.mempool.write().await.add_operation(&user_op)?;
            }
            MempoolEvent::UserOpIncluded { user_op } => {
                self.mempool.write().await.remove_operation(&user_op.hash)?;
            }
            MempoolEvent::UserOpDropped { user_op, reason: _ } => {
                self.mempool.write().await.remove_operation(&user_op.hash)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        mempool::PoolConfig,
        types::{VersionedUserOperation, WrappedUserOperation},
    };
    use crate::infrastructure::in_memory::mempool::InMemoryMempool;
    use crate::services::interfaces::event_source::EventSource;
    use alloy_primitives::{Address, FixedBytes, Uint};
    use alloy_rpc_types::erc4337;
    use async_trait::async_trait;
    use tokio::sync::Mutex;

    fn make_wrapped_op(max_fee: u128, hash: [u8; 32]) -> WrappedUserOperation {
        let op = VersionedUserOperation::UserOperation(erc4337::UserOperation {
            sender: Address::ZERO,
            nonce: Uint::from(0u64),
            init_code: Default::default(),
            call_data: Default::default(),
            call_gas_limit: Uint::from(100_000u64),
            verification_gas_limit: Uint::from(100_000u64),
            pre_verification_gas: Uint::from(21_000u64),
            max_fee_per_gas: Uint::from(max_fee),
            max_priority_fee_per_gas: Uint::from(max_fee),
            paymaster_and_data: Default::default(),
            signature: Default::default(),
        });

        WrappedUserOperation {
            operation: op,
            hash: FixedBytes::from(hash),
        }
    }

    struct MockEventSource {
        events: Mutex<Vec<MempoolEvent>>,
    }

    impl MockEventSource {
        fn new(events: Vec<MempoolEvent>) -> Self {
            Self {
                events: Mutex::new(events),
            }
        }
    }

    #[async_trait]
    impl EventSource for MockEventSource {
        async fn receive(&self) -> anyhow::Result<MempoolEvent> {
            let mut guard = self.events.lock().await;
            if guard.is_empty() {
                Err(anyhow::anyhow!("no more events"))
            } else {
                Ok(guard.remove(0))
            }
        }
    }

    #[tokio::test]
    async fn handle_add_operation() {
        let mempool = Arc::new(RwLock::new(InMemoryMempool::new(PoolConfig::default())));

        let op_hash = [1u8; 32];
        let wrapped = make_wrapped_op(1_000, op_hash);

        let add_event = MempoolEvent::UserOpAdded {
            user_op: wrapped.clone(),
        };
        let mock_source = Arc::new(MockEventSource::new(vec![add_event]));

        let engine = MempoolEngine::new(mempool.clone(), mock_source);

        engine.process_next().await.unwrap();
        let items: Vec<_> = mempool.read().await.get_top_operations(10).collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].hash, FixedBytes::from(op_hash));
    }

    #[tokio::test]
    async fn remove_operation_should_remove_from_mempool() {
        let mempool = Arc::new(RwLock::new(InMemoryMempool::new(PoolConfig::default())));
        let op_hash = [1u8; 32];
        let wrapped = make_wrapped_op(1_000, op_hash);
        let add_event = MempoolEvent::UserOpAdded {
            user_op: wrapped.clone(),
        };
        let remove_event = MempoolEvent::UserOpDropped {
            user_op: wrapped.clone(),
            reason: "test".to_string(),
        };
        let mock_source = Arc::new(MockEventSource::new(vec![add_event, remove_event]));

        let engine = MempoolEngine::new(mempool.clone(), mock_source);
        engine.process_next().await.unwrap();
        let items: Vec<_> = mempool.read().await.get_top_operations(10).collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].hash, FixedBytes::from(op_hash));
        engine.process_next().await.unwrap();
        let items: Vec<_> = mempool.read().await.get_top_operations(10).collect();
        assert_eq!(items.len(), 0);
    }
}
