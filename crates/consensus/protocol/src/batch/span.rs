//! Span Batch implementation for efficient multi-block L2 transaction batching.
//!
//! Span batches are an advanced batching format that can contain transactions for multiple
//! L2 blocks in a single compressed structure. This provides significant efficiency gains
//! over single batches by amortizing overhead across multiple blocks and enabling
//! sophisticated compression techniques.

use alloc::vec::Vec;

use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{FixedBytes, bytes};
use base_common_consensus::OpTxType;
use base_common_genesis::RollupConfig;
use tracing::{info, warn};

use crate::{
    BatchDropReason, BatchValidationProvider, BatchValidity, BlockInfo, L2BlockInfo, SingleBatch,
    SpanBatchBits, SpanBatchElement, SpanBatchError, SpanBatchPrefix, SpanBatchTransactions,
};

/// Container for the inputs required to build a span of L2 blocks in derived form.
///
/// A [`SpanBatch`] represents a compressed format for multiple L2 blocks that enables
/// significant space savings compared to individual single batches. The format uses
/// differential encoding, bit packing, and shared data structures to minimize the
/// L1 footprint while maintaining all necessary information for L2 block reconstruction.
///
/// # Compression Techniques
///
/// ## Temporal Compression
/// - **Relative timestamps**: Store timestamps relative to genesis to reduce size
/// - **Differential encoding**: Encode changes between consecutive blocks
/// - **Epoch sharing**: Multiple blocks can share the same L1 origin
///
/// ## Spatial Compression
/// - **Shared prefixes**: Common data shared across all blocks in span
/// - **Transaction batching**: Transactions grouped and compressed together
/// - **Bit packing**: Use minimal bits for frequently-used fields
///
/// # Format Structure
///
/// ```text
/// SpanBatch {
///   prefix: {
///     rel_timestamp,     // Relative to genesis
///     l1_origin_num,     // Final L1 block number
///     parent_check,      // First 20 bytes of parent hash
///     l1_origin_check,   // First 20 bytes of L1 origin hash
///   },
///   payload: {
///     block_count,       // Number of blocks in span
///     origin_bits,       // Bit array indicating L1 origin changes
///     block_tx_counts,   // Transaction count per block
///     txs,              // Compressed transaction data
///   }
/// }
/// ```
///
/// # Validation and Integrity
///
/// The span batch format includes several integrity checks:
/// - **Parent check**: Validates continuity with previous span
/// - **L1 origin check**: Ensures proper L1 origin binding
/// - **Transaction count validation**: Verifies transaction distribution
/// - **Bit field consistency**: Ensures origin bits match block count
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SpanBatch {
    /// First 20 bytes of the parent hash of the first block in the span.
    ///
    /// This field provides a collision-resistant check to ensure the span batch
    /// builds properly on the expected parent block. Using only 20 bytes saves
    /// space while maintaining strong integrity guarantees.
    pub parent_check: FixedBytes<20>,
    /// First 20 bytes of the L1 origin hash of the last block in the span.
    ///
    /// This field enables validation that the span batch references the correct
    /// L1 origin block, ensuring proper derivation ordering and preventing
    /// replay attacks across different L1 contexts.
    pub l1_origin_check: FixedBytes<20>,
    /// Genesis block timestamp for relative timestamp calculations.
    ///
    /// All timestamps in the span batch are stored relative to this genesis
    /// timestamp to minimize storage requirements. This enables efficient
    /// timestamp compression while maintaining full precision.
    pub genesis_timestamp: u64,
    /// Chain ID for transaction validation and network identification.
    ///
    /// Required for proper transaction signature validation and to prevent
    /// cross-chain replay attacks. All transactions in the span must be
    /// valid for this chain ID.
    pub chain_id: u64,
    /// Ordered list of block elements contained in this span.
    ///
    /// Each element represents the derived data for one L2 block, including
    /// timestamp, epoch information, and transaction references. The order
    /// must match the intended L2 block sequence.
    pub batches: Vec<SpanBatchElement>,
    /// Cached bit array indicating L1 origin changes between consecutive blocks.
    ///
    /// This compressed representation allows efficient encoding of which blocks
    /// in the span advance to a new L1 origin. Bit `i` is set if block `i+1`
    /// has a different L1 origin than block `i`.
    pub origin_bits: SpanBatchBits,
    /// Cached transaction count for each block in the span.
    ///
    /// Pre-computed transaction counts enable efficient random access to
    /// transactions for specific blocks without scanning the entire transaction
    /// list. Index `i` contains the transaction count for block `i`.
    pub block_tx_counts: Vec<u64>,
    /// Cached compressed transaction data for all blocks in the span.
    ///
    /// Contains all transactions from all blocks in a compressed format that
    /// enables efficient encoding and decoding. Transactions are grouped and
    /// compressed using span-specific techniques.
    pub txs: SpanBatchTransactions,
}

impl SpanBatch {
    /// Returns the starting timestamp for the first batch in the span.
    ///
    /// This is the absolute timestamp (not relative to genesis) of the first
    /// block in the span batch. Used for validation and block sequencing.
    ///
    /// # Panics
    /// Panics if the span batch contains no elements (`batches` is empty).
    /// This should never happen in valid span batches as they must contain
    /// at least one block.
    ///
    /// # Usage
    /// Typically used during span batch validation to ensure proper temporal
    /// ordering with respect to the parent block and L1 derivation window.
    pub fn starting_timestamp(&self) -> u64 {
        self.batches[0].timestamp
    }

    /// Returns the final timestamp for the last batch in the span.
    ///
    /// This is the absolute timestamp (not relative to genesis) of the last
    /// block in the span batch. Used for validation and determining the
    /// span's temporal range.
    ///
    /// # Panics
    /// Panics if the span batch contains no elements (`batches` is empty).
    /// This should never happen in valid span batches as they must contain
    /// at least one block.
    ///
    /// # Usage
    /// Used during validation to ensure the span doesn't exceed maximum
    /// temporal ranges and fits within L1 derivation windows.
    pub fn final_timestamp(&self) -> u64 {
        self.batches[self.batches.len() - 1].timestamp
    }

    /// Returns the L1 epoch number for the first batch in the span.
    ///
    /// The epoch number corresponds to the L1 block number that serves as
    /// the L1 origin for the first L2 block in this span. This establishes
    /// the L1 derivation context for the span.
    ///
    /// # Panics
    /// Panics if the span batch contains no elements (`batches` is empty).
    /// This should never happen in valid span batches as they must contain
    /// at least one block.
    ///
    /// # Usage
    /// Used during validation to ensure proper L1 origin sequencing and
    /// that the span begins with the expected L1 context.
    pub fn starting_epoch_num(&self) -> u64 {
        self.batches[0].epoch_num
    }

    /// Validates that the L1 origin hash matches the span's L1 origin check.
    ///
    /// Compares the first 20 bytes of the provided hash against the stored
    /// `l1_origin_check` field. This provides a collision-resistant validation
    /// that the span batch was derived from the expected L1 context.
    ///
    /// # Arguments
    /// * `hash` - The full 32-byte L1 origin hash to validate
    ///
    /// # Returns
    /// * `true` - If the first 20 bytes match the span's L1 origin check
    /// * `false` - If there's a mismatch, indicating invalid L1 context
    ///
    /// # Algorithm
    /// ```text
    /// l1_origin_check[0..20] == hash[0..20]
    /// ```
    ///
    /// Using only 20 bytes provides strong collision resistance (2^160 space)
    /// while saving 12 bytes per span compared to storing full hashes.
    pub fn check_origin_hash(&self, hash: FixedBytes<32>) -> bool {
        self.l1_origin_check == hash[..20]
    }

    /// Validates that the parent hash matches the span's parent check.
    ///
    /// Compares the first 20 bytes of the provided hash against the stored
    /// `parent_check` field. This ensures the span batch builds on the
    /// expected parent block, maintaining chain continuity.
    ///
    /// # Arguments
    /// * `hash` - The full 32-byte parent hash to validate
    ///
    /// # Returns
    /// * `true` - If the first 20 bytes match the span's parent check
    /// * `false` - If there's a mismatch, indicating discontinuity
    ///
    /// # Algorithm
    /// ```text
    /// parent_check[0..20] == hash[0..20]
    /// ```
    ///
    /// This validation is critical for maintaining the integrity of the L2
    /// chain and preventing insertion of span batches in wrong locations.
    pub fn check_parent_hash(&self, hash: FixedBytes<32>) -> bool {
        self.parent_check == hash[..20]
    }

    /// Accesses the nth element from the end of the batch list.
    ///
    /// This is a convenience method for accessing recent elements in the span,
    /// typically used during validation or processing algorithms that need to
    /// examine the latest elements in the sequence.
    ///
    /// # Arguments
    /// * `n` - Offset from the end (0 = last element, 1 = second-to-last, etc.)
    ///
    /// # Returns
    /// Reference to the nth element from the end of the batch list
    ///
    /// # Panics
    /// Panics if `n >= batches.len()`, i.e., if trying to access beyond
    /// the available elements.
    ///
    /// # Algorithm
    /// ```text
    /// index = batches.len() - 1 - n
    /// return &batches[index]
    /// ```
    fn peek(&self, n: usize) -> &SpanBatchElement {
        &self.batches[self.batches.len() - 1 - n]
    }

    /// Encodes this span directly into its raw wire format.
    ///
    /// [`crate::Batch::encode`] and protocol-level fixture builders call this
    /// after writing the batch type byte. Encoding from the derived caches avoids
    /// cloning them into an owned [`crate::RawSpanBatch`].
    ///
    /// Returns [`SpanBatchError::EmptySpanBatch`] when no block has been appended.
    pub fn encode(&self, w: &mut dyn bytes::BufMut) -> Result<(), SpanBatchError> {
        if self.batches.is_empty() {
            return Err(SpanBatchError::EmptySpanBatch);
        }
        let span_start = &self.batches[0];
        let span_end = &self.batches[self.batches.len() - 1];

        // The wire prefix anchors the span to its first L2 timestamp, parent,
        // and final L1 origin.
        SpanBatchPrefix {
            rel_timestamp: span_start.timestamp - self.genesis_timestamp,
            l1_origin_num: span_end.epoch_num,
            parent_check: self.parent_check,
            l1_origin_check: self.l1_origin_check,
        }
        .encode_prefix(w);

        // Encode directly from the derived caches instead of cloning them into
        // an intermediate RawSpanBatch payload.
        let block_count = self.batches.len();
        let mut varint_buf = [0u8; 10];
        w.put_slice(unsigned_varint::encode::u64(block_count as u64, &mut varint_buf));
        SpanBatchBits::encode(w, block_count, &self.origin_bits)?;
        for block_tx_count in &self.block_tx_counts {
            w.put_slice(unsigned_varint::encode::u64(*block_tx_count, &mut varint_buf));
        }
        self.txs.encode(w)
    }

    /// Converts all [`SpanBatchElement`]s after the L2 safe head to [`SingleBatch`]es. The
    /// resulting [`SingleBatch`]es do not contain a parent hash, as it is populated by the
    /// Batch Queue stage.
    pub fn get_singular_batches(
        &self,
        l1_origins: &[BlockInfo],
        l2_safe_head: L2BlockInfo,
    ) -> Result<Vec<SingleBatch>, SpanBatchError> {
        let mut single_batches = Vec::with_capacity(self.batches.len());
        let mut origin_index = 0;
        for batch in &self.batches {
            if batch.timestamp <= l2_safe_head.block_info.timestamp {
                continue;
            }
            // Overlapping span batches can pass the prefix checks but then the
            // first batch after the safe head has an outdated L1 origin.
            if batch.epoch_num < l2_safe_head.l1_origin.number {
                return Err(SpanBatchError::L1OriginBeforeSafeHead);
            }
            let origin_epoch_hash = l1_origins[origin_index..l1_origins.len()]
                .iter()
                .enumerate()
                .find(|(_, origin)| origin.number == batch.epoch_num)
                .map(|(i, origin)| {
                    origin_index += i;
                    origin.hash
                })
                .ok_or(SpanBatchError::MissingL1Origin)?;
            let single_batch = SingleBatch {
                epoch_num: batch.epoch_num,
                epoch_hash: origin_epoch_hash,
                timestamp: batch.timestamp,
                transactions: batch.transactions.clone(),
                ..Default::default()
            };
            single_batches.push(single_batch);
        }
        Ok(single_batches)
    }

    /// Append a [`SingleBatch`] to the [`SpanBatch`]. Updates the L1 origin check if need be.
    pub fn append_singular_batch(
        &mut self,
        singular_batch: SingleBatch,
        seq_num: u64,
    ) -> Result<(), SpanBatchError> {
        // If the new element is not ordered with respect to the last element, panic.
        if !self.batches.is_empty() && self.peek(0).timestamp > singular_batch.timestamp {
            panic!("Batch is not ordered");
        }

        let SingleBatch { epoch_hash, parent_hash, .. } = singular_batch;

        // Always append the new batch and set the L1 origin check.
        self.batches.push(singular_batch.into());
        // Always update the L1 origin check.
        self.l1_origin_check = epoch_hash[..20].try_into().expect("Sub-slice cannot fail");

        let epoch_bit = if self.batches.len() == 1 {
            // If there is only one batch, initialize the parent check and set the epoch bit based
            // on the sequence number.
            self.parent_check = parent_hash[..20].try_into().expect("Sub-slice cannot fail");
            seq_num == 0
        } else {
            // If there is more than one batch, set the epoch bit based on the last two batches.
            self.peek(1).epoch_num < self.peek(0).epoch_num
        };

        // Set the respective bit in the origin bits.
        self.origin_bits.set_bit(self.batches.len() - 1, epoch_bit);

        let new_txs = self.peek(0).transactions.clone();

        // Update the block tx counts cache with the latest batch's transaction count.
        self.block_tx_counts.push(new_txs.len() as u64);

        // Add the new transactions to the transaction cache.
        self.txs.add_txs(new_txs, self.chain_id)
    }

    /// Checks if the span batch is valid.
    pub async fn check_batch<BV: BatchValidationProvider>(
        &self,
        cfg: &RollupConfig,
        l1_blocks: &[BlockInfo],
        l2_safe_head: L2BlockInfo,
        inclusion_block: &BlockInfo,
        fetcher: &mut BV,
    ) -> BatchValidity {
        let (prefix_validity, parent_block) =
            self.check_batch_prefix(cfg, l1_blocks, l2_safe_head, inclusion_block, fetcher).await;
        if !matches!(prefix_validity, BatchValidity::Accept) {
            return prefix_validity;
        }

        let starting_epoch_num = self.starting_epoch_num();
        let parent_block = parent_block.expect("parent_block must be Some");

        let mut origin_index = 0;
        let mut origin_advanced = starting_epoch_num == parent_block.l1_origin.number + 1;
        for (i, batch) in self.batches.iter().enumerate() {
            let batch_timestamp = batch.timestamp;
            let batch_epoch = batch.epoch_num;

            if batch_timestamp <= l2_safe_head.block_info.timestamp {
                continue;
            }
            if batch_epoch < l2_safe_head.l1_origin.number {
                warn!(
                    target: "batch_span",
                    "batch L1 origin is before safe head L1 origin, batch_epoch: {}, safe_head_epoch: {:?}",
                    batch_epoch,
                    l2_safe_head.l1_origin
                );
                return BatchValidity::Drop(BatchDropReason::L1OriginBeforeSafeHead);
            }

            // Find the L1 origin for the batch.
            let Some((offset, l1_origin)) =
                l1_blocks[origin_index..].iter().enumerate().find(|(_, b)| batch_epoch == b.number)
            else {
                warn!(
                    target: "batch_span",
                    "unable to find L1 origin for batch, batch_epoch: {}, batch_timestamp: {}",
                    batch_epoch,
                    batch_timestamp
                );
                return BatchValidity::Drop(BatchDropReason::MissingL1Origin);
            };
            origin_index += offset;

            if i > 0 {
                origin_advanced = false;
                if batch_epoch > self.batches[i - 1].epoch_num {
                    origin_advanced = true;
                }
            }
            if batch_timestamp < l1_origin.timestamp {
                warn!(
                    target: "batch_span",
                    "batch timestamp is less than L1 origin timestamp, l2_timestamp: {}, l1_timestamp: {}, origin: {:?}",
                    batch_timestamp,
                    l1_origin.timestamp,
                    l1_origin.id()
                );
                return BatchValidity::Drop(BatchDropReason::TimestampBeforeL1Origin);
            }

            // Check if we ran out of sequencer time drift
            let max_drift = cfg.max_sequencer_drift(l1_origin.timestamp);
            if batch_timestamp > l1_origin.timestamp + max_drift {
                if batch.transactions.is_empty() {
                    // If the sequencer is co-operating by producing an empty batch,
                    // then allow the batch if it was the right thing to do to maintain the L2 time
                    // >= L1 time invariant. We only check batches that do not
                    // advance the epoch, to ensure epoch advancement regardless of time drift is
                    // allowed.
                    if !origin_advanced {
                        if origin_index + 1 >= l1_blocks.len() {
                            info!(
                                target: "batch_span",
                                "without the next L1 origin we cannot determine yet if this empty batch that exceeds the time drift is still valid"
                            );
                            return BatchValidity::Undecided;
                        }
                        if batch_timestamp >= l1_blocks[origin_index + 1].timestamp {
                            // check if the next L1 origin could have been adopted
                            warn!(
                                target: "batch_span",
                                "batch exceeded sequencer time drift without adopting next origin, and next L1 origin would have been valid"
                            );
                            return BatchValidity::Drop(
                                BatchDropReason::SequencerDriftNotAdoptedNextOrigin,
                            );
                        }
                        info!(
                            target: "batch_span",
                            "continuing with empty batch before late L1 block to preserve L2 time invariant"
                        );
                    }
                } else {
                    // If the sequencer is ignoring the time drift rule, then drop the batch and
                    // force an empty batch instead, as the sequencer is not
                    // allowed to include anything past this point without moving to the next epoch.
                    warn!(
                        target: "batch_span",
                        "batch exceeded sequencer time drift, sequencer must adopt new L1 origin to include transactions again, max_time: {}",
                        l1_origin.timestamp + max_drift
                    );
                    return BatchValidity::Drop(BatchDropReason::SequencerDriftExceeded);
                }
            }

            // Check that the transactions are not empty and do not contain any deposits.
            for (i, tx) in batch.transactions.iter().enumerate() {
                if tx.is_empty() {
                    warn!(
                        target: "batch_span",
                        "transaction data must not be empty, but found empty tx, tx_index: {}",
                        i
                    );
                    return BatchValidity::Drop(BatchDropReason::EmptyTransaction);
                }
                if tx.as_ref().first() == Some(&(OpTxType::Deposit as u8)) {
                    warn!(
                        target: "batch_span",
                        "sequencers may not embed any deposits into batch data, but found tx that has one, tx_index: {}",
                        i
                    );
                    return BatchValidity::Drop(BatchDropReason::DepositTransaction);
                }

                // If isthmus is not active yet and the transaction is a 7702, drop the batch.
                if !cfg.is_isthmus_active(batch.timestamp)
                    && tx.as_ref().first() == Some(&(OpTxType::Eip7702 as u8))
                {
                    warn!(target: "batch_span", tx_index = i, "EIP-7702 transactions are not supported pre-isthmus");
                    return BatchValidity::Drop(BatchDropReason::Eip7702PreIsthmus);
                }

                // If Everest is not active yet and the transaction is an 8130, drop the batch.
                if !cfg.is_everest_active(batch.timestamp)
                    && tx.as_ref().first() == Some(&(OpTxType::Eip8130 as u8))
                {
                    warn!(target: "batch_span", tx_index = i, "EIP-8130 transactions are not supported pre-Everest");
                    return BatchValidity::Drop(BatchDropReason::Eip8130PreEverest);
                }
            }
        }

        // Check overlapped blocks
        let parent_num = parent_block.block_info.number;
        let next_timestamp =
            cfg.l2_block_timestamp(l2_safe_head.block_info.number.saturating_add(1));
        if self.starting_timestamp() < next_timestamp {
            for i in 0..(l2_safe_head.block_info.number - parent_num) {
                let safe_block_num = parent_num + i + 1;
                let safe_block_payload = match fetcher.block_by_number(safe_block_num).await {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(target: "batch_span", block_number = safe_block_num, error = %e, "Failed to fetch block");
                        return BatchValidity::Undecided;
                    }
                };
                let safe_block = &safe_block_payload.body;
                let batch_txs = &self.batches[i as usize].transactions;
                // Execution payload has deposit txs but batch does not.
                let deposit_count: usize = safe_block
                    .transactions
                    .iter()
                    .map(|tx| if tx.is_deposit() { 1 } else { 0 })
                    .sum();
                if safe_block.transactions.len() - deposit_count != batch_txs.len() {
                    warn!(
                        target: "batch_span",
                        safe_block_txs = safe_block.transactions.len(),
                        batch_txs = batch_txs.len(),
                        "Overlapped block's tx count does not match"
                    );
                    return BatchValidity::Drop(BatchDropReason::OverlappedTxCountMismatch);
                }
                let batch_txs_len = batch_txs.len();
                #[allow(clippy::needless_range_loop)]
                for j in 0..batch_txs_len {
                    let mut buf = Vec::new();
                    safe_block.transactions[j + deposit_count].encode_2718(&mut buf);
                    if buf != batch_txs[j].0 {
                        warn!(target: "batch_span", "overlapped block's transaction does not match");
                        return BatchValidity::Drop(BatchDropReason::OverlappedTxMismatch);
                    }
                }
                let safe_block_ref = match L2BlockInfo::from_block_and_genesis(
                    &safe_block_payload,
                    &cfg.genesis,
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(
                            target: "batch_span",
                            "failed to extract L2BlockInfo from execution payload, hash: {}, err: {e}",
                            safe_block_payload.header.hash_slow()
                        );
                        return BatchValidity::Drop(BatchDropReason::L2BlockInfoExtractionFailed);
                    }
                };
                if safe_block_ref.l1_origin.number != self.batches[i as usize].epoch_num {
                    warn!(
                        "overlapped block's L1 origin number does not match {}, {}",
                        safe_block_ref.l1_origin.number, self.batches[i as usize].epoch_num
                    );
                    return BatchValidity::Drop(BatchDropReason::OverlappedL1OriginMismatch);
                }
            }
        }

        BatchValidity::Accept
    }

    /// Checks the validity of the batch's prefix.
    ///
    /// This function is used for post-Holocene upgrade to perform batch validation
    /// as each batch is being loaded in.
    pub async fn check_batch_prefix<BF: BatchValidationProvider>(
        &self,
        cfg: &RollupConfig,
        l1_origins: &[BlockInfo],
        l2_safe_head: L2BlockInfo,
        inclusion_block: &BlockInfo,
        fetcher: &mut BF,
    ) -> (BatchValidity, Option<L2BlockInfo>) {
        if l1_origins.is_empty() {
            warn!(target: "batch_span", "missing L1 block input, cannot proceed with batch checking");
            return (BatchValidity::Undecided, None);
        }
        if self.batches.is_empty() {
            warn!(target: "batch_span", "empty span batch, cannot proceed with batch checking");
            return (BatchValidity::Undecided, None);
        }
        let next = l2_safe_head.block_info.number + 1;
        if cfg.is_denim_active(cfg.l2_block_timestamp(next)) {
            warn!(target: "batch_span", next_block_number = next, "Dropping span batch after Denim activation");
            return (BatchValidity::Drop(BatchDropReason::SpanBatchPostDenim), None);
        }

        let epoch = l1_origins[0];
        let next_timestamp = cfg.l2_block_timestamp(next);

        let starting_epoch_num = self.starting_epoch_num();
        let mut batch_origin = epoch;
        if starting_epoch_num == batch_origin.number + 1 {
            if l1_origins.len() < 2 {
                info!(
                    target: "batch_span",
                    "eager batch wants to advance current epoch {:?}, but could not without more L1 blocks",
                    epoch.id()
                );
                return (BatchValidity::Undecided, None);
            }
            batch_origin = l1_origins[1];
        }
        if !cfg.is_delta_active(batch_origin.timestamp) {
            warn!(
                target: "batch_span",
                "received SpanBatch (id {:?}) with L1 origin (timestamp {}) before Delta upgrade",
                batch_origin.id(),
                batch_origin.timestamp
            );
            return (BatchValidity::Drop(BatchDropReason::SpanBatchPreDelta), None);
        }

        if self.starting_timestamp() > next_timestamp {
            warn!(
                target: "batch_span",
                "received out-of-order batch for future processing after next batch ({} > {})",
                self.starting_timestamp(),
                next_timestamp
            );

            // After holocene is activated, gaps are disallowed.
            if cfg.is_holocene_active(inclusion_block.timestamp) {
                return (BatchValidity::Drop(BatchDropReason::FutureTimestampHolocene), None);
            }
            return (BatchValidity::Future, None);
        }

        // Drop the batch if it has no new blocks after the safe head.
        if self.final_timestamp() < next_timestamp {
            warn!(target: "batch_span", "span batch has no new blocks after safe head");
            return if cfg.is_holocene_active(inclusion_block.timestamp) {
                (BatchValidity::Past, None)
            } else {
                (BatchValidity::Drop(BatchDropReason::SpanBatchNoNewBlocksPreHolocene), None)
            };
        }

        // Find the parent block of the span batch.
        // If the span batch does not overlap the current safe chain, parent block should be the L2
        // safe head.
        let mut parent_num = l2_safe_head.block_info.number;
        let mut parent_block = l2_safe_head;
        if self.starting_timestamp() < next_timestamp {
            if self.starting_timestamp() > l2_safe_head.block_info.timestamp {
                // Batch timestamp cannot be between safe head and next timestamp.
                warn!(target: "batch_span", "batch has misaligned timestamp, block time is too short");
                return (BatchValidity::Drop(BatchDropReason::SpanBatchMisalignedTimestamp), None);
            }
            if !(l2_safe_head.block_info.timestamp - self.starting_timestamp())
                .is_multiple_of(cfg.block_time)
            {
                warn!(target: "batch_span", "batch has misaligned timestamp, not overlapped exactly");
                return (BatchValidity::Drop(BatchDropReason::SpanBatchNotOverlappedExactly), None);
            }
            parent_num = l2_safe_head.block_info.number
                - (l2_safe_head.block_info.timestamp - self.starting_timestamp()) / cfg.block_time
                - 1;
            parent_block = match fetcher.l2_block_info_by_number(parent_num).await {
                Ok(block) => block,
                Err(e) => {
                    warn!(target: "batch_span", block_number = parent_num, error = %e, "Failed to fetch L2 block");
                    // Unable to validate the batch for now. Retry later.
                    return (BatchValidity::Undecided, None);
                }
            };
        }
        if !self.check_parent_hash(parent_block.block_info.hash) {
            warn!(
                target: "batch_span",
                expected_block_num = parent_num,
                received_block_num = parent_block.block_info.number,
                parent_hash = %parent_block.block_info.hash,
                parent_hash_check = %self.parent_check,
                "Parent block mismatch"
            );
            return (BatchValidity::Drop(BatchDropReason::ParentHashMismatch), None);
        }

        // Filter out batches that were included too late.
        if starting_epoch_num + cfg.seq_window_size < inclusion_block.number {
            warn!(target: "batch_span", "batch was included too late, sequence window expired");
            return (BatchValidity::Drop(BatchDropReason::IncludedTooLate), None);
        }

        // Check the L1 origin of the batch
        if starting_epoch_num > parent_block.l1_origin.number + 1 {
            warn!(
                target: "batch_span",
                starting_epoch = starting_epoch_num,
                next_epoch = parent_block.l1_origin.number + 1,
                "Batch is for future epoch too far ahead"
            );
            return (BatchValidity::Drop(BatchDropReason::EpochTooFarInFuture), None);
        }

        // Verify the l1 origin hash for each l1 block.
        // SAFETY: `Self::batches` is not empty, so the last element is guaranteed to exist.
        let end_epoch_num = self.batches.last().unwrap().epoch_num;
        let mut origin_checked = false;
        // l1Blocks is supplied from batch queue and its length is limited to SequencerWindowSize.
        for l1_block in l1_origins {
            if l1_block.number == end_epoch_num {
                if !self.check_origin_hash(l1_block.hash) {
                    warn!(
                        target: "batch_span",
                        l1_block_number = ?l1_block.number,
                        l1_block_hash = ?l1_block.hash,
                        l1_origin_number = ?starting_epoch_num,
                        l1_check_hash = ?self.l1_origin_check,
                        "batch is for different L1 chain, epoch hash does not match",
                    );
                    return (BatchValidity::Drop(BatchDropReason::EpochHashMismatch), None);
                }
                origin_checked = true;
                break;
            }
        }
        if !origin_checked {
            info!(target: "batch_span", "need more l1 blocks to check entire origins of span batch");
            return (BatchValidity::Undecided, None);
        }

        if starting_epoch_num < parent_block.l1_origin.number {
            warn!(target: "batch_span", minimum_epoch = ?parent_block.block_info.id(), "Dropped batch, epoch is too old");
            return (BatchValidity::Drop(BatchDropReason::EpochTooOld), None);
        }

        (BatchValidity::Accept, Some(parent_block))
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_consensus::{Header, SignableTransaction, TxLegacy, constants::EIP1559_TX_TYPE_ID};
    use alloy_eips::BlockNumHash;
    use alloy_primitives::{B256, Bytes, Signature, b256};
    use base_common_consensus::{BaseBlock, BaseTxEnvelope};
    use base_common_genesis::{BaseUpgradeConfig, ChainGenesis, UpgradeConfig};
    use rstest::rstest;
    use tracing::Level;

    use super::*;
    use crate::{RawSpanBatch, SpanBatchPayload, test_utils::TestBatchValidator};

    fn gen_l1_blocks(
        start_num: u64,
        count: u64,
        start_timestamp: u64,
        interval: u64,
    ) -> Vec<BlockInfo> {
        (0..count)
            .map(|i| BlockInfo {
                number: start_num + i,
                timestamp: start_timestamp + i * interval,
                hash: B256::left_padding_from(&i.to_be_bytes()),
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn test_starting_timestamp() {
        let timestamp = 10;
        let first_element = SpanBatchElement { timestamp, ..Default::default() };
        let batch =
            SpanBatch { batches: vec![first_element, Default::default()], ..Default::default() };
        assert_eq!(batch.starting_timestamp(), timestamp);
    }

    #[test]
    fn test_final_timestamp() {
        let timestamp = 10;
        let last_element = SpanBatchElement { timestamp, ..Default::default() };
        let batch =
            SpanBatch { batches: vec![Default::default(), last_element], ..Default::default() };
        assert_eq!(batch.final_timestamp(), timestamp);
    }

    #[test]
    fn test_starting_epoch_num() {
        let epoch_num = 10;
        let first_element = SpanBatchElement { epoch_num, ..Default::default() };
        let batch =
            SpanBatch { batches: vec![first_element, Default::default()], ..Default::default() };
        assert_eq!(batch.starting_epoch_num(), epoch_num);
    }

    #[test]
    fn test_peek() {
        let first_element = SpanBatchElement { epoch_num: 10, ..Default::default() };
        let second_element = SpanBatchElement { epoch_num: 11, ..Default::default() };
        let batch =
            SpanBatch { batches: vec![first_element, second_element], ..Default::default() };
        assert_eq!(batch.peek(0).epoch_num, 11);
        assert_eq!(batch.peek(1).epoch_num, 10);
    }

    #[test]
    fn test_append_empty_singular_batch() {
        let mut batch = SpanBatch::default();
        let singular_batch = SingleBatch {
            epoch_num: 10,
            epoch_hash: FixedBytes::from([17u8; 32]),
            parent_hash: FixedBytes::from([17u8; 32]),
            timestamp: 10,
            transactions: vec![],
        };
        assert!(batch.append_singular_batch(singular_batch, 0).is_ok());
        assert_eq!(batch.batches.len(), 1);
        assert_eq!(batch.origin_bits.get_bit(0), Some(1));
        assert_eq!(batch.block_tx_counts, vec![0]);
        assert_eq!(batch.txs.tx_data.len(), 0);

        // Add another empty single batch.
        let singular_batch = SingleBatch {
            epoch_num: 11,
            epoch_hash: FixedBytes::from([17u8; 32]),
            parent_hash: FixedBytes::from([17u8; 32]),
            timestamp: 20,
            transactions: vec![],
        };
        assert!(batch.append_singular_batch(singular_batch, 1).is_ok());
    }

    #[test]
    fn encode_matches_owned_raw_span_batch() {
        let tx =
            BaseTxEnvelope::Legacy(TxLegacy::default().into_signed(Signature::test_signature()))
                .encoded_2718()
                .into();
        let mut batch = SpanBatch { genesis_timestamp: 100, ..Default::default() };
        batch
            .append_singular_batch(
                SingleBatch {
                    epoch_num: 10,
                    epoch_hash: FixedBytes::repeat_byte(0x22),
                    parent_hash: FixedBytes::repeat_byte(0x11),
                    timestamp: 110,
                    transactions: vec![tx],
                },
                0,
            )
            .unwrap();
        batch
            .append_singular_batch(
                SingleBatch {
                    epoch_num: 11,
                    epoch_hash: FixedBytes::repeat_byte(0x33),
                    timestamp: 112,
                    ..Default::default()
                },
                0,
            )
            .unwrap();
        let mut expected = Vec::new();
        RawSpanBatch {
            prefix: SpanBatchPrefix {
                rel_timestamp: 10,
                l1_origin_num: 11,
                parent_check: batch.parent_check,
                l1_origin_check: batch.l1_origin_check,
            },
            payload: SpanBatchPayload {
                block_count: 2,
                origin_bits: batch.origin_bits.clone(),
                block_tx_counts: batch.block_tx_counts.clone(),
                txs: batch.txs.clone(),
            },
        }
        .encode(&mut expected)
        .unwrap();
        let mut actual = Vec::new();

        batch.encode(&mut actual).unwrap();

        assert_eq!(actual, expected);
    }

    #[test]
    fn encode_rejects_empty_span_batch() {
        assert_eq!(
            SpanBatch::default().encode(&mut Vec::new()),
            Err(SpanBatchError::EmptySpanBatch)
        );
    }

    #[test]
    fn test_check_origin_hash() {
        let l1_origin_check = FixedBytes::from([17u8; 20]);
        let hash = b256!("1111111111111111111111111111111111111111000000000000000000000000");
        let batch = SpanBatch { l1_origin_check, ..Default::default() };
        assert!(batch.check_origin_hash(hash));
        // This hash has 19 matching bytes, the other 13 are zeros.
        let invalid = b256!("1111111111111111111111111111111111111100000000000000000000000000");
        assert!(!batch.check_origin_hash(invalid));
    }

    #[test]
    fn test_check_parent_hash() {
        let parent_check = FixedBytes::from([17u8; 20]);
        let hash = b256!("1111111111111111111111111111111111111111000000000000000000000000");
        let batch = SpanBatch { parent_check, ..Default::default() };
        assert!(batch.check_parent_hash(hash));
        // This hash has 19 matching bytes, the other 13 are zeros.
        let invalid = b256!("1111111111111111111111111111111111111100000000000000000000000000");
        assert!(!batch.check_parent_hash(invalid));
    }

    #[tokio::test]
    async fn test_singular_batches_outdated_l1_origin() {
        let l1_block = BlockInfo { number: 10, timestamp: 20, ..Default::default() };
        let l1_blocks = vec![l1_block];
        let l2_safe_head = L2BlockInfo {
            block_info: BlockInfo { timestamp: 10, ..Default::default() },
            l1_origin: BlockNumHash { number: 10, ..Default::default() },
            ..Default::default()
        };
        let first = SpanBatchElement { epoch_num: 9, timestamp: 20, ..Default::default() };
        let second = SpanBatchElement { epoch_num: 10, timestamp: 30, ..Default::default() };
        let batch = SpanBatch { batches: vec![first, second], ..Default::default() };
        assert_eq!(
            batch.get_singular_batches(&l1_blocks, l2_safe_head),
            Err(SpanBatchError::L1OriginBeforeSafeHead),
        );
    }

    #[tokio::test]
    async fn test_singular_batches_missing_l1_origin() {
        let l1_block = BlockInfo { number: 10, timestamp: 20, ..Default::default() };
        let l1_blocks = vec![l1_block];
        let l2_safe_head = L2BlockInfo {
            block_info: BlockInfo { timestamp: 10, ..Default::default() },
            l1_origin: BlockNumHash { number: 10, ..Default::default() },
            ..Default::default()
        };
        let first = SpanBatchElement { epoch_num: 10, timestamp: 20, ..Default::default() };
        let second = SpanBatchElement { epoch_num: 11, timestamp: 30, ..Default::default() };
        let batch = SpanBatch { batches: vec![first, second], ..Default::default() };
        assert_eq!(
            batch.get_singular_batches(&l1_blocks, l2_safe_head),
            Err(SpanBatchError::MissingL1Origin),
        );
    }

    #[tokio::test]
    async fn test_check_batch_prefix_rejects_first_denim_block_with_nonzero_genesis() {
        let cfg = RollupConfig {
            block_time: 2,
            genesis: ChainGenesis {
                l2: BlockNumHash { number: 40, ..Default::default() },
                l2_time: 100,
                ..Default::default()
            },
            upgrades: UpgradeConfig {
                base: BaseUpgradeConfig { denim: Some(104), ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        };
        let batch = SpanBatch {
            batches: vec![SpanBatchElement { timestamp: 1, ..Default::default() }],
            ..Default::default()
        };
        let mut fetcher = TestBatchValidator::default();
        let l1_origins = [BlockInfo::default()];

        let pre_denim_parent = L2BlockInfo {
            block_info: BlockInfo { number: 40, timestamp: 100, ..Default::default() },
            ..Default::default()
        };
        assert_ne!(
            batch
                .check_batch_prefix(
                    &cfg,
                    &l1_origins,
                    pre_denim_parent,
                    &BlockInfo::default(),
                    &mut fetcher,
                )
                .await
                .0,
            BatchValidity::Drop(BatchDropReason::SpanBatchPostDenim)
        );

        let denim_parent = L2BlockInfo {
            block_info: BlockInfo { number: 41, timestamp: 102, ..Default::default() },
            ..Default::default()
        };
        assert_eq!(
            batch
                .check_batch_prefix(
                    &cfg,
                    &l1_origins,
                    denim_parent,
                    &BlockInfo::default(),
                    &mut fetcher,
                )
                .await
                .0,
            BatchValidity::Drop(BatchDropReason::SpanBatchPostDenim)
        );
    }

    const PARENT_HASH: B256 =
        b256!("1111111111111111111111111111111111111111000000000000000000000000");
    const L1_HASH: B256 = b256!("3333333333333333333333333333333333333333000000000000000000000000");
    const PAYLOAD_HASH: B256 =
        b256!("0e2ee9abe94ee4514b170d7039d8151a7469d434a8575dbab5bd4187a27732dd");

    /// The expected tracing output of a check: the level and the fragments of its only event.
    type ExpectedLog = Option<(Level, &'static [&'static str])>;

    /// Everything [`SpanBatch::check_batch`] takes. The default is a valid two-block span batch
    /// that overlaps the safe head, so each case overrides only what it exercises.
    struct Scenario {
        cfg: RollupConfig,
        l1_blocks: Vec<BlockInfo>,
        safe_head: L2BlockInfo,
        inclusion_block: BlockInfo,
        fetcher: TestBatchValidator,
        batch: SpanBatch,
    }

    impl Default for Scenario {
        fn default() -> Self {
            Self {
                cfg: cfg(),
                l1_blocks: vec![l1_block(11)],
                safe_head: L2BlockInfo {
                    block_info: BlockInfo {
                        number: 41,
                        timestamp: 10,
                        hash: PARENT_HASH,
                        ..Default::default()
                    },
                    l1_origin: BlockNumHash { number: 9, ..Default::default() },
                    ..Default::default()
                },
                inclusion_block: BlockInfo { number: 50, ..Default::default() },
                fetcher: TestBatchValidator { blocks: vec![l2_parent(9)], ..Default::default() },
                batch: batch(vec![element(10, 10), element(11, 20)]),
            }
        }
    }

    impl Scenario {
        /// Runs the check and asserts its validity and its single logged event.
        async fn assert_check(mut self, expected: BatchValidity, log: ExpectedLog) {
            let (trace_store, _guard) = crate::capture_traces!();
            let validity = self
                .batch
                .check_batch(
                    &self.cfg,
                    &self.l1_blocks,
                    self.safe_head,
                    &self.inclusion_block,
                    &mut self.fetcher,
                )
                .await;
            assert_eq!(validity, expected);
            let Some((level, fragments)) = log else {
                assert!(trace_store.is_empty());
                return;
            };
            let logs = trace_store.get_by_level(level);
            assert_eq!(logs.len(), 1, "{logs:?}");
            for fragment in fragments {
                assert!(logs[0].contains(fragment), "{fragment:?} not in {:?}", logs[0]);
            }
        }
    }

    fn cfg() -> RollupConfig {
        RollupConfig {
            seq_window_size: 100,
            upgrades: UpgradeConfig { delta_time: Some(0), ..Default::default() },
            block_time: 10,
            genesis: ChainGenesis {
                l2: BlockNumHash { number: 41, ..Default::default() },
                l2_time: 10,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn l1_block(number: u64) -> BlockInfo {
        BlockInfo { number, timestamp: 10, hash: L1_HASH, ..Default::default() }
    }

    /// The L2 block before the safe head, which the default batch overlaps.
    fn l2_parent(l1_origin: u64) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo {
                number: 40,
                timestamp: 10,
                hash: PARENT_HASH,
                ..Default::default()
            },
            l1_origin: BlockNumHash { number: l1_origin, ..Default::default() },
            ..Default::default()
        }
    }

    fn element(epoch_num: u64, timestamp: u64) -> SpanBatchElement {
        SpanBatchElement { epoch_num, timestamp, ..Default::default() }
    }

    fn element_with(epoch_num: u64, timestamp: u64, tx: &[u8]) -> SpanBatchElement {
        SpanBatchElement {
            transactions: vec![Bytes::copy_from_slice(tx)],
            ..element(epoch_num, timestamp)
        }
    }

    fn batch(batches: Vec<SpanBatchElement>) -> SpanBatch {
        SpanBatch {
            batches,
            parent_check: FixedBytes::<20>::from_slice(&PARENT_HASH[..20]),
            l1_origin_check: FixedBytes::<20>::from_slice(&L1_HASH[..20]),
            ..Default::default()
        }
    }

    fn empty_block(number: u64, transactions: Vec<BaseTxEnvelope>) -> BaseBlock {
        BaseBlock {
            header: Header { number, ..Default::default() },
            body: alloy_consensus::BlockBody {
                transactions,
                ommers: Vec::new(),
                withdrawals: None,
            },
        }
    }

    fn cfg_with_genesis_hash(l2: BlockNumHash, l1: BlockNumHash) -> RollupConfig {
        RollupConfig { genesis: ChainGenesis { l2, l1, ..cfg().genesis }, ..cfg() }
    }

    /// A scenario whose middle block carries `tx`, which sits between two valid filler txs.
    fn scenario_with_tx(upgrades: UpgradeConfig, tx: &[u8]) -> Scenario {
        let filler = [EIP1559_TX_TYPE_ID];
        let l1_blocks = gen_l1_blocks(9, 3, 0, 10);
        Scenario {
            cfg: RollupConfig { max_sequencer_drift: 100, upgrades, ..cfg() },
            batch: SpanBatch {
                l1_origin_check: FixedBytes::<20>::from_slice(&l1_blocks[0].hash[..20]),
                ..batch(vec![
                    element_with(10, 20, &filler),
                    element_with(10, 20, tx),
                    element_with(11, 20, &filler),
                ])
            },
            l1_blocks,
            ..Default::default()
        }
    }

    /// A scenario that overlaps the safe head and reaches the overlap checks.
    fn overlap_scenario(base_blocks: Vec<BaseBlock>) -> Scenario {
        let l1_blocks = gen_l1_blocks(9, 3, 0, 10);
        let l2 = |number, timestamp, l1_origin| L2BlockInfo {
            block_info: BlockInfo { number, timestamp, ..Default::default() },
            l1_origin: BlockNumHash { number: l1_origin, ..Default::default() },
            ..Default::default()
        };
        Scenario {
            cfg: RollupConfig {
                max_sequencer_drift: 1000,
                genesis: ChainGenesis {
                    l2: BlockNumHash { number: 10, ..Default::default() },
                    l2_time: 20,
                    ..Default::default()
                },
                ..cfg()
            },
            safe_head: l2(10, 20, 11),
            fetcher: TestBatchValidator {
                base_blocks,
                blocks: vec![l2(8, 0, 9), l2(9, 10, 10), l2(10, 20, 11)],
                ..Default::default()
            },
            batch: SpanBatch {
                batches: vec![element_with(10, 10, &[EIP1559_TX_TYPE_ID]), element(11, 60)],
                ..Default::default()
            },
            l1_blocks,
            ..Default::default()
        }
    }

    fn eip1559_tx() -> BaseTxEnvelope {
        BaseTxEnvelope::Eip1559(alloy_consensus::Signed::new_unchecked(
            alloy_consensus::TxEip1559 {
                gas_limit: 2,
                max_fee_per_gas: 1,
                max_priority_fee_per_gas: 1,
                to: alloy_primitives::TxKind::Create,
                value: alloy_primitives::U256::from(3),
                ..Default::default()
            },
            Signature::test_signature(),
            B256::ZERO,
        ))
    }

    fn drift_scenario(l1_blocks: Vec<BlockInfo>, elements: Vec<SpanBatchElement>) -> Scenario {
        Scenario {
            cfg: RollupConfig { max_sequencer_drift: 0, ..cfg() },
            batch: SpanBatch {
                l1_origin_check: FixedBytes::<20>::from_slice(&l1_blocks[1].hash[..20]),
                ..batch(elements)
            },
            l1_blocks,
            fetcher: TestBatchValidator::default(),
            ..Default::default()
        }
    }

    #[rstest]
    #[case::missing_l1_block_input(
        Scenario { l1_blocks: vec![], batch: SpanBatch::default(), ..Default::default() },
        BatchValidity::Undecided,
        Some((Level::WARN, &["missing L1 block input, cannot proceed with batch checking"][..]))
    )]
    #[case::empty_batch(
        Scenario { batch: SpanBatch::default(), ..Default::default() },
        BatchValidity::Undecided,
        Some((Level::WARN, &["empty span batch, cannot proceed with batch checking"][..]))
    )]
    #[case::eager_block_missing_origins(
        Scenario {
            l1_blocks: vec![l1_block(9)],
            batch: batch(vec![element(10, 10)]),
            ..Default::default()
        },
        BatchValidity::Undecided,
        Some((
            Level::INFO,
            &["eager batch wants to advance current epoch", "but could not without more L1 blocks"][..],
        ))
    )]
    #[case::delta_inactive(
        Scenario {
            cfg: RollupConfig {
                upgrades: UpgradeConfig { delta_time: Some(10), ..Default::default() },
                ..cfg()
            },
            l1_blocks: vec![BlockInfo { number: 10, timestamp: 9, ..Default::default() }],
            batch: batch(vec![element(10, 10)]),
            ..Default::default()
        },
        BatchValidity::Drop(BatchDropReason::SpanBatchPreDelta),
        Some((Level::WARN, &["before Delta upgrade"][..]))
    )]
    #[case::out_of_order(
        Scenario { l1_blocks: vec![l1_block(10)], batch: batch(vec![element(10, 21)]), ..Default::default() },
        BatchValidity::Future,
        Some((
            Level::WARN,
            &["received out-of-order batch for future processing after next batch (21 > 20)"][..],
        ))
    )]
    #[case::no_new_blocks(
        Scenario { l1_blocks: vec![l1_block(10)], batch: batch(vec![element(10, 10)]), ..Default::default() },
        BatchValidity::Drop(BatchDropReason::SpanBatchNoNewBlocksPreHolocene),
        Some((Level::WARN, &["span batch has no new blocks after safe head"][..]))
    )]
    #[case::overlap_tx_count_mismatch(
        overlap_scenario(vec![empty_block(9, vec![])]),
        BatchValidity::Drop(BatchDropReason::OverlappedTxCountMismatch),
        Some((
            Level::WARN,
            &["Overlapped block's tx count does not match, safe_block_txs: 0, batch_txs: 1"][..],
        ))
    )]
    #[case::overlap_tx_mismatch(
        overlap_scenario(vec![empty_block(9, vec![eip1559_tx()])]),
        BatchValidity::Drop(BatchDropReason::OverlappedTxMismatch),
        Some((Level::WARN, &["overlapped block's transaction does not match"][..]))
    )]
    #[case::timestamp_before_l1_origin(
        Scenario {
            cfg: RollupConfig {
                genesis: ChainGenesis { l2_time: 10, ..Default::default() },
                ..cfg()
            },
            l1_blocks: vec![BlockInfo { number: 10, timestamp: 20, ..Default::default() }],
            safe_head: L2BlockInfo {
                block_info: BlockInfo { timestamp: 10, ..Default::default() },
                l1_origin: BlockNumHash { number: 10, ..Default::default() },
                ..Default::default()
            },
            batch: SpanBatch {
                batches: vec![element(10, 20), element(10, 19), element(10, 30)],
                ..Default::default()
            },
            ..Default::default()
        },
        BatchValidity::Drop(BatchDropReason::TimestampBeforeL1Origin),
        Some((
            Level::WARN,
            &["batch timestamp is less than L1 origin timestamp, l2_timestamp: 19, l1_timestamp: 20"][..],
        ))
    )]
    #[case::misaligned_timestamp(
        Scenario {
            l1_blocks: vec![l1_block(10)],
            batch: batch(vec![element(10, 11), element(11, 21)]),
            ..Default::default()
        },
        BatchValidity::Drop(BatchDropReason::SpanBatchMisalignedTimestamp),
        Some((Level::WARN, &["batch has misaligned timestamp, block time is too short"][..]))
    )]
    #[case::misaligned_without_overlap(
        Scenario {
            cfg: RollupConfig {
                genesis: ChainGenesis::default(),
                ..cfg()
            },
            l1_blocks: vec![l1_block(10)],
            safe_head: L2BlockInfo {
                block_info: BlockInfo { timestamp: 10, ..Default::default() },
                ..Default::default()
            },
            batch: batch(vec![element(10, 8), element(11, 20)]),
            ..Default::default()
        },
        BatchValidity::Drop(BatchDropReason::SpanBatchNotOverlappedExactly),
        Some((Level::WARN, &["batch has misaligned timestamp, not overlapped exactly"][..]))
    )]
    #[case::failed_to_fetch_l2_block(
        Scenario {
            l1_blocks: vec![l1_block(10)],
            fetcher: TestBatchValidator::default(),
            ..Default::default()
        },
        BatchValidity::Undecided,
        Some((Level::WARN, &["Failed to fetch L2 block", "Block not found"][..]))
    )]
    #[case::parent_hash_mismatch(
        Scenario {
            l1_blocks: vec![l1_block(10)],
            fetcher: TestBatchValidator {
                blocks: vec![L2BlockInfo {
                    block_info: BlockInfo { number: 41, timestamp: 10, ..Default::default() },
                    ..Default::default()
                }],
                short_circuit: true,
                ..Default::default()
            },
            ..Default::default()
        },
        BatchValidity::Drop(BatchDropReason::ParentHashMismatch),
        Some((
            Level::WARN,
            &["Parent block mismatch", "expected_block_num", "40", "received_block_num", "41"][..],
        ))
    )]
    #[case::sequence_window_expired(
        Scenario {
            cfg: RollupConfig { seq_window_size: 0, ..cfg() },
            l1_blocks: vec![l1_block(10)],
            ..Default::default()
        },
        BatchValidity::Drop(BatchDropReason::IncludedTooLate),
        Some((Level::WARN, &["batch was included too late, sequence window expired"][..]))
    )]
    #[case::starting_epoch_too_far_ahead(
        Scenario {
            l1_blocks: vec![l1_block(10)],
            safe_head: L2BlockInfo {
                l1_origin: BlockNumHash { number: 8, ..Default::default() },
                ..Scenario::default().safe_head
            },
            fetcher: TestBatchValidator { blocks: vec![l2_parent(8)], ..Default::default() },
            ..Default::default()
        },
        BatchValidity::Drop(BatchDropReason::EpochTooFarInFuture),
        Some((
            Level::WARN,
            &["Batch is for future epoch too far ahead", "starting_epoch", "next_epoch"][..],
        ))
    )]
    #[case::epoch_hash_mismatch(
        Scenario {
            batch: SpanBatch {
                l1_origin_check: Default::default(),
                ..batch(vec![element(10, 10), element(11, 20)])
            },
            ..Default::default()
        },
        BatchValidity::Drop(BatchDropReason::EpochHashMismatch),
        Some((
            Level::WARN,
            &["batch is for different L1 chain, epoch hash does not match"][..],
        ))
    )]
    #[case::need_more_l1_blocks(
        Scenario { l1_blocks: vec![l1_block(10)], ..Default::default() },
        BatchValidity::Undecided,
        Some((Level::INFO, &["need more l1 blocks to check entire origins of span batch"][..]))
    )]
    #[case::epoch_too_old(
        Scenario {
            safe_head: L2BlockInfo {
                l1_origin: BlockNumHash { number: 13, ..Default::default() },
                ..Scenario::default().safe_head
            },
            fetcher: TestBatchValidator { blocks: vec![l2_parent(14)], ..Default::default() },
            ..Default::default()
        },
        BatchValidity::Drop(BatchDropReason::EpochTooOld),
        Some((Level::WARN, &["Dropped batch, epoch is too old", "minimum_epoch"][..]))
    )]
    #[case::sequencer_drift_not_adopted_next_origin(
        drift_scenario(
            gen_l1_blocks(9, 3, 10, 0),
            vec![element(10, 20), element(10, 30), element(11, 40)],
        ),
        BatchValidity::Drop(BatchDropReason::SequencerDriftNotAdoptedNextOrigin),
        Some((
            Level::WARN,
            &["batch exceeded sequencer time drift without adopting next origin, and next L1 origin would have been valid"][..],
        ))
    )]
    #[case::continuing_with_empty_batch(
        // The second batch is valid without advancing the origin because its timestamp (30) is
        // before L1 block 11 (40). The third batch then adopts L1 block 11 at timestamp 40.
        drift_scenario(
            gen_l1_blocks(10, 2, 10, 30),
            vec![element(10, 20), element(10, 30), element(11, 40)],
        ),
        BatchValidity::Accept,
        Some((
            Level::INFO,
            &["continuing with empty batch before late L1 block to preserve L2 time invariant"][..],
        ))
    )]
    #[case::sequencer_drift_exceeded(
        drift_scenario(
            gen_l1_blocks(9, 3, 10, 0),
            vec![
                element_with(10, 20, &[]),
                element_with(10, 20, &[]),
                element_with(11, 20, &[]),
            ],
        ),
        BatchValidity::Drop(BatchDropReason::SequencerDriftExceeded),
        Some((
            Level::WARN,
            &["batch exceeded sequencer time drift, sequencer must adopt new L1 origin to include transactions again, max_time: 10"][..],
        ))
    )]
    #[case::empty_tx(
        scenario_with_tx(UpgradeConfig { delta_time: Some(0), ..Default::default() }, &[]),
        BatchValidity::Drop(BatchDropReason::EmptyTransaction),
        Some((Level::WARN, &["transaction data must not be empty, but found empty tx"][..]))
    )]
    #[case::deposit_tx(
        scenario_with_tx(UpgradeConfig { delta_time: Some(0), ..Default::default() }, &[OpTxType::Deposit as u8]),
        BatchValidity::Drop(BatchDropReason::DepositTransaction),
        Some((
            Level::WARN,
            &["sequencers may not embed any deposits into batch data, but found tx that has one, tx_index: 0"][..],
        ))
    )]
    #[case::eip7702_pre_isthmus(
        scenario_with_tx(UpgradeConfig { delta_time: Some(0), ..Default::default() }, &[alloy_consensus::TxType::Eip7702 as u8]),
        BatchValidity::Drop(BatchDropReason::Eip7702PreIsthmus),
        Some((Level::WARN, &["EIP-7702 transactions are not supported pre-isthmus", "tx_index"][..]))
    )]
    #[case::eip8130_pre_everest(
        scenario_with_tx(UpgradeConfig { delta_time: Some(0), ..Default::default() }, &[OpTxType::Eip8130 as u8]),
        BatchValidity::Drop(BatchDropReason::Eip8130PreEverest),
        Some((Level::WARN, &["EIP-8130 transactions are not supported pre-Everest", "tx_index"][..]))
    )]
    #[case::eip8130_span_post_everest(
        // Everest enables EIP-8130 after Denim has disabled span batches, so EIP-8130
        // transactions must use singular batches.
        scenario_with_tx(
            UpgradeConfig {
                delta_time: Some(0),
                base: BaseUpgradeConfig { denim: Some(0), everest: Some(0), ..Default::default() },
                ..Default::default()
            },
            &[OpTxType::Eip8130 as u8],
        ),
        BatchValidity::Drop(BatchDropReason::SpanBatchPostDenim),
        Some((Level::WARN, &["Dropping span batch after Denim activation"][..]))
    )]
    #[case::failed_to_fetch_payload(
        Scenario::default(),
        BatchValidity::Undecided,
        Some((Level::WARN, &["Failed to fetch block", "L2 Block not found"][..]))
    )]
    #[case::failed_to_extract_l2_block_info(
        Scenario {
            fetcher: TestBatchValidator {
                base_blocks: vec![empty_block(41, vec![])],
                ..Scenario::default().fetcher
            },
            ..Default::default()
        },
        BatchValidity::Drop(BatchDropReason::L2BlockInfoExtractionFailed),
        Some((
            Level::WARN,
            &["failed to extract L2BlockInfo from execution payload", "0e2ee9abe94ee4514b170d7039d8151a7469d434a8575dbab5bd4187a27732dd"][..],
        ))
    )]
    #[case::overlapped_origin_mismatch(
        Scenario {
            cfg: cfg_with_genesis_hash(
                BlockNumHash { number: 41, hash: PAYLOAD_HASH },
                BlockNumHash::default(),
            ),
            fetcher: TestBatchValidator {
                base_blocks: vec![empty_block(41, vec![])],
                ..Scenario::default().fetcher
            },
            ..Default::default()
        },
        BatchValidity::Drop(BatchDropReason::OverlappedL1OriginMismatch),
        Some((Level::WARN, &["overlapped block's L1 origin number does not match"][..]))
    )]
    #[case::overlapped_origin_outdated(
        Scenario {
            cfg: RollupConfig {
                genesis: ChainGenesis {
                    l2: BlockNumHash { number: 40, hash: PARENT_HASH },
                    l2_time: 0,
                    ..Default::default()
                },
                ..cfg()
            },
            l1_blocks: vec![BlockInfo { number: 10, timestamp: 5, hash: L1_HASH, ..Default::default() }],
            safe_head: L2BlockInfo {
                l1_origin: BlockNumHash { number: 10, hash: L1_HASH },
                ..Scenario::default().safe_head
            },
            fetcher: TestBatchValidator {
                blocks: vec![L2BlockInfo { l1_origin: BlockNumHash { number: 9, ..Default::default() }, ..l2_parent(9) }],
                base_blocks: vec![empty_block(41, vec![])],
                ..Default::default()
            },
            batch: batch(vec![element(9, 10), element(9, 20), element(10, 30)]),
            ..Default::default()
        },
        BatchValidity::Drop(BatchDropReason::L1OriginBeforeSafeHead),
        Some((Level::WARN, &["batch L1 origin is before safe head L1 origin"][..]))
    )]
    #[case::valid_with_genesis_epoch(
        Scenario {
            cfg: cfg_with_genesis_hash(
                BlockNumHash { number: 41, hash: PAYLOAD_HASH },
                BlockNumHash { number: 10, ..Default::default() },
            ),
            fetcher: TestBatchValidator {
                base_blocks: vec![empty_block(41, vec![])],
                ..Scenario::default().fetcher
            },
            ..Default::default()
        },
        BatchValidity::Accept,
        None
    )]
    #[tokio::test]
    async fn test_check_batch(
        #[case] scenario: Scenario,
        #[case] expected: BatchValidity,
        #[case] log: ExpectedLog,
    ) {
        scenario.assert_check(expected, log).await;
    }
}
