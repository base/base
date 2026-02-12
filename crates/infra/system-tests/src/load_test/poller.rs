use super::tracker::TransactionTracker;
use alloy_network::ReceiptResponse;
use alloy_provider::{Provider, RootProvider};
use anyhow::Result;
use op_alloy_network::Optimism;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;

pub struct ReceiptPoller {
    sequencer: RootProvider<Optimism>,
    tracker: Arc<TransactionTracker>,
    timeout: Duration,
}

impl ReceiptPoller {
    pub fn new(
        sequencer: RootProvider<Optimism>,
        tracker: Arc<TransactionTracker>,
        timeout: Duration,
    ) -> Self {
        Self {
            sequencer,
            tracker,
            timeout,
        }
    }

    pub async fn run(self) -> Result<()> {
        let mut interval = tokio::time::interval(Duration::from_secs(2)); // Block time

        loop {
            interval.tick().await;

            let pending_txs = self.tracker.get_pending();

            for (tx_hash, send_time) in pending_txs {
                let elapsed = send_time.elapsed();

                if elapsed > self.timeout {
                    self.tracker.record_timeout(tx_hash);
                    debug!("Transaction timed out: {:?}", tx_hash);
                    continue;
                }

                match self.sequencer.get_transaction_receipt(tx_hash).await {
                    Ok(Some(receipt)) => {
                        // Verify transaction succeeded (status == true) and is in a block
                        if receipt.status() && receipt.block_number().is_some() {
                            self.tracker.record_included(tx_hash);
                            debug!("Transaction included and succeeded: {:?}", tx_hash);
                        } else if receipt.block_number().is_some() {
                            // Transaction was included but reverted
                            self.tracker.record_reverted(tx_hash);
                            debug!("Transaction included but reverted: {:?}", tx_hash);
                        }
                        // If no block_number yet, keep polling
                    }
                    Ok(None) => {
                        // Transaction not yet included, continue polling
                    }
                    Err(e) => {
                        debug!("Error fetching receipt for {:?}: {}", tx_hash, e);
                        // Don't mark as timeout, might be temporary RPC error
                    }
                }
            }

            // Exit when all transactions resolved and test completed
            if self.tracker.all_resolved() && self.tracker.is_test_completed() {
                break;
            }
        }

        Ok(())
    }
}
