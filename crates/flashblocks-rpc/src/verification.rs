use crate::cache::Cache;
use crate::metrics::Metrics;
use futures_util::TryStreamExt;
use op_alloy_consensus::OpTxEnvelope;
use reth::api::FullNodeComponents;
use reth::builder::NodeTypes;
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_optimism_primitives::OpBlock;
use reth_optimism_primitives::OpPrimitives;
use reth_primitives::Block;
use std::sync::Arc;
use tracing::debug;
use tracing::{error, info, warn};

#[derive(Debug)]
pub enum VerificationResult {
    Match,
    BlockNotFound,
    TransactionCountMismatch {
        expected: usize,
        actual: usize,
    },
    TransactionMismatches {
        count: usize,
    },
    TransactionHashMismatch {
        expected: Vec<String>,
        actual: Vec<String>,
    },
    TransactionHashListNotFound,
}

pub struct VerificationService {
    cache: Arc<Cache>,
    metrics: Metrics,
}

impl VerificationService {
    pub fn new(cache: Arc<Cache>) -> Self {
        Self {
            cache,
            metrics: Metrics::default(),
        }
    }

    pub fn verify_block(&self, block: &Block<OpTxEnvelope>) -> VerificationResult {
        let block_number = block.number;

        let cached_block = match self
            .cache
            .get::<OpBlock>(&format!("block:{}", block_number))
        {
            Some(cached_block) => cached_block,
            None => {
                error!(
                    "Block {} not found in cache during verification",
                    block_number
                );
                self.metrics.block_verification_not_found.increment(1);
                return VerificationResult::BlockNotFound;
            }
        };

        let expected_txs = cached_block.body.transactions.len();
        let actual_txs = block.body.transactions.len();

        if expected_txs != actual_txs {
            error!(
                "Transaction count mismatch for block {}: expected {}, got {}",
                block_number, expected_txs, actual_txs
            );
            self.metrics
                .block_verification_tx_count_mismatch
                .increment(1);
            self.metrics.block_verification_failure.increment(1);
            return VerificationResult::TransactionCountMismatch {
                expected: expected_txs,
                actual: actual_txs,
            };
        }

        let expected_tx_hashes = self
            .cache
            .get::<Vec<String>>(&format!("tx_hashes:{}", block_number));
        if expected_tx_hashes.is_none() {
            error!(
                "No transaction hash list found in cache for block {}",
                block_number
            );
            self.metrics.block_verification_failure.increment(1);
            return VerificationResult::TransactionHashListNotFound;
        }
        let expected_tx_hashes = expected_tx_hashes.unwrap();

        let actual_tx_hashes: Vec<String> = block
            .body
            .transactions
            .iter()
            .map(|tx| tx.tx_hash().to_string())
            .collect();

        if expected_tx_hashes != actual_tx_hashes {
            error!(
                "Transaction hash mismatch for block {}: expected {:?}, got {:?}",
                block_number, expected_tx_hashes, actual_tx_hashes
            );
            self.metrics.block_verification_failure.increment(1);
            return VerificationResult::TransactionHashMismatch {
                expected: expected_tx_hashes,
                actual: actual_tx_hashes,
            };
        }

        let mut mismatch_count = 0;
        for (index, (expected_tx, actual_tx)) in cached_block
            .body
            .transactions
            .iter()
            .zip(block.body.transactions.iter())
            .enumerate()
        {
            let expected_hash = expected_tx.tx_hash();
            let actual_hash = actual_tx.tx_hash();

            if expected_hash != actual_hash {
                error!(
                    "Transaction mismatch at index {} in block {}: expected {:?}, got {:?}",
                    index, block_number, expected_hash, actual_hash
                );
                mismatch_count += 1;
            }
        }

        if mismatch_count > 0 {
            self.metrics.block_verification_failure.increment(1);
            error!(
                block = block.number,
                mismatch_count = mismatch_count,
                "Found {} transaction mismatches during verification",
                mismatch_count
            );
            return VerificationResult::TransactionMismatches {
                count: mismatch_count,
            };
        }

        self.metrics.block_verification_success.increment(1);
        VerificationResult::Match
    }
}

impl Clone for VerificationService {
    fn clone(&self) -> Self {
        Self {
            cache: Arc::clone(&self.cache),
            metrics: self.metrics.clone(),
        }
    }
}

pub async fn flashblocks_verification_exex<
    Node: FullNodeComponents<Types: NodeTypes<Primitives = OpPrimitives>>,
>(
    mut ctx: ExExContext<Node>,
    cache: Arc<Cache>,
) -> eyre::Result<()> {
    info!("FlashblocksVerification ExEx started");

    let verification_service = VerificationService::new(cache);
    while let Some(notification) = ctx.notifications.try_next().await? {
        match &notification {
            ExExNotification::ChainCommitted { new } => {
                debug!(committed_chain = ?new.range(), "Verifying committed chain");
                for block in new.blocks().values() {
                    verification_service.verify_block(&block.clone().into_block());
                }
            }
            ExExNotification::ChainReorged { old: _, new } => {
                warn!(new_chain = ?new.range(), "Verifying reorged chain");
                for block in new.blocks().values() {
                    verification_service.verify_block(&block.clone().into_block());
                }
            }
            ExExNotification::ChainReverted { old: _ } => {
                warn!("Chain reverted, previously verified flashblock transactions are now invalid unless they are in the new chain");
            }
        }

        if let Some(committed_chain) = notification.committed_chain() {
            ctx.events
                .send(ExExEvent::FinishedHeight(committed_chain.tip().num_hash()))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Sealable, B256};
    use op_alloy_consensus::TxDeposit;
    use reth_optimism_primitives::{OpBlock, OpTransactionSigned};
    use reth_primitives::{Block, BlockBody, Header};

    fn create_mock_tx(source_hash: u8) -> OpTransactionSigned {
        let source_hash = B256::from_slice(
            &[0; 31][..31]
                .iter()
                .chain(&[source_hash])
                .copied()
                .collect::<Vec<_>>(),
        );
        let tx = TxDeposit {
            source_hash,
            ..Default::default()
        };
        OpTransactionSigned::Deposit(tx.seal_slow())
    }

    fn create_mock_op_block(number: u64, txs: Vec<OpTransactionSigned>) -> OpBlock {
        let header = Header {
            number,
            ..Default::default()
        };

        let body = BlockBody {
            transactions: txs,
            ommers: vec![],
            withdrawals: None,
        };

        OpBlock { header, body }
    }

    fn create_mock_block(number: u64, txs: Vec<OpTransactionSigned>) -> Block<OpTransactionSigned> {
        let header = Header {
            number,
            ..Default::default()
        };

        let body = BlockBody {
            transactions: txs
                .into_iter()
                .map(|tx| OpTransactionSigned::from(tx))
                .collect(),
            ommers: vec![],
            withdrawals: None,
        };

        Block { header, body }
    }

    fn setup_test_cache() -> Arc<Cache> {
        Arc::new(Cache::default())
    }

    #[test]
    fn test_valid_block_match() {
        let cache = setup_test_cache();
        let verification_service = VerificationService::new(Arc::clone(&cache));

        let block_number = 1;
        let tx1 = create_mock_tx(1);
        let tx2 = create_mock_tx(2);

        let cached_block = create_mock_op_block(block_number, vec![tx1.clone(), tx2.clone()]);
        cache
            .set(&format!("block:{}", block_number), &cached_block, None)
            .unwrap();

        let tx_hashes = vec![tx1.hash().to_string(), tx2.hash().to_string()];
        cache
            .set(&format!("tx_hashes:{}", block_number), &tx_hashes, None)
            .unwrap();

        let final_block = create_mock_block(block_number, vec![tx1.clone(), tx2.clone()]);

        let result = verification_service.verify_block(&final_block);

        match result {
            VerificationResult::Match => {} // Test passed
            _ => panic!("Expected VerificationResult::Match, got {:?}", result),
        }
    }

    #[test]
    fn test_transaction_in_cache_not_in_block() {
        let cache = setup_test_cache();
        let verification_service = VerificationService::new(Arc::clone(&cache));

        let block_number = 1;
        let tx1 = create_mock_tx(1);
        let tx2 = create_mock_tx(2);
        let tx3 = create_mock_tx(3);

        let cached_block =
            create_mock_op_block(block_number, vec![tx1.clone(), tx2.clone(), tx3.clone()]);
        cache
            .set(&format!("block:{}", block_number), &cached_block, None)
            .unwrap();

        let tx_hashes = vec![
            tx1.hash().to_string(),
            tx2.hash().to_string(),
            tx3.hash().to_string(),
        ];
        cache
            .set(&format!("tx_hashes:{}", block_number), &tx_hashes, None)
            .unwrap();

        let final_block = create_mock_block(block_number, vec![tx1.clone(), tx2.clone()]);

        let result = verification_service.verify_block(&final_block);

        match result {
            VerificationResult::TransactionCountMismatch { expected, actual } => {
                assert_eq!(expected, 3);
                assert_eq!(actual, 2);
            }
            _ => panic!("Expected TransactionCountMismatch, got {:?}", result),
        }
    }

    #[test]
    fn test_transaction_in_block_not_in_cache() {
        let cache = setup_test_cache();
        let verification_service = VerificationService::new(Arc::clone(&cache));

        let block_number = 1;
        let tx1 = create_mock_tx(1);
        let tx2 = create_mock_tx(2);
        let tx3 = create_mock_tx(3);

        let cached_block = create_mock_op_block(block_number, vec![tx1.clone(), tx2.clone()]);
        cache
            .set(&format!("block:{}", block_number), &cached_block, None)
            .unwrap();

        let tx_hashes = vec![tx1.hash().to_string(), tx2.hash().to_string()];
        cache
            .set(&format!("tx_hashes:{}", block_number), &tx_hashes, None)
            .unwrap();

        let final_block =
            create_mock_block(block_number, vec![tx1.clone(), tx2.clone(), tx3.clone()]);

        let result = verification_service.verify_block(&final_block);

        match result {
            VerificationResult::TransactionCountMismatch { expected, actual } => {
                assert_eq!(expected, 2);
                assert_eq!(actual, 3);
            }
            _ => panic!("Expected TransactionCountMismatch, got {:?}", result),
        }
    }

    #[test]
    fn test_cached_block_not_present() {
        let cache = setup_test_cache();
        let verification_service = VerificationService::new(Arc::clone(&cache));

        let block_number = 1;
        let tx1 = create_mock_tx(1);
        let tx2 = create_mock_tx(2);

        let final_block = create_mock_block(block_number, vec![tx1.clone(), tx2.clone()]);

        let result = verification_service.verify_block(&final_block);

        match result {
            VerificationResult::BlockNotFound => {} // Test passed
            _ => panic!("Expected BlockNotFound, got {:?}", result),
        }
    }

    #[test]
    fn test_transaction_hash_mismatch() {
        let cache = setup_test_cache();
        let verification_service = VerificationService::new(Arc::clone(&cache));

        let block_number = 1;
        let tx1 = create_mock_tx(1);
        let tx2 = create_mock_tx(2);
        let tx3 = create_mock_tx(3); // Different transaction for the block

        let cached_block = create_mock_op_block(block_number, vec![tx1.clone(), tx2.clone()]);
        cache
            .set(&format!("block:{}", block_number), &cached_block, None)
            .unwrap();

        let tx_hashes = vec![tx1.hash().to_string(), tx2.hash().to_string()];
        cache
            .set(&format!("tx_hashes:{}", block_number), &tx_hashes, None)
            .unwrap();

        let final_block = create_mock_block(block_number, vec![tx1.clone(), tx3.clone()]);

        let result = verification_service.verify_block(&final_block);

        match result {
            VerificationResult::TransactionHashMismatch { expected, actual } => {
                assert_eq!(expected.len(), 2);
                assert_eq!(actual.len(), 2);
                assert_eq!(expected[1], tx2.hash().to_string());
                assert_eq!(actual[1], tx3.hash().to_string());
            }
            _ => panic!("Expected TransactionHashMismatch, got {:?}", result),
        }
    }

    #[test]
    fn test_transaction_hash_list_not_found() {
        let cache = setup_test_cache();
        let verification_service = VerificationService::new(Arc::clone(&cache));

        let block_number = 1;
        let tx1 = create_mock_tx(1);
        let tx2 = create_mock_tx(2);

        let cached_block = create_mock_op_block(block_number, vec![tx1.clone(), tx2.clone()]);
        cache
            .set(&format!("block:{}", block_number), &cached_block, None)
            .unwrap();

        let final_block = create_mock_block(block_number, vec![tx1.clone(), tx2.clone()]);

        let result = verification_service.verify_block(&final_block);

        match result {
            VerificationResult::TransactionHashListNotFound => {} // Test passed
            _ => panic!("Expected TransactionHashListNotFound, got {:?}", result),
        }
    }

    #[test]
    fn test_all_transaction_hashes_mismatch() {
        let cache = setup_test_cache();
        let verification_service = VerificationService::new(Arc::clone(&cache));

        let block_number = 1;
        let tx1 = create_mock_tx(1);
        let tx2 = create_mock_tx(2);

        let different_tx1 = create_mock_tx(3);
        let different_tx2 = create_mock_tx(4);

        let cached_block = create_mock_op_block(block_number, vec![tx1.clone(), tx2.clone()]);
        cache
            .set(&format!("block:{}", block_number), &cached_block, None)
            .unwrap();

        let tx_hashes = vec![tx1.hash().to_string(), tx2.hash().to_string()];
        cache
            .set(&format!("tx_hashes:{}", block_number), &tx_hashes, None)
            .unwrap();

        let final_block = create_mock_block(
            block_number,
            vec![different_tx1.clone(), different_tx2.clone()],
        );

        let result = verification_service.verify_block(&final_block);

        match result {
            VerificationResult::TransactionHashMismatch { expected, actual } => {
                assert_eq!(expected.len(), 2);
                assert_eq!(actual.len(), 2);
                assert_eq!(expected[0], tx1.hash().to_string());
                assert_eq!(expected[1], tx2.hash().to_string());
                assert_eq!(actual[0], different_tx1.hash().to_string());
                assert_eq!(actual[1], different_tx2.hash().to_string());
            }
            _ => panic!("Expected TransactionHashMismatch, got {:?}", result),
        }
    }
}
