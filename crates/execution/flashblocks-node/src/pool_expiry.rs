//! Flashblock receiver that frees validity-transaction deadlines in the local pool.

use std::sync::Arc;

use base_common_flashblocks::Flashblock;
use base_execution_txpool::FlashblockExpiry;
use base_flashblocks::FlashblocksReceiver;
use tracing::debug;

/// Forwards every received flashblock to the pending-state receiver and records its position in
/// the local transaction pool.
///
/// The builder evicts a validity transaction the moment the flashblock holding its deadline is
/// published. A node that only follows that stream learned of the deadline no earlier than the
/// next block commit, so an unbumped same-nonce replacement submitted in between was rejected as
/// underpriced against a transaction the builder had already dropped. Recording the published
/// position here closes that gap for transaction ingress on mempool and RPC nodes. The pool
/// bounds the recorded position against its canonical tip, so this receiver forwards every
/// flashblock unconditionally and leaves the trust decision to the pool.
#[derive(Debug)]
pub struct PoolExpiringReceiver<Receiver, Pool> {
    receiver: Arc<Receiver>,
    pool: Pool,
}

impl<Receiver, Pool> PoolExpiringReceiver<Receiver, Pool> {
    /// Wraps `receiver` so `pool` observes each flashblock before the pending state does.
    pub const fn new(receiver: Arc<Receiver>, pool: Pool) -> Self {
        Self { receiver, pool }
    }
}

impl<Receiver, Pool> FlashblocksReceiver for PoolExpiringReceiver<Receiver, Pool>
where
    Receiver: FlashblocksReceiver,
    Pool: FlashblockExpiry,
{
    fn on_flashblock_received(&self, flashblock: Flashblock) {
        let block_number = flashblock.metadata.block_number;
        let flashblock_index = flashblock.index;
        // Expire before handing off: the pending-state processor executes asynchronously, and the
        // same-nonce slot must be free by the time a client reacting to this flashblock resubmits.
        let expired = self.pool.expire_published(block_number, flashblock_index);
        if expired > 0 {
            debug!(
                block = block_number,
                flashblock = flashblock_index,
                expired,
                "validity transactions expired at received flashblock"
            );
        }
        self.receiver.on_flashblock_received(flashblock);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use alloy_rpc_types_engine::PayloadId;
    use base_common_flashblocks::{ExecutionPayloadFlashblockDeltaV1, Metadata};

    use super::*;

    /// Records the positions the pool was asked to expire. `FlashblockExpiry::publish_and_expire`
    /// is generic over its closure, which `mockall` cannot express.
    #[derive(Clone, Default)]
    struct RecordingPool(Arc<Mutex<Vec<(u64, u64)>>>);

    impl FlashblockExpiry for RecordingPool {
        fn publish_and_expire<R, E>(
            &self,
            _block_number: u64,
            _flashblock_index: u64,
            _publish: impl FnOnce() -> Result<R, E>,
        ) -> Result<(R, usize), E> {
            unreachable!("followers never publish")
        }

        fn expire_published(&self, block_number: u64, flashblock_index: u64) -> usize {
            self.0.lock().unwrap().push((block_number, flashblock_index));
            1
        }
    }

    #[derive(Default)]
    struct RecordingReceiver(Mutex<Vec<(u64, u64)>>);

    impl FlashblocksReceiver for RecordingReceiver {
        fn on_flashblock_received(&self, flashblock: Flashblock) {
            self.0.lock().unwrap().push((flashblock.metadata.block_number, flashblock.index));
        }
    }

    fn flashblock(block_number: u64, index: u64) -> Flashblock {
        Flashblock {
            payload_id: PayloadId::default(),
            index,
            base: None,
            diff: ExecutionPayloadFlashblockDeltaV1::default(),
            metadata: Metadata::new(block_number),
        }
    }

    #[test]
    fn expires_pool_at_received_position_then_forwards() {
        let pool = RecordingPool::default();
        let inner = Arc::new(RecordingReceiver::default());
        let receiver = PoolExpiringReceiver::new(Arc::clone(&inner), pool.clone());

        receiver.on_flashblock_received(flashblock(100, 0));
        receiver.on_flashblock_received(flashblock(100, 3));

        assert_eq!(*pool.0.lock().unwrap(), vec![(100, 0), (100, 3)]);
        assert_eq!(*inner.0.lock().unwrap(), vec![(100, 0), (100, 3)]);
    }
}
