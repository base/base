//! System-style tests for consensus startup against pruned execution history.

use std::{sync::Arc, time::Duration};

use alloy_consensus::transaction::Recovered;
use alloy_eips::{BlockId, BlockNumHash, BlockNumberOrTag, eip2718::Encodable2718};
use alloy_json_rpc::ErrorPayload;
use alloy_primitives::{Address, B256, Bloom, Sealed, U256};
use alloy_rpc_types_engine::{ExecutionPayloadV1, PayloadStatus, PayloadStatusEnum};
use alloy_rpc_types_eth::{Block as RpcBlock, BlockTransactions};
use async_trait::async_trait;
use base_common_consensus::{BaseTxEnvelope, TxDeposit};
use base_common_genesis::{ChainGenesis, RollupConfig, SystemConfig};
use base_common_rpc_types::Transaction as BaseTransaction;
use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
use base_consensus_derive::Signal;
use base_consensus_engine::{
    Engine, EngineState, ForkchoiceCheckpointError, ForkchoiceCheckpointLabel,
    ForkchoiceCheckpointReader,
    test_utils::{MockEngineClient, MockL2BlockError, test_engine_client_builder},
};
use base_consensus_node::{
    DerivationClientResult, EngineActorRequest, EngineDerivationClient, EngineError,
    EngineProcessor, EngineProcessorOptions, NodeMode, NoopCheckpointWriter,
};
use base_protocol::{BlockInfo, L1BlockInfoBedrock, L2BlockInfo};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

const FINALIZED_BLOCK_NUMBER: u64 = 44_343_400;
const LATEST_BLOCK_NUMBER: u64 = 44_343_433;
const NEXT_UNSAFE_HASH_BYTE: u8 = 0x62;
const ENGINE_RESET_TIMEOUT: Duration = Duration::from_secs(5);
const PRUNED_HISTORY_UNAVAILABLE_CODE: i64 = 4444;
const PRUNED_HISTORY_UNAVAILABLE_MESSAGE: &str = "pruned history unavailable";

#[derive(Debug)]
struct NoopDerivationClient;

#[async_trait]
impl EngineDerivationClient for NoopDerivationClient {
    async fn notify_sync_completed(&self, _safe_head: L2BlockInfo) -> DerivationClientResult<()> {
        Ok(())
    }

    async fn send_new_engine_safe_head(
        &self,
        _safe_head: L2BlockInfo,
    ) -> DerivationClientResult<()> {
        Ok(())
    }

    async fn send_signal(&self, _signal: Signal) -> DerivationClientResult<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct StaticCheckpointReader {
    safe: L2BlockInfo,
    finalized: L2BlockInfo,
}

#[async_trait]
impl ForkchoiceCheckpointReader for StaticCheckpointReader {
    async fn checkpoint(
        &self,
        label: ForkchoiceCheckpointLabel,
    ) -> Result<Option<L2BlockInfo>, ForkchoiceCheckpointError> {
        Ok(Some(match label {
            ForkchoiceCheckpointLabel::Safe => self.safe,
            ForkchoiceCheckpointLabel::Finalized => self.finalized,
        }))
    }
}

/// A validator seeds its unsafe head from reth, accepts its first unsafe payload, then performs the
/// initial reset after EL sync completes. Some pruned reth nodes return `4444: pruned history
/// unavailable` for full historical labeled-block requests during that reset. The consensus
/// service should treat the labeled block as unavailable and continue startup instead of exiting
/// the engine processor.
///
/// The observed failure mode is an engine task exit with
/// `EngineReset(SyncStart(RpcError(...)))`.
#[tokio::test]
async fn validator_initial_reset_survives_pruned_history_unavailable_from_reth() {
    let mut processor = PrunedHistoryStartup::new().start_validator_processor();

    processor.wait_for_validator_bootstrap().await;
    processor.process_first_unsafe_payload().await;
    processor.assert_completes_initial_reset().await;
    processor.shutdown().await;
}

#[derive(Debug)]
struct PrunedHistoryStartup {
    rollup: Arc<RollupConfig>,
    client: Arc<MockEngineClient>,
    checkpointed_finalized_head: L2BlockInfo,
    reth_latest_head: L2BlockInfo,
    next_unsafe_hash: B256,
}

impl PrunedHistoryStartup {
    fn new() -> Self {
        let rollup = test_rollup_config();
        let genesis_block = genesis_l2_block();
        let finalized_block = pruned_l2_block(FINALIZED_BLOCK_NUMBER, rollup.genesis.l2.hash);
        let checkpointed_finalized_head = block_info_from_rpc_block(&finalized_block);
        let finalized_block_by_hash =
            full_l2_block_with_l1_info(FINALIZED_BLOCK_NUMBER, rollup.genesis.l2.hash);
        let latest_block = full_l2_block_with_l1_info(
            LATEST_BLOCK_NUMBER,
            checkpointed_finalized_head.block_info.hash,
        );
        let reth_latest_head = block_info_from_rpc_block(&latest_block);
        let next_unsafe_hash = B256::with_last_byte(NEXT_UNSAFE_HASH_BYTE);
        let client = Arc::new(
            test_engine_client_builder()
                .with_config(Arc::clone(&rollup))
                .with_block_info_by_tag(BlockNumberOrTag::Latest, reth_latest_head)
                .with_l2_block_error(
                    BlockId::Number(BlockNumberOrTag::Finalized),
                    MockL2BlockError::ErrorResp(pruned_history_unavailable_error()),
                )
                .with_l2_block(BlockId::Number(0.into()), genesis_block)
                .with_l2_block(
                    BlockId::from(checkpointed_finalized_head.block_info.hash),
                    finalized_block_by_hash,
                )
                .with_l2_block(BlockId::Number(BlockNumberOrTag::Latest), latest_block)
                .with_l1_block(BlockId::from(B256::ZERO), RpcBlock::default())
                .with_new_payload_v2_response(PayloadStatus {
                    status: PayloadStatusEnum::Valid,
                    latest_valid_hash: Some(next_unsafe_hash),
                })
                .with_fork_choice_updated_v3_response(valid_fcu())
                .build(),
        );

        Self { rollup, client, checkpointed_finalized_head, reth_latest_head, next_unsafe_hash }
    }

    fn start_validator_processor(self) -> RunningValidatorProcessor {
        let (state_tx, state_rx) = watch::channel(EngineState::default());
        let (queue_tx, _) = watch::channel(0usize);
        let engine = Engine::new(EngineState::default(), state_tx, queue_tx);
        let checkpoint_reader = Arc::new(StaticCheckpointReader {
            safe: self.checkpointed_finalized_head,
            finalized: self.checkpointed_finalized_head,
        });
        let processor = EngineProcessor::new_with_checkpoint(
            Arc::clone(&self.client),
            Arc::clone(&self.rollup),
            NoopDerivationClient,
            engine,
            validator_options(),
            checkpoint_reader,
            Arc::new(NoopCheckpointWriter),
        );
        let (request_tx, request_rx) = mpsc::channel(8);
        let handle = base_consensus_node::EngineRequestReceiver::start(processor, request_rx);

        RunningValidatorProcessor {
            state_rx,
            request_tx,
            handle,
            checkpointed_finalized_head: self.checkpointed_finalized_head,
            reth_latest_head: self.reth_latest_head,
            next_unsafe_hash: self.next_unsafe_hash,
        }
    }
}

struct RunningValidatorProcessor {
    state_rx: watch::Receiver<EngineState>,
    request_tx: mpsc::Sender<EngineActorRequest>,
    handle: JoinHandle<Result<(), EngineError>>,
    checkpointed_finalized_head: L2BlockInfo,
    reth_latest_head: L2BlockInfo,
    next_unsafe_hash: B256,
}

impl RunningValidatorProcessor {
    async fn wait_for_validator_bootstrap(&mut self) {
        self.state_rx
            .wait_for(|state| state.sync_state.unsafe_head() == self.reth_latest_head)
            .await
            .expect("state channel closed before validator bootstrap seeded latest head");
    }

    async fn process_first_unsafe_payload(&self) {
        self.request_tx
            .send(EngineActorRequest::ProcessUnsafeL2BlockRequest(Box::new(
                unsafe_payload_with_l1_info(
                    self.reth_latest_head.block_info.number + 1,
                    self.reth_latest_head.block_info.hash,
                    self.next_unsafe_hash,
                ),
            )))
            .await
            .expect("engine processor request channel closed");
    }

    async fn assert_completes_initial_reset(&mut self) {
        let wait_for_reset = self.state_rx.wait_for(|state| {
            state.el_sync_finished
                && state.sync_state.safe_head() == self.checkpointed_finalized_head
                && state.sync_state.finalized_head() == self.checkpointed_finalized_head
        });

        tokio::select! {
            result = &mut self.handle => {
                panic!(
                    "engine processor exited before initial reset completed after pruned history unavailable: {result:?}"
                );
            }
            result = tokio::time::timeout(ENGINE_RESET_TIMEOUT, wait_for_reset) => {
                match result {
                    Ok(Ok(_)) => {}
                    Ok(Err(err)) => {
                        if self.handle.is_finished() {
                            let result = (&mut self.handle).await;
                            panic!(
                                "engine processor exited before initial reset completed after pruned history unavailable: {result:?}"
                            );
                        }

                        panic!(
                            "engine state channel closed before initial reset after pruned history unavailable: {err:?}"
                        );
                    }
                    Err(_) => {
                        if self.handle.is_finished() {
                            let result = (&mut self.handle).await;
                            panic!(
                                "engine processor exited before initial reset completed after pruned history unavailable: {result:?}"
                            );
                        }

                        panic!(
                            "timed out waiting for initial reset after pruned history unavailable"
                        );
                    }
                }
            }
        }
    }

    async fn shutdown(self) {
        drop(self.request_tx);
        let result = self.handle.await.expect("engine processor task panicked");
        assert!(
            matches!(result, Err(EngineError::ChannelClosed)),
            "expected clean ChannelClosed shutdown after test, got {result:?}"
        );
    }
}

fn validator_options() -> EngineProcessorOptions {
    EngineProcessorOptions {
        node_mode: NodeMode::Validator,
        unsafe_head_tx: None,
        conductor: None,
        sequencer_stopped: false,
    }
}

fn test_rollup_config() -> Arc<RollupConfig> {
    Arc::new(RollupConfig {
        genesis: ChainGenesis {
            l2: BlockNumHash { number: 0, hash: genesis_hash() },
            l1: BlockNumHash { number: 0, hash: B256::ZERO },
            system_config: Some(SystemConfig::default()),
            ..Default::default()
        },
        ..Default::default()
    })
}

fn block_info_from_rpc_block(block: &RpcBlock<BaseTransaction>) -> L2BlockInfo {
    L2BlockInfo {
        block_info: BlockInfo {
            number: block.header.number,
            hash: block.header.hash_slow(),
            parent_hash: block.header.parent_hash,
            timestamp: block.header.timestamp,
        },
        ..Default::default()
    }
}

fn pruned_history_unavailable_error() -> ErrorPayload {
    ErrorPayload {
        code: PRUNED_HISTORY_UNAVAILABLE_CODE,
        message: PRUNED_HISTORY_UNAVAILABLE_MESSAGE.into(),
        data: None,
    }
}

const fn valid_fcu() -> alloy_rpc_types_engine::ForkchoiceUpdated {
    alloy_rpc_types_engine::ForkchoiceUpdated {
        payload_status: PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: None },
        payload_id: None,
    }
}

fn genesis_hash() -> B256 {
    genesis_l2_block().into_consensus().hash_slow()
}

fn genesis_l2_block() -> RpcBlock<BaseTransaction> {
    RpcBlock::<BaseTransaction>::default()
}

fn pruned_l2_block(number: u64, parent_hash: B256) -> RpcBlock<BaseTransaction> {
    let mut block = RpcBlock::<BaseTransaction>::default();
    block.header.inner.number = number;
    block.header.inner.parent_hash = parent_hash;
    block.header.inner.timestamp = number;
    block.transactions = BlockTransactions::Full(vec![]);
    block
}

fn l1_info_deposit_tx_bytes() -> Vec<u8> {
    BaseTxEnvelope::from(TxDeposit {
        input: L1BlockInfoBedrock::default().encode_calldata(),
        ..Default::default()
    })
    .encoded_2718()
}

fn l1_info_rpc_transaction(block_number: u64) -> BaseTransaction {
    let envelope = BaseTxEnvelope::Deposit(Sealed::new_unchecked(
        TxDeposit { input: L1BlockInfoBedrock::default().encode_calldata(), ..Default::default() },
        B256::ZERO,
    ));
    BaseTransaction {
        inner: alloy_rpc_types_eth::Transaction {
            inner: Recovered::new_unchecked(envelope, Address::ZERO),
            block_hash: None,
            block_number: Some(block_number),
            block_timestamp: None,
            effective_gas_price: Some(0),
            transaction_index: Some(0),
        },
        deposit_nonce: None,
        deposit_receipt_version: None,
    }
}

fn full_l2_block_with_l1_info(number: u64, parent_hash: B256) -> RpcBlock<BaseTransaction> {
    let mut block = RpcBlock::<BaseTransaction>::default();
    block.header.inner.number = number;
    block.header.inner.parent_hash = parent_hash;
    block.header.inner.timestamp = number;
    block.transactions = BlockTransactions::Full(vec![l1_info_rpc_transaction(number)]);
    block
}

fn unsafe_payload_with_l1_info(
    block_number: u64,
    parent_hash: B256,
    block_hash: B256,
) -> BaseExecutionPayloadEnvelope {
    BaseExecutionPayloadEnvelope {
        parent_beacon_block_root: None,
        execution_payload: BaseExecutionPayload::V1(ExecutionPayloadV1 {
            parent_hash,
            fee_recipient: Address::ZERO,
            state_root: B256::ZERO,
            receipts_root: B256::ZERO,
            logs_bloom: Bloom::ZERO,
            prev_randao: B256::ZERO,
            block_number,
            gas_limit: 30_000_000,
            gas_used: 0,
            timestamp: block_number,
            extra_data: Default::default(),
            base_fee_per_gas: U256::ZERO,
            block_hash,
            transactions: vec![l1_info_deposit_tx_bytes().into()],
        }),
    }
}
