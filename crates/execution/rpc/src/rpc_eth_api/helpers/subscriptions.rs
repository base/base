//! Streams subscriptions providers for `eth_subscribe`.

use base_common_types_chain::{BlockHeader, TransactionMeta, TxReceipt, transaction::TxHashRef};
use base_common_types_rpc::{
    BaseTransactionReceipt, Filter, Log, pubsub::TransactionReceiptsParams,
};
use base_execution_state_provider::CanonStateSubscriptions;
use futures::StreamExt;
use tracing::error;

use crate::{BaseEthApi, ConvertReceiptInput, logs_utils};

/// Provides streams subscriptions for `eth_subscribe`.
///
/// Override the default methods to inject additional data sources (e.g. flashblocks).
impl BaseEthApi {
    /// Returns a stream that yields matching logs from canonical chain updates.
    pub fn log_stream(&self, filter: Filter) -> impl futures::Stream<Item = Log> + Send + Unpin {
        let converter = self.converter();
        self.provider().canonical_state_stream().flat_map(move |canon_state| {
            let reverted_chains = canon_state.reverted();
            let committed_chain = canon_state.committed();
            let reverted = reverted_chains.iter().flat_map(|chain| {
                chain.blocks_and_receipts().map(|(block, receipts)| (block, receipts, true))
            });
            let committed = committed_chain
                .blocks_and_receipts()
                .map(|(block, receipts)| (block, receipts, false));
            let mut all_logs = Vec::new();

            for (block, receipts, removed) in reverted.chain(committed) {
                let result = logs_utils::matching_block_logs_with_tx_hashes(
                    converter,
                    &filter,
                    block.sealed_header(),
                    block
                        .transactions_recovered()
                        .zip(receipts.iter())
                        .map(|(tx, receipt)| (*tx.tx_hash(), receipt)),
                    removed,
                );
                match result {
                    Ok(logs) => all_logs.extend(logs),
                    Err(err) => {
                        error!(target = "rpc", %err, "Failed to convert logs");
                    }
                }
            }

            futures::stream::iter(all_logs)
        })
    }

    /// Returns a stream that yields matching transaction receipts from canonical chain updates.
    pub fn transaction_receipts_stream(
        &self,
        filter: TransactionReceiptsParams,
    ) -> impl futures::Stream<Item = Vec<BaseTransactionReceipt>> + Send + Unpin {
        let converter = self.converter();
        self.provider().canonical_state_stream().flat_map(move |new_chain| {
            let results: Vec<_> = new_chain
                .committed()
                .blocks_and_receipts()
                .filter_map(|(block, receipts)| {
                    let block_hash = block.hash();
                    let block_number = block.number();
                    let base_fee = block.base_fee_per_gas();
                    let excess_blob_gas = block.excess_blob_gas();
                    let timestamp = block.timestamp();

                    let mut gas_used: u64 = 0;
                    let mut next_log_index: usize = 0;

                    let inputs: Vec<_> = block
                        .transactions_recovered()
                        .zip(receipts.iter())
                        .enumerate()
                        .filter_map(|(idx, (tx, receipt))| {
                            let gas_used_before = gas_used;
                            let next_log_index_before = next_log_index;
                            let cumulative_gas_used = receipt.cumulative_gas_used();

                            gas_used = cumulative_gas_used;
                            next_log_index += receipt.logs().len();

                            let matches = match &filter.transaction_hashes {
                                Some(hashes) if !hashes.is_empty() => hashes.contains(tx.tx_hash()),
                                _ => true,
                            };

                            matches.then(|| ConvertReceiptInput {
                                tx,
                                gas_used: cumulative_gas_used - gas_used_before,
                                next_log_index: next_log_index_before,
                                meta: TransactionMeta {
                                    tx_hash: *tx.tx_hash(),
                                    index: idx as u64,
                                    block_hash,
                                    block_number,
                                    base_fee,
                                    excess_blob_gas,
                                    timestamp,
                                },
                                receipt: receipt.clone(),
                            })
                        })
                        .collect();

                    if inputs.is_empty() {
                        return None;
                    }

                    match converter.convert_receipts_with_block(inputs, block.sealed_block()) {
                        Ok(rpc_receipts) => Some(rpc_receipts),
                        Err(err) => {
                            error!(target = "rpc", %err, "Failed to convert receipts");
                            None
                        }
                    }
                })
                .collect();

            futures::stream::iter(results)
        })
    }
}
