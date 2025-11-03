//! Dummy flashblocks integration for testing pending state

use alloy_primitives::{Bytes, TxHash};
use base_reth_flashblocks_rpc::subscription::Flashblock;
use eyre::Result;
use tokio::sync::{mpsc, oneshot};

// Re-export types from flashblocks-rpc
pub use base_reth_flashblocks_rpc::subscription::{Flashblock as FlashblockPayload, Metadata as FlashblockMetadata};

/// Context for managing dummy flashblock delivery in tests
///
/// This provides a non-WebSocket, queue-based mechanism for delivering
/// flashblocks to a test node, similar to how the rpc.rs tests work currently.
pub struct FlashblocksContext {
    /// Channel for sending flashblocks to the node
    sender: mpsc::Sender<(Flashblock, oneshot::Sender<()>)>,
}

impl FlashblocksContext {
    /// Create a new flashblocks context with a channel
    ///
    /// Returns the context and a receiver that the node should consume
    pub fn new() -> (Self, mpsc::Receiver<(Flashblock, oneshot::Sender<()>)>) {
        let (sender, receiver) = mpsc::channel(100);
        (Self { sender }, receiver)
    }

    /// Send a flashblock to the node and wait for processing
    pub async fn send_flashblock(&self, flashblock: Flashblock) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.sender.send((flashblock, tx)).await?;
        rx.await?;
        Ok(())
    }

    /// Send multiple flashblocks sequentially
    pub async fn send_flashblocks(&self, flashblocks: Vec<Flashblock>) -> Result<()> {
        for flashblock in flashblocks {
            self.send_flashblock(flashblock).await?;
        }
        Ok(())
    }
}

impl Default for FlashblocksContext {
    fn default() -> Self {
        Self::new().0
    }
}

/// Helper to extract transactions from a vec of flashblocks
pub fn extract_transactions_from_flashblocks(flashblocks: &[Flashblock]) -> Vec<Bytes> {
    let mut all_txs = Vec::new();

    for flashblock in flashblocks {
        all_txs.extend(flashblock.diff.transactions.clone());
    }

    all_txs
}

/// Helper to get all transaction hashes from flashblock metadata
pub fn extract_tx_hashes_from_flashblocks(flashblocks: &[Flashblock]) -> Vec<TxHash> {
    let mut all_hashes = Vec::new();

    for flashblock in flashblocks {
        all_hashes.extend(flashblock.metadata.receipts.keys().copied());
    }

    all_hashes
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, U256};
    use alloy_primitives::map::HashMap;
    use alloy_consensus::Receipt;
    use alloy_rpc_types_engine::PayloadId;
    use base_reth_flashblocks_rpc::subscription::Metadata;
    use op_alloy_consensus::OpDepositReceipt;
    use reth_optimism_primitives::OpReceipt;
    use rollup_boost::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1};

    fn create_test_flashblock() -> Flashblock {
        Flashblock {
            payload_id: PayloadId::new([0; 8]),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::default(),
                parent_hash: B256::default(),
                fee_recipient: Address::ZERO,
                prev_randao: B256::default(),
                block_number: 1,
                gas_limit: 30_000_000,
                timestamp: 0,
                extra_data: Bytes::new(),
                base_fee_per_gas: U256::ZERO,
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                transactions: vec![Bytes::from(vec![0x01, 0x02, 0x03])],
                ..Default::default()
            },
            metadata: Metadata {
                block_number: 1,
                receipts: {
                    let mut receipts = HashMap::default();
                    receipts.insert(
                        B256::random(),
                        OpReceipt::Deposit(OpDepositReceipt {
                            inner: Receipt {
                                status: true.into(),
                                cumulative_gas_used: 21000,
                                logs: vec![],
                            },
                            deposit_nonce: Some(1),
                            deposit_receipt_version: None,
                        }),
                    );
                    receipts
                },
                new_account_balances: HashMap::default(),
            },
        }
    }

    #[tokio::test]
    async fn test_flashblocks_context() {
        let (ctx, mut receiver) = FlashblocksContext::new();
        let flashblock = create_test_flashblock();

        // Spawn a task to receive and acknowledge
        let handle = tokio::spawn(async move {
            if let Some((fb, tx)) = receiver.recv().await {
                assert_eq!(fb.metadata.block_number, 1);
                tx.send(()).unwrap();
            }
        });

        // Send flashblock
        ctx.send_flashblock(flashblock).await.unwrap();

        // Wait for receiver task
        handle.await.unwrap();
    }

    #[test]
    fn test_extract_transactions() {
        let flashblock = create_test_flashblock();
        let txs = extract_transactions_from_flashblocks(&[flashblock]);

        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0], Bytes::from(vec![0x01, 0x02, 0x03]));
    }
}
