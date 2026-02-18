/// Block assembly from EVM execution state for flashblock payload building.
use std::sync::Arc;

use alloy_consensus::{
    BlockBody, EMPTY_OMMER_ROOT_HASH, Header, constants::EMPTY_WITHDRAWALS, proofs,
};
use alloy_eips::{Encodable2718, eip7685::EMPTY_REQUESTS_HASH, merge::BEACON_NONCE};
use alloy_evm::Database;
use alloy_primitives::{B256, Bloom, U256};
use base_access_lists::FlashblockAccessList;
use base_primitives::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, FlashblocksPayloadV1,
};
use either::Either;
use reth_node_api::{Block, BuiltPayloadExecutedBlock, PayloadBuilderError};
use reth_optimism_consensus::{calculate_receipt_root_no_memo_optimism, isthmus};
use reth_optimism_node::OpBuiltPayload;
use reth_optimism_primitives::{OpReceipt, OpTransactionSigned};
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_primitives_traits::RecoveredBlock;
use reth_provider::{
    BlockExecutionOutput, BlockExecutionResult, ExecutionOutcome, HashedPostStateProvider,
    ProviderError, StateRootProvider, StorageRootProvider,
};
use reth_revm::{State, db::states::bundle_state::BundleRetention};
use reth_trie::{HashedPostState, updates::TrieUpdates};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::{ExecutionInfo, context::OpPayloadBuilderCtx};

/// Computed block roots derived from the execution outcome.
///
/// Contains the state-independent roots that can be computed from the execution
/// outcome alone, without access to the underlying state database.
#[derive(Debug)]
pub struct BlockRoots {
    /// Receipts root for this block.
    pub receipts_root: B256,
    /// Logs bloom for this block.
    pub logs_bloom: Bloom,
}

#[derive(Debug, Serialize, Deserialize)]
struct FlashblocksMetadata {
    block_number: u64,
    access_list: FlashblockAccessList,
}

/// Assembles a sealed block and flashblock delta from execution state.
///
/// Encapsulates the block-sealing logic previously in the `build_block` free
/// function. Sub-steps are exposed as individually testable methods — in
/// particular, [`validate_block_number`], [`compute_block_roots`],
/// [`build_header`], and [`build_flashblock_delta`] can all be exercised
/// without touching the EVM or a live database.
///
/// [`validate_block_number`]: BlockAssembler::validate_block_number
/// [`compute_block_roots`]: BlockAssembler::compute_block_roots
/// [`build_header`]: BlockAssembler::build_header
/// [`build_flashblock_delta`]: BlockAssembler::build_flashblock_delta
#[derive(Debug)]
pub struct BlockAssembler<'a> {
    ctx: &'a OpPayloadBuilderCtx,
    info: &'a mut ExecutionInfo,
    calculate_state_root: bool,
}

impl<'a> BlockAssembler<'a> {
    /// Creates a new [`BlockAssembler`].
    pub fn new(
        ctx: &'a OpPayloadBuilderCtx,
        info: &'a mut ExecutionInfo,
        calculate_state_root: bool,
    ) -> Self {
        Self { ctx, info, calculate_state_root }
    }

    /// Validates that the block number in the build context matches the expected value.
    ///
    /// Returns an error if `ctx.block_number() != ctx.parent().number + 1`.
    pub fn validate_block_number(&self) -> Result<(), PayloadBuilderError> {
        let block_number = self.ctx.block_number();
        let expected = self.ctx.parent().number + 1;
        if block_number != expected {
            return Err(PayloadBuilderError::Other(
                eyre::eyre!(
                    "build context block number mismatch: expected {}, got {}",
                    expected,
                    block_number
                )
                .into(),
            ));
        }
        Ok(())
    }

    /// Computes the receipts root and logs bloom from the execution outcome.
    ///
    /// These are state-independent and can be tested with any known execution outcome.
    pub fn compute_block_roots(
        &self,
        execution_outcome: &ExecutionOutcome<OpReceipt>,
    ) -> Result<BlockRoots, PayloadBuilderError> {
        let block_number = self.ctx.block_number();

        let receipts_root = execution_outcome
            .generic_receipts_root_slow(block_number, |receipts| {
                calculate_receipt_root_no_memo_optimism(
                    receipts,
                    &self.ctx.chain_spec,
                    self.ctx.attributes().timestamp(),
                )
            })
            .ok_or_else(|| {
                PayloadBuilderError::Other(
                    eyre::eyre!(
                        "receipts and block number not in range, block number {}",
                        block_number
                    )
                    .into(),
                )
            })?;

        let logs_bloom = execution_outcome.block_logs_bloom(block_number).ok_or_else(|| {
            PayloadBuilderError::Other(
                eyre::eyre!(
                    "logs bloom and block number not in range, block number {}",
                    block_number
                )
                .into(),
            )
        })?;

        Ok(BlockRoots { receipts_root, logs_bloom })
    }

    /// Builds the block header from all assembled pieces.
    ///
    /// Takes computed roots and state root as parameters so each combination
    /// of hardfork-dependent fields can be tested independently.
    pub fn build_header(
        &self,
        roots: &BlockRoots,
        state_root: B256,
        withdrawals_root: Option<B256>,
        requests_hash: Option<B256>,
    ) -> Result<Header, PayloadBuilderError> {
        let transactions_root =
            proofs::calculate_transaction_root(&self.info.executed_transactions);

        let (excess_blob_gas, blob_gas_used) = self.ctx.blob_fields(self.info);
        let extra_data = self.ctx.extra_data()?;

        Ok(Header {
            parent_hash: self.ctx.parent().hash(),
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: self.ctx.evm_env.block_env.beneficiary,
            state_root,
            transactions_root,
            receipts_root: roots.receipts_root,
            withdrawals_root,
            logs_bloom: roots.logs_bloom,
            timestamp: self.ctx.attributes().payload_attributes.timestamp,
            mix_hash: self.ctx.attributes().payload_attributes.prev_randao,
            nonce: BEACON_NONCE.into(),
            base_fee_per_gas: Some(self.ctx.base_fee()),
            number: self.ctx.parent().number + 1,
            gas_limit: self.ctx.block_gas_limit(),
            difficulty: U256::ZERO,
            gas_used: self.info.cumulative_gas_used,
            extra_data,
            parent_beacon_block_root: self.ctx
                .attributes()
                .payload_attributes
                .parent_beacon_block_root,
            blob_gas_used,
            excess_blob_gas,
            requests_hash,
        })
    }

    /// Extracts the new transactions since the last flashblock, builds the flashblock
    /// access list, and constructs the [`FlashblocksPayloadV1`] delta.
    ///
    /// Updates `info.extra.last_flashblock_index` to the current transaction count.
    pub fn build_flashblock_delta(
        &mut self,
        block_hash: B256,
        state_root: B256,
        roots: &BlockRoots,
        withdrawals_root: Option<B256>,
    ) -> Result<FlashblocksPayloadV1, PayloadBuilderError> {
        let min_tx_index = self.info.extra.last_flashblock_index as u64;
        let new_transactions =
            self.info.executed_transactions[self.info.extra.last_flashblock_index..].to_vec();

        let new_transactions_encoded =
            new_transactions.into_iter().map(|tx| tx.encoded_2718().into()).collect::<Vec<_>>();

        let max_tx_index = min_tx_index + new_transactions_encoded.len() as u64;
        self.info.extra.last_flashblock_index = self.info.executed_transactions.len();

        let fal_builder = std::mem::take(&mut self.info.extra.access_list_builder);
        let access_list = fal_builder.build(min_tx_index, max_tx_index);

        let metadata = FlashblocksMetadata {
            block_number: self.ctx.parent().number + 1,
            access_list,
        };

        let (_, blob_gas_used) = self.ctx.blob_fields(self.info);

        Ok(FlashblocksPayloadV1 {
            payload_id: self.ctx.payload_id(),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: self
                    .ctx
                    .attributes()
                    .payload_attributes
                    .parent_beacon_block_root
                    .ok_or_else(|| {
                        PayloadBuilderError::Other(
                            eyre::eyre!("parent beacon block root not found").into(),
                        )
                    })?,
                parent_hash: self.ctx.parent().hash(),
                fee_recipient: self.ctx.attributes().suggested_fee_recipient(),
                prev_randao: self.ctx.attributes().payload_attributes.prev_randao,
                block_number: self.ctx.parent().number + 1,
                gas_limit: self.ctx.block_gas_limit(),
                timestamp: self.ctx.attributes().payload_attributes.timestamp,
                extra_data: self.ctx.extra_data()?,
                base_fee_per_gas: self.ctx.base_fee().try_into().unwrap(),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root,
                receipts_root: roots.receipts_root,
                logs_bloom: roots.logs_bloom,
                gas_used: self.info.cumulative_gas_used,
                block_hash,
                transactions: new_transactions_encoded,
                withdrawals: self
                    .ctx
                    .withdrawals()
                    .cloned()
                    .unwrap_or_default()
                    .to_vec(),
                withdrawals_root: withdrawals_root.unwrap_or_default(),
                blob_gas_used,
            },
            metadata: serde_json::to_value(&metadata).unwrap_or_default(),
        })
    }

    /// Assembles a sealed block and flashblock delta from the current EVM state.
    ///
    /// Merges state transitions, validates block number, computes roots, and
    /// produces both the [`OpBuiltPayload`] for the p2p layer and the
    /// [`FlashblocksPayloadV1`] for flashblock subscribers. After assembly,
    /// restores the transition state so subsequent flashblocks accumulate
    /// correctly.
    pub fn assemble<DB, P>(
        mut self,
        state: &mut State<DB>,
    ) -> Result<(OpBuiltPayload, FlashblocksPayloadV1), PayloadBuilderError>
    where
        DB: Database<Error = ProviderError> + AsRef<P> + revm::Database,
        P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
    {
        let untouched_transition_state = state.transition_state.clone();

        let state_merge_start_time = std::time::Instant::now();
        state.merge_transitions(BundleRetention::Reverts);
        let state_transition_merge_time = state_merge_start_time.elapsed();
        self.ctx.metrics.state_transition_merge_duration.record(state_transition_merge_time);
        self.ctx.metrics.state_transition_merge_gauge.set(state_transition_merge_time);

        self.validate_block_number()?;

        let block_number = self.ctx.block_number();

        let execution_outcome = ExecutionOutcome::new(
            state.bundle_state.clone(),
            vec![self.info.receipts.clone()],
            block_number,
            vec![],
        );

        let roots = self.compute_block_roots(&execution_outcome)?;

        let state_root_start_time = std::time::Instant::now();
        let mut state_root = B256::ZERO;
        let mut trie_output = TrieUpdates::default();
        let mut hashed_state = HashedPostState::default();

        if self.calculate_state_root {
            let state_provider = state.database.as_ref();
            hashed_state = state_provider.hashed_post_state(execution_outcome.state());
            (state_root, trie_output) = state
                .database
                .as_ref()
                .state_root_with_updates(hashed_state.clone())
                .inspect_err(|err| {
                    warn!(
                        target: "payload_builder",
                        parent_header = %self.ctx.parent().hash(),
                        %err,
                        "failed to calculate state root for payload"
                    );
                })?;
            let state_root_calculation_time = state_root_start_time.elapsed();
            self.ctx
                .metrics
                .state_root_calculation_duration
                .record(state_root_calculation_time);
            self.ctx.metrics.state_root_calculation_gauge.set(state_root_calculation_time);
        }

        let requests_hash = self.ctx.is_isthmus_active().then_some(EMPTY_REQUESTS_HASH);

        let withdrawals_root = if self.ctx.is_isthmus_active() {
            Some(
                isthmus::withdrawals_root(execution_outcome.state(), state.database.as_ref())
                    .map_err(PayloadBuilderError::other)?,
            )
        } else if self.ctx.is_canyon_active() {
            Some(EMPTY_WITHDRAWALS)
        } else {
            None
        };

        let header = self.build_header(&roots, state_root, withdrawals_root, requests_hash)?;

        let block = alloy_consensus::Block::<OpTransactionSigned>::new(
            header,
            BlockBody {
                transactions: self.info.executed_transactions.clone(),
                ommers: vec![],
                withdrawals: self.ctx.withdrawals().cloned(),
            },
        );

        let recovered_block =
            RecoveredBlock::new_unhashed(block.clone(), self.info.executed_senders.clone());

        let executed = BuiltPayloadExecutedBlock {
            recovered_block: Arc::new(recovered_block),
            execution_output: Arc::new(BlockExecutionOutput {
                result: BlockExecutionResult {
                    receipts: self.info.receipts.clone(),
                    requests: vec![].into(),
                    gas_used: self.info.cumulative_gas_used,
                    blob_gas_used: 0,
                },
                state: state.take_bundle(),
            }),
            hashed_state: Either::Left(Arc::new(hashed_state)),
            trie_updates: Either::Left(Arc::new(trie_output)),
        };

        debug!(target: "payload_builder", message = "Executed block created");

        let sealed_block = Arc::new(block.seal_slow());
        debug!(target: "payload_builder", ?sealed_block, "sealed built block");

        let block_hash = sealed_block.hash();

        let fb_payload = self.build_flashblock_delta(block_hash, state_root, &roots, withdrawals_root)?;

        state.transition_state = untouched_transition_state;

        let total_fees = self.info.total_fees;
        let payload_id = self.ctx.payload_id();
        Ok((OpBuiltPayload::new(payload_id, sealed_block, total_fees, Some(executed)), fb_payload))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bloom;
    use reth_provider::ExecutionOutcome;
    use reth_revm::db::states::bundle_state::BundleState;

    use super::*;
    use crate::{ExecutionInfo, context::OpPayloadBuilderCtx};

    fn empty_outcome(block_number: u64) -> ExecutionOutcome<OpReceipt> {
        ExecutionOutcome::new(BundleState::default(), vec![vec![]], block_number, vec![])
    }

    // ── validate_block_number ────────────────────────────────────────────────

    #[test]
    fn test_validate_block_number_ok() {
        // parent = 1, block = 2 → 2 == 1 + 1, should pass
        let ctx = OpPayloadBuilderCtx::test_ctx_with(1, 2);
        let mut info = ExecutionInfo::default();
        let assembler = BlockAssembler::new(&ctx, &mut info, false);
        assembler.validate_block_number().unwrap();
    }

    #[test]
    fn test_validate_block_number_mismatch() {
        // parent = 5, block = 2 → 2 ≠ 5 + 1, should fail
        let ctx = OpPayloadBuilderCtx::test_ctx_with(5, 2);
        let mut info = ExecutionInfo::default();
        let assembler = BlockAssembler::new(&ctx, &mut info, false);
        assert!(assembler.validate_block_number().is_err());
    }

    // ── compute_block_roots ──────────────────────────────────────────────────

    #[test]
    fn test_compute_block_roots_empty_block() {
        let ctx = OpPayloadBuilderCtx::test_ctx_with(1, 2);
        let mut info = ExecutionInfo::default();
        let assembler = BlockAssembler::new(&ctx, &mut info, false);

        let outcome = empty_outcome(2);
        let roots = assembler.compute_block_roots(&outcome).unwrap();

        // Empty block has empty logs bloom.
        assert_eq!(roots.logs_bloom, Bloom::ZERO);
        // Receipts root for an empty list is a known non-zero trie hash.
        assert_ne!(roots.receipts_root, B256::ZERO);
    }

    #[test]
    fn test_compute_block_roots_block_number_out_of_range() {
        // outcome is for block 99 but ctx.block_number() == 2 → should error
        let ctx = OpPayloadBuilderCtx::test_ctx_with(1, 2);
        let mut info = ExecutionInfo::default();
        let assembler = BlockAssembler::new(&ctx, &mut info, false);

        let outcome = empty_outcome(99);
        assert!(assembler.compute_block_roots(&outcome).is_err());
    }

    // ── build_header ─────────────────────────────────────────────────────────

    #[test]
    fn test_build_header_basic_fields() {
        let ctx = OpPayloadBuilderCtx::test_ctx_with(1, 2);
        let mut info = ExecutionInfo::default();
        let assembler = BlockAssembler::new(&ctx, &mut info, false);

        let roots = BlockRoots { receipts_root: B256::ZERO, logs_bloom: Bloom::ZERO };
        let header = assembler.build_header(&roots, B256::ZERO, None, None).unwrap();

        assert_eq!(header.number, 2);
        assert_eq!(header.parent_hash, B256::ZERO);
        assert_eq!(header.receipts_root, B256::ZERO);
        assert_eq!(header.logs_bloom, Bloom::ZERO);
        // Pre-Ecotone: no blob fields.
        assert!(header.blob_gas_used.is_none());
        assert!(header.excess_blob_gas.is_none());
        // Optional fields match what was passed in.
        assert!(header.withdrawals_root.is_none());
        assert!(header.requests_hash.is_none());
    }

    #[test]
    fn test_build_header_with_withdrawals_and_requests() {
        let ctx = OpPayloadBuilderCtx::test_ctx_with(1, 2);
        let mut info = ExecutionInfo::default();
        let assembler = BlockAssembler::new(&ctx, &mut info, false);

        let roots = BlockRoots { receipts_root: B256::ZERO, logs_bloom: Bloom::ZERO };
        let wr = B256::from([1u8; 32]);
        let rh = B256::from([2u8; 32]);
        let header = assembler.build_header(&roots, B256::ZERO, Some(wr), Some(rh)).unwrap();

        assert_eq!(header.withdrawals_root, Some(wr));
        assert_eq!(header.requests_hash, Some(rh));
    }
}
