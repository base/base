//! Cumulative block and incremental flashblock assembly.

use std::{sync::Arc, time::Instant};

use alloy_consensus::{
    BlockBody, EMPTY_OMMER_ROOT_HASH, Header, TxReceipt, constants::EMPTY_WITHDRAWALS, proofs,
};
use alloy_eips::{Encodable2718, eip7685::EMPTY_REQUESTS_HASH, merge::BEACON_NONCE};
use alloy_primitives::{Address, B256, Bloom, U256, logs_bloom, map::foldhash::HashMap};
use base_common_chains::Upgrades;
use base_common_consensus::{BaseReceipt, BaseTransactionSigned};
use base_common_flashblocks::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, FlashblockId, FlashblocksPayloadV1,
    Metadata,
};
use base_execution_consensus::{calculate_receipt_root_no_memo, isthmus};
use base_execution_payload_builder::BaseBuiltPayload;
use base_execution_txpool::AccountStateDiff;
use reth_node_api::{BuiltPayloadExecutedBlock, PayloadBuilderError};
use reth_payload_primitives::PayloadAttributes;
use reth_primitives_traits::{Block, RecoveredBlock};
use reth_provider::{
    BlockExecutionOutput, BlockExecutionResult, HashedPostStateProvider, ProviderError,
    StateRootProvider, StorageRootProvider,
};
use reth_revm::{State, db::states::bundle_state::BundleRetention};
use reth_trie::{HashedPostState, updates::TrieUpdates};
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use tracing::{Level, debug, span, warn};

use crate::{BuilderMetrics, ExecutionInfo, flashblocks::BasePayloadBuilderCtx};

/// The complete payload, incremental flashblock update, and pool invalidation state produced by
/// one assembly pass.
#[derive(Debug)]
pub struct FlashblockAssembly {
    /// Complete cumulative payload containing every transaction executed in the block so far.
    pub payload: BaseBuiltPayload,
    /// Incremental flashblock update containing transactions since the previous assembly pass.
    pub flashblock: FlashblocksPayloadV1,
    /// Account changes used to invalidate transactions that conflict with newly published state.
    pub state_diff: Vec<AccountStateDiff>,
}

/// Metadata serialized into flashblock WebSocket messages.
#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize)]
pub struct FlashblocksMetadata {
    /// Metadata fields consumed by flashblock clients.
    #[serde(flatten)]
    pub metadata: Metadata,
    /// Receipts for transactions in this flashblock (removed in Base 1.0).
    pub receipts: Option<HashMap<B256, BaseReceipt>>,
    /// Changed account balances (removed in Base 1.0).
    pub new_account_balances: Option<HashMap<Address, U256>>,
}

/// Builds complete cumulative payloads and incremental flashblock updates from execution state.
#[derive(Debug, Default, Clone, Copy)]
pub struct FlashblockAssembler;

/// Controls whether assembly computes the canonical state root and trie updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateRootMode {
    /// Skip state-root computation for an intermediate flashblock.
    Skip,
    /// Compute the state root and trie updates for a final or synchronization payload.
    Compute,
}

/// Controls whether assembly populates the `base` field of the flashblock payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashblockBaseMode {
    /// Populate `base` for the first (fallback) flashblock of a block, which publishes it as-is.
    Include,
    /// Skip constructing `base` for intermediate flashblocks, whose callers discard it anyway.
    Omit,
}

impl FlashblockAssembler {
    /// Builds a cumulative payload and the delta since the previous assembly pass.
    ///
    /// Intermediate flashblocks may skip state-root calculation. Final payloads and no-pool
    /// synchronization payloads must request it. Similarly, only the first (fallback) flashblock
    /// of a block needs `flashblock.base` populated; later callers discard it immediately, so
    /// `base_mode` lets them skip constructing it. All fallible work runs before the bundle state
    /// is consumed, so a failure can never leave `state` with a taken bundle or an advanced delta
    /// cursor to reuse; the REVM transition state is restored on every exit path (including
    /// failures), so `state` is left safe to reuse. Execution information (`info`) is only mutated
    /// once assembly succeeds; on failure the delta cursor is left untouched.
    pub fn build<DB, P>(
        state: &mut State<DB>,
        ctx: &BasePayloadBuilderCtx,
        info: &mut ExecutionInfo,
        prev_flashblock_id: FlashblockId,
        state_root_mode: StateRootMode,
        base_mode: FlashblockBaseMode,
    ) -> Result<FlashblockAssembly, PayloadBuilderError>
    where
        DB: revm::Database<Error = ProviderError> + AsRef<P>,
        P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
    {
        // Reject a mismatched build context before touching `state`, so a cheap precondition
        // failure leaves the REVM state completely untouched.
        let block_number = ctx.block_number();
        let expected = ctx.parent().number + 1;
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

        // Snapshot the transition state before merging so it can be restored on every exit path
        // (see the end of this function), keeping `state` safe to reuse even if assembly fails.
        let untouched_transition_state = state.transition_state.clone();
        let state_merge_start_time = Instant::now();
        state.merge_transitions(BundleRetention::Reverts);
        let state_transition_merge_time = state_merge_start_time.elapsed();
        BuilderMetrics::state_transition_merge_duration().record(state_transition_merge_time);
        BuilderMetrics::state_transition_merge_gauge().set(state_transition_merge_time);

        // Run all fallible work inside this closure so `state.transition_state` is restored
        // afterwards whether assembly succeeds or bails out early via `?`.
        let assembly = (|| -> Result<FlashblockAssembly, PayloadBuilderError> {
            let receipts_root = calculate_receipt_root_no_memo(
                &info.receipts,
                &ctx.chain_spec,
                ctx.attributes().timestamp(),
            );
            let logs_bloom: Bloom = logs_bloom(info.receipts.iter().flat_map(|r| r.logs()));

            let state_root_start_time = Instant::now();
            let mut state_root = B256::ZERO;
            let mut trie_output = TrieUpdates::default();
            let mut hashed_state = HashedPostState::default();

            if state_root_mode == StateRootMode::Compute {
                let state_root_span = span!(
                    Level::INFO,
                    "calculate_state_root",
                    block_number = ctx.block_number(),
                    parent_hash = %ctx.parent().hash(),
                );
                let _state_root_span_guard = state_root_span.enter();

                let state_provider = state.database.as_ref();
                hashed_state = state_provider.hashed_post_state(&state.bundle_state);
                (state_root, trie_output) = state_provider
                    .state_root_with_updates(hashed_state.clone())
                    .inspect_err(|err| {
                        warn!(target: "payload_builder",
                            parent_header=%ctx.parent().hash(),
                            %err,
                            "failed to calculate state root for payload"
                        );
                    })?;
                let state_root_calculation_time = state_root_start_time.elapsed();
                BuilderMetrics::state_root_calculation_duration()
                    .record(state_root_calculation_time);
                BuilderMetrics::state_root_calculation_gauge().set(state_root_calculation_time);
            }

            let mut requests_hash = None;
            let withdrawals_root =
                if ctx.chain_spec.is_isthmus_active_at_timestamp(ctx.attributes().timestamp()) {
                    requests_hash = Some(EMPTY_REQUESTS_HASH);
                    Some(
                        isthmus::withdrawals_root(&state.bundle_state, state.database.as_ref())
                            .map_err(PayloadBuilderError::other)?,
                    )
                } else if ctx.chain_spec.is_canyon_active_at_timestamp(ctx.attributes().timestamp())
                {
                    Some(EMPTY_WITHDRAWALS)
                } else {
                    None
                };

            let transactions_root = proofs::calculate_transaction_root(&info.executed_transactions);
            let (excess_blob_gas, blob_gas_used) = ctx.blob_fields(info);
            let extra_data = ctx.extra_data()?;
            let parent_beacon_block_root =
                ctx.attributes().payload_attributes.parent_beacon_block_root.ok_or_else(|| {
                    PayloadBuilderError::Other(
                        eyre::eyre!("parent beacon block root not found").into(),
                    )
                })?;

            let header = Header {
                parent_hash: ctx.parent().hash(),
                ommers_hash: EMPTY_OMMER_ROOT_HASH,
                beneficiary: ctx.evm_env.block_env.beneficiary,
                state_root,
                transactions_root,
                receipts_root,
                withdrawals_root,
                logs_bloom,
                timestamp: ctx.attributes().payload_attributes.timestamp,
                mix_hash: ctx.attributes().payload_attributes.prev_randao,
                nonce: BEACON_NONCE.into(),
                base_fee_per_gas: Some(ctx.base_fee()),
                number: ctx.parent().number + 1,
                gas_limit: ctx.block_gas_limit(),
                difficulty: U256::ZERO,
                gas_used: info.cumulative_gas_used,
                extra_data: extra_data.clone(),
                parent_beacon_block_root: Some(parent_beacon_block_root),
                blob_gas_used,
                excess_blob_gas,
                requests_hash,
                block_access_list_hash: None,
                slot_number: ctx.attributes().payload_attributes.slot_number,
            };

            let block = alloy_consensus::Block::<BaseTransactionSigned>::new(
                header,
                BlockBody {
                    transactions: info.executed_transactions.clone(),
                    ommers: vec![],
                    withdrawals: ctx.withdrawals().cloned(),
                },
            );

            // Seal once to get the hash, then hand it to the recovered block below so the
            // latter never has to hash its (identical) header again on first access.
            let sealed_block = Arc::new(block.clone().seal_slow());
            let block_hash = sealed_block.hash();
            debug!(target: "payload_builder", ?sealed_block, "sealed built block");

            // Read the invalidation diff before take_bundle() empties the bundle state.
            let state_diff = AccountStateDiff::collect_for_intra_block(&state.bundle_state);
            let legacy_account_balances = (!ctx
                .chain_spec
                .is_azul_active_at_timestamp(ctx.attributes().timestamp()))
            .then(|| {
                state
                    .bundle_state
                    .state
                    .iter()
                    .filter_map(|(address, account)| {
                        account.info.as_ref().map(|info| (*address, info.balance))
                    })
                    .collect::<HashMap<Address, U256>>()
            });

            let delta_start = info.extra.last_flashblock_index;
            let new_transactions = &info.executed_transactions[delta_start..];
            let new_transactions_encoded =
                new_transactions.iter().map(|tx| tx.encoded_2718().into()).collect::<Vec<_>>();

            let metadata =
                if ctx.chain_spec.is_azul_active_at_timestamp(ctx.attributes().timestamp()) {
                    FlashblocksMetadata {
                        metadata: Metadata {
                            block_number: ctx.parent().number + 1,
                            prev_flashblock_id,
                        },
                        receipts: None,
                        new_account_balances: None,
                    }
                } else {
                    let receipts_with_hash = new_transactions
                        .iter()
                        .zip(info.receipts[delta_start..].iter())
                        .map(|(tx, receipt)| (tx.tx_hash(), receipt.clone()))
                        .collect::<HashMap<B256, BaseReceipt>>();
                    FlashblocksMetadata {
                        metadata: Metadata {
                            block_number: ctx.parent().number + 1,
                            prev_flashblock_id,
                        },
                        new_account_balances: legacy_account_balances,
                        receipts: Some(receipts_with_hash),
                    }
                };

            let base = match base_mode {
                FlashblockBaseMode::Include => Some(ExecutionPayloadBaseV1 {
                    parent_beacon_block_root,
                    parent_hash: ctx.parent().hash(),
                    fee_recipient: ctx.attributes().payload_attributes.suggested_fee_recipient,
                    prev_randao: ctx.attributes().payload_attributes.prev_randao,
                    block_number: ctx.parent().number + 1,
                    gas_limit: ctx.block_gas_limit(),
                    timestamp: ctx.attributes().payload_attributes.timestamp,
                    extra_data,
                    base_fee_per_gas: U256::from(ctx.base_fee()),
                }),
                FlashblockBaseMode::Omit => None,
            };

            let flashblock = FlashblocksPayloadV1 {
                payload_id: ctx.payload_id(),
                index: 0,
                base,
                diff: ExecutionPayloadFlashblockDeltaV1 {
                    state_root,
                    receipts_root,
                    logs_bloom,
                    gas_used: info.cumulative_gas_used,
                    block_hash,
                    transactions: new_transactions_encoded,
                    withdrawals: ctx.withdrawals().cloned().unwrap_or_default().to_vec(),
                    withdrawals_root: withdrawals_root.unwrap_or_default(),
                    blob_gas_used,
                },
                metadata: serde_json::to_value(&metadata).map_err(PayloadBuilderError::other)?,
            };

            // Every fallible operation above has now succeeded, so it's safe to consume the
            // bundle state and advance the delta cursor: no later `?` can leave `state` or
            // `info` half-updated.
            let recovered_block =
                RecoveredBlock::new(block, info.executed_senders.clone(), block_hash);
            let executed = BuiltPayloadExecutedBlock {
                recovered_block: Arc::new(recovered_block),
                execution_output: Arc::new(BlockExecutionOutput {
                    result: BlockExecutionResult {
                        receipts: info.receipts.clone(),
                        requests: vec![].into(),
                        gas_used: info.cumulative_gas_used,
                        blob_gas_used: 0,
                    },
                    state: state.take_bundle(),
                }),
                hashed_state: Arc::new(hashed_state),
                trie_updates: Arc::new(trie_output),
            };
            debug!(target: "payload_builder", message = "Executed block created");

            info.extra.last_flashblock_index = info.executed_transactions.len();

            Ok(FlashblockAssembly {
                payload: BaseBuiltPayload::new(
                    ctx.payload_id(),
                    sealed_block,
                    info.total_fees,
                    Some(executed),
                    None,
                ),
                flashblock,
                state_diff,
            })
        })();

        // Restore the transition state on every exit path so the failure contract holds and the
        // REVM state is left in the shape the next flashblock expects.
        state.transition_state = untouched_transition_state;
        assembly
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::{Header, Receipt, TxEip1559};
    use alloy_primitives::{Address, B256, Log, TxKind, U256, map::foldhash::HashMap};
    use base_common_consensus::{BaseReceipt, BaseTypedTransaction};
    use base_common_flashblocks::{FlashblockId, Metadata};
    use base_execution_chainspec::BaseChainSpec;
    use reth_chainspec::ChainSpec;
    use reth_primitives_traits::SealedHeader;
    use reth_provider::noop::NoopProvider;
    use reth_revm::{State, database::StateProviderDatabase};

    use super::{FlashblockAssembler, FlashblocksMetadata};
    use crate::{
        ExecutionInfo,
        flashblocks::BasePayloadBuilderCtx,
        test_utils::{generate_signer_from_seed, sign_base_tx},
    };

    fn minimal_chain_spec() -> Arc<BaseChainSpec> {
        let genesis = serde_json::from_value(serde_json::json!({
            "config": { "chainId": 901 },
            "gasLimit": "0x1C9C380",
            "timestamp": "0x0"
        }))
        .expect("valid genesis");
        let inner =
            ChainSpec::builder().chain(901.into()).genesis(genesis).cancun_activated().build();
        Arc::new(BaseChainSpec::from(inner))
    }

    fn genesis_header() -> Arc<SealedHeader> {
        Arc::new(SealedHeader::seal_slow(Header {
            gas_limit: 30_000_000,
            timestamp: 0,
            ..Default::default()
        }))
    }

    fn build(state_root_mode: super::StateRootMode) -> super::FlashblockAssembly {
        let ctx = BasePayloadBuilderCtx::for_test(minimal_chain_spec(), genesis_header());
        let db = StateProviderDatabase::new(NoopProvider::default());
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        FlashblockAssembler::build::<_, NoopProvider>(
            &mut state,
            &ctx,
            &mut ExecutionInfo::default(),
            FlashblockId::default(),
            state_root_mode,
            super::FlashblockBaseMode::Include,
        )
        .expect("assembly should succeed for an empty block")
    }

    #[test]
    fn builds_empty_block_without_state_root() {
        let assembly = build(super::StateRootMode::Skip);
        assert_eq!(assembly.payload.block().number, 1);
        assert_eq!(assembly.payload.block().gas_used, 0);
        assert_eq!(assembly.flashblock.diff.block_hash, assembly.payload.block().hash());
        assert!(assembly.state_diff.is_empty());
    }

    #[test]
    fn builds_empty_block_with_state_root() {
        let assembly = build(super::StateRootMode::Compute);
        assert_eq!(assembly.payload.block().state_root, B256::ZERO);
        assert_eq!(assembly.payload.block().number, 1);
        assert!(assembly.state_diff.is_empty());
    }

    #[test]
    fn omits_base_when_requested() {
        let ctx = BasePayloadBuilderCtx::for_test(minimal_chain_spec(), genesis_header());
        let db = StateProviderDatabase::new(NoopProvider::default());
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let assembly = FlashblockAssembler::build::<_, NoopProvider>(
            &mut state,
            &ctx,
            &mut ExecutionInfo::default(),
            FlashblockId::default(),
            super::StateRootMode::Skip,
            super::FlashblockBaseMode::Omit,
        )
        .expect("assembly should succeed for an empty block");
        assert!(assembly.flashblock.base.is_none());
    }

    #[test]
    fn includes_base_when_requested() {
        let assembly = build(super::StateRootMode::Skip);
        assert!(assembly.flashblock.base.is_some());
    }

    #[test]
    fn emits_incremental_deltas_for_cumulative_payloads() {
        let ctx = BasePayloadBuilderCtx::for_test(minimal_chain_spec(), genesis_header());
        let mut state = State::builder()
            .with_database(StateProviderDatabase::new(NoopProvider::default()))
            .with_bundle_update()
            .build();
        let signer = generate_signer_from_seed("cumulative-assembly-test");
        let mut info = ExecutionInfo::with_capacity(2);

        for nonce in 0..2 {
            let transaction = sign_base_tx(
                &signer,
                BaseTypedTransaction::Eip1559(TxEip1559 {
                    chain_id: 901,
                    nonce,
                    gas_limit: 21_000,
                    max_fee_per_gas: 1_000_000_000,
                    to: TxKind::Call(signer.address()),
                    value: U256::from(1u64),
                    ..Default::default()
                }),
            )
            .expect("sign transaction")
            .into_inner();
            info.executed_transactions.push(transaction);
            info.executed_senders.push(signer.address());
            info.receipts.push(BaseReceipt::Eip1559(Receipt {
                status: true.into(),
                cumulative_gas_used: (nonce + 1) * 21_000,
                logs: Vec::new(),
            }));
            info.cumulative_gas_used = (nonce + 1) * 21_000;

            let assembly = FlashblockAssembler::build::<_, NoopProvider>(
                &mut state,
                &ctx,
                &mut info,
                FlashblockId::default(),
                super::StateRootMode::Skip,
                super::FlashblockBaseMode::Include,
            )
            .expect("assembly should succeed");

            assert_eq!(assembly.payload.block().body().transactions.len(), nonce as usize + 1);
            assert_eq!(assembly.flashblock.diff.transactions.len(), 1);
            assert_eq!(assembly.flashblock.diff.gas_used, (nonce + 1) * 21_000);
            assert_eq!(assembly.flashblock.diff.block_hash, assembly.payload.block().hash());
        }
    }

    #[test]
    fn rejects_block_number_mismatch() {
        let mut ctx = BasePayloadBuilderCtx::for_test(minimal_chain_spec(), genesis_header());
        ctx.evm_env.block_env.number = U256::from(99);
        let db = StateProviderDatabase::new(NoopProvider::default());
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let error = FlashblockAssembler::build::<_, NoopProvider>(
            &mut state,
            &ctx,
            &mut ExecutionInfo::default(),
            FlashblockId::default(),
            super::StateRootMode::Skip,
            super::FlashblockBaseMode::Include,
        )
        .expect_err("assembly should reject a block number mismatch");
        assert!(error.to_string().contains("block number mismatch"));
    }

    #[test]
    fn rejects_missing_beacon_block_root() {
        let mut ctx = BasePayloadBuilderCtx::for_test(minimal_chain_spec(), genesis_header());
        ctx.config.attributes.payload_attributes.parent_beacon_block_root = None;
        let db = StateProviderDatabase::new(NoopProvider::default());
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let error = FlashblockAssembler::build::<_, NoopProvider>(
            &mut state,
            &ctx,
            &mut ExecutionInfo::default(),
            FlashblockId::default(),
            super::StateRootMode::Skip,
            super::FlashblockBaseMode::Include,
        )
        .expect_err("assembly should reject a missing beacon block root");
        assert!(error.to_string().contains("parent beacon block root not found"));
    }

    fn metadata() -> FlashblocksMetadata {
        let tx_hash = B256::from([0xAA; 32]);
        let address = Address::from([0xBB; 20]);
        let receipt = BaseReceipt::Eip1559(Receipt {
            status: true.into(),
            cumulative_gas_used: 21_000,
            logs: Vec::<Log>::new(),
        });
        FlashblocksMetadata {
            receipts: Some(HashMap::from_iter([(tx_hash, receipt)])),
            new_account_balances: Some(HashMap::from_iter([(
                address,
                U256::from(1_000_000_000_000_000_000u128),
            )])),
            metadata: Metadata {
                block_number: 42,
                prev_flashblock_id: FlashblockId { block_number: 41, index: 10 },
            },
        }
    }

    #[test]
    fn flashblocks_metadata_json_format_is_stable() {
        let json = serde_json::to_value(metadata()).unwrap();
        let object = json.as_object().unwrap();
        let mut keys: Vec<&String> = object.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["block_number", "new_account_balances", "prev_flashblock_id", "receipts"]
        );
        assert_eq!(object["block_number"], serde_json::json!(42));
        assert_eq!(object["prev_flashblock_id"], serde_json::json!("41-10"));
        let tx_key = format!("{:#x}", B256::from([0xAA; 32]));
        assert_eq!(object["receipts"][tx_key]["type"], "0x2");
        let address_key = format!("{:#x}", Address::from([0xBB; 20]));
        assert!(object["new_account_balances"].get(address_key).is_some());
    }

    #[test]
    fn client_metadata_deserializes_from_builder_metadata() {
        let client: Metadata = serde_json::from_value(serde_json::to_value(metadata()).unwrap())
            .expect("client metadata should parse builder metadata");
        assert_eq!(client.block_number, 42);
        assert_eq!(client.prev_flashblock_id, FlashblockId { block_number: 41, index: 10 });
    }

    #[test]
    fn client_metadata_deserializes_from_v0_4_1_format() {
        let metadata: Metadata =
            serde_json::from_value(serde_json::json!({"block_number": 123})).unwrap();
        assert_eq!(metadata.block_number, 123);
        assert_eq!(metadata.prev_flashblock_id, FlashblockId::default());
    }
}
