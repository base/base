//! In-process engine client for action tests that performs real EVM execution.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use alloy_consensus::{Header, Sealed};
use alloy_eips::{BlockId, eip1898::BlockNumberOrTag, eip2718::Decodable2718};
use alloy_network::{Ethereum, Network};
use alloy_primitives::{Address, B256, BlockHash, StorageKey};
use alloy_provider::{EthGetBlock, ProviderCall, RpcWithBlock};
use alloy_rpc_types_engine::{
    ClientVersionV1, ExecutionPayloadBodiesV1, ExecutionPayloadEnvelopeV2, ExecutionPayloadInputV2,
    ExecutionPayloadV1, ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadId,
    PayloadStatus, PayloadStatusEnum,
};
use alloy_rpc_types_eth::{
    Block, BlockTransactions, EIP1186AccountProofResponse, Transaction as EthTransaction,
};
use alloy_transport::{TransportError, TransportErrorKind, TransportResult};
use alloy_transport_http::Http;
use async_trait::async_trait;
use base_alloy_consensus::OpTxEnvelope;
use base_alloy_network::Base;
use base_alloy_provider::OpEngineApi;
use base_alloy_rpc_types::Transaction as OpTransaction;
use base_alloy_rpc_types_engine::{
    OpExecutionPayloadEnvelopeV3, OpExecutionPayloadEnvelopeV4, OpExecutionPayloadEnvelopeV5,
    OpExecutionPayloadV4, OpPayloadAttributes,
};
use base_consensus_engine::{EngineClient, EngineClientError, HyperAuthClient};
use base_consensus_genesis::RollupConfig;
use base_protocol::{BlockInfo, L2BlockInfo};

use crate::{SharedBlockHashRegistry, SharedL1Chain, StatefulL2Executor};

/// Mutable state owned by [`ActionEngineClient`], protected by a `Mutex` so
/// the client can implement the `&self` methods required by [`EngineClient`].
#[derive(Debug)]
struct ActionEngineClientInner {
    executor: StatefulL2Executor,
    canonical_head: L2BlockInfo,
    executed_headers: HashMap<u64, Header>,
}

/// An in-process engine client for action tests that performs real EVM execution.
///
/// `ActionEngineClient` implements [`EngineClient`] using the same EVM execution
/// path as [`L2Sequencer`]. When `new_payload_vX` is called with a derived payload,
/// every transaction is executed through revm, the resulting MPT state root is
/// computed, and — when the paired `block_registry` contains a sequencer-computed
/// state root for that block — the two roots are asserted equal. A mismatch panics
/// with a diagnostic showing both roots and the block number.
///
/// `fork_choice_updated_vX` updates the internal canonical head to the supplied
/// `head_block_hash`. Query methods (`get_l2_block`, `l2_block_by_label`) serve
/// from the registry of executed headers. `get_l1_block` delegates to the
/// `SharedL1Chain` passed at construction. `get_proof` returns an empty proof
/// (not required for derivation testing).
///
/// [`L2Sequencer`]: crate::L2Sequencer
#[derive(Clone, Debug)]
pub struct ActionEngineClient {
    inner: Arc<Mutex<ActionEngineClientInner>>,
    rollup_config: Arc<RollupConfig>,
    block_registry: SharedBlockHashRegistry,
    l1_chain: SharedL1Chain,
}

impl ActionEngineClient {
    /// Create a new `ActionEngineClient` paired with the given registry and L1 chain.
    ///
    /// The internal EVM is seeded from the same genesis state as [`L2Sequencer`]:
    /// [`TEST_ACCOUNT_ADDRESS`] is funded with 1 ETH. The `block_registry` should
    /// be shared with the sequencer so that state roots computed here can be
    /// compared against sequencer-produced roots after derivation.
    ///
    /// [`L2Sequencer`]: crate::L2Sequencer
    /// [`TEST_ACCOUNT_ADDRESS`]: crate::TEST_ACCOUNT_ADDRESS
    pub fn new(
        rollup_config: Arc<RollupConfig>,
        canonical_head: L2BlockInfo,
        block_registry: SharedBlockHashRegistry,
        l1_chain: SharedL1Chain,
    ) -> Self {
        let executor = StatefulL2Executor::new((*rollup_config).clone());
        let inner = Arc::new(Mutex::new(ActionEngineClientInner {
            executor,
            canonical_head,
            executed_headers: HashMap::new(),
        }));
        Self { inner, rollup_config, block_registry, l1_chain }
    }

    fn execute_v1_inner(
        inner: &mut ActionEngineClientInner,
        registry: &SharedBlockHashRegistry,
        payload: &ExecutionPayloadV1,
    ) -> TransportResult<B256> {
        let txs = payload
            .transactions
            .iter()
            .map(|raw| {
                OpTxEnvelope::decode_2718(&mut raw.as_ref()).map_err(|e| {
                    TransportError::from(TransportErrorKind::custom_str(&e.to_string()))
                })
            })
            .collect::<TransportResult<Vec<_>>>()?;

        let (state_root, gas_used) = inner
            .executor
            .execute_transactions(
                &txs,
                payload.block_number,
                payload.timestamp,
                payload.parent_hash,
            )
            .map_err(|e| TransportError::from(TransportErrorKind::custom_str(&e.to_string())))?;

        if let Some(expected_root) = registry.get_state_root(payload.block_number) {
            assert_eq!(
                state_root, expected_root,
                "state root mismatch at block {}: computed={}, expected={}",
                payload.block_number, state_root, expected_root,
            );
        }

        let header = Header {
            number: payload.block_number,
            timestamp: payload.timestamp,
            parent_hash: payload.parent_hash,
            gas_limit: payload.gas_limit,
            gas_used,
            state_root,
            base_fee_per_gas: Some(payload.base_fee_per_gas.to()),
            ..Default::default()
        };
        let block_hash = header.hash_slow();
        inner.executed_headers.insert(payload.block_number, header);
        Ok(block_hash)
    }

    const fn make_valid(block_hash: B256) -> PayloadStatus {
        PayloadStatus { status: PayloadStatusEnum::Valid, latest_valid_hash: Some(block_hash) }
    }

    const fn make_fcu_valid(head_block_hash: B256) -> ForkchoiceUpdated {
        ForkchoiceUpdated { payload_status: Self::make_valid(head_block_hash), payload_id: None }
    }

    fn header_to_l1_rpc_block(header: &Header, block_hash: B256) -> Block<EthTransaction> {
        let sealed = Sealed::new_unchecked(header.clone(), block_hash);
        let rpc_header = alloy_rpc_types_eth::Header::from_sealed(sealed);
        Block {
            header: rpc_header,
            uncles: vec![],
            transactions: BlockTransactions::Hashes(vec![]),
            withdrawals: None,
        }
    }

    fn header_to_l2_rpc_block(header: &Header, block_hash: B256) -> Block<OpTransaction> {
        let sealed = Sealed::new_unchecked(header.clone(), block_hash);
        let rpc_header = alloy_rpc_types_eth::Header::from_sealed(sealed);
        Block {
            header: rpc_header,
            uncles: vec![],
            transactions: BlockTransactions::Hashes(vec![]),
            withdrawals: None,
        }
    }
}

#[async_trait]
impl EngineClient for ActionEngineClient {
    fn cfg(&self) -> &RollupConfig {
        &self.rollup_config
    }

    fn get_l1_block(&self, block: BlockId) -> EthGetBlock<<Ethereum as Network>::BlockResponse> {
        let chain = self.l1_chain.clone();
        let block_id = block;
        EthGetBlock::new_provider(
            block,
            Box::new(move |_kind| {
                let chain = chain.clone();
                ProviderCall::BoxedFuture(Box::pin(async move {
                    let rpc_block = match block_id {
                        BlockId::Number(num_or_tag) => {
                            let number = match num_or_tag {
                                BlockNumberOrTag::Number(n) => n,
                                _ => return Ok(None),
                            };
                            chain
                                .get_block(number)
                                .map(|l1| Self::header_to_l1_rpc_block(&l1.header, l1.hash()))
                        }
                        BlockId::Hash(block_hash) => chain
                            .block_by_hash(block_hash.block_hash)
                            .map(|l1| Self::header_to_l1_rpc_block(&l1.header, l1.hash())),
                    };
                    Ok(rpc_block)
                }))
            }),
        )
    }

    fn get_l2_block(&self, block: BlockId) -> EthGetBlock<<Base as Network>::BlockResponse> {
        let inner = Arc::clone(&self.inner);
        let block_id = block;
        EthGetBlock::new_provider(
            block,
            Box::new(move |_kind| {
                let inner = Arc::clone(&inner);
                ProviderCall::BoxedFuture(Box::pin(async move {
                    let guard = inner.lock().expect("action engine inner lock poisoned");
                    let rpc_block = match block_id {
                        BlockId::Number(num_or_tag) => {
                            let number = match num_or_tag {
                                BlockNumberOrTag::Number(n) => n,
                                _ => return Ok(None),
                            };
                            guard
                                .executed_headers
                                .get(&number)
                                .map(|h| Self::header_to_l2_rpc_block(h, h.hash_slow()))
                        }
                        BlockId::Hash(block_hash) => guard
                            .executed_headers
                            .values()
                            .find(|h| h.hash_slow() == block_hash.block_hash)
                            .map(|h| Self::header_to_l2_rpc_block(h, h.hash_slow())),
                    };
                    Ok(rpc_block)
                }))
            }),
        )
    }

    fn get_proof(
        &self,
        address: Address,
        _keys: Vec<StorageKey>,
    ) -> RpcWithBlock<(Address, Vec<StorageKey>), EIP1186AccountProofResponse> {
        RpcWithBlock::new_provider(move |_block_id| {
            ProviderCall::BoxedFuture(Box::pin(async move {
                Ok(EIP1186AccountProofResponse {
                    address,
                    balance: Default::default(),
                    code_hash: Default::default(),
                    nonce: 0,
                    storage_hash: Default::default(),
                    account_proof: vec![],
                    storage_proof: vec![],
                })
            }))
        })
    }

    async fn new_payload_v1(&self, _payload: ExecutionPayloadV1) -> TransportResult<PayloadStatus> {
        Err(TransportError::from(TransportErrorKind::custom_str(
            "ActionEngineClient does not support new_payload_v1 \
             (OP Stack derivation uses new_payload_v2 or later)",
        )))
    }

    async fn l2_block_by_label(
        &self,
        numtag: BlockNumberOrTag,
    ) -> Result<Option<Block<OpTransaction>>, EngineClientError> {
        let guard = self.inner.lock().expect("action engine inner lock poisoned");
        let block = match numtag {
            BlockNumberOrTag::Number(n) => guard
                .executed_headers
                .get(&n)
                .map(|h| Self::header_to_l2_rpc_block(h, h.hash_slow())),
            BlockNumberOrTag::Latest
            | BlockNumberOrTag::Safe
            | BlockNumberOrTag::Finalized
            | BlockNumberOrTag::Pending => {
                let number = guard.canonical_head.block_info.number;
                guard
                    .executed_headers
                    .get(&number)
                    .map(|h| Self::header_to_l2_rpc_block(h, h.hash_slow()))
            }
            BlockNumberOrTag::Earliest => guard
                .executed_headers
                .values()
                .min_by_key(|h| h.number)
                .map(|h| Self::header_to_l2_rpc_block(h, h.hash_slow())),
        };
        Ok(block)
    }

    async fn l2_block_info_by_label(
        &self,
        numtag: BlockNumberOrTag,
    ) -> Result<Option<L2BlockInfo>, EngineClientError> {
        let guard = self.inner.lock().expect("action engine inner lock poisoned");
        let info = match numtag {
            BlockNumberOrTag::Latest
            | BlockNumberOrTag::Safe
            | BlockNumberOrTag::Finalized
            | BlockNumberOrTag::Pending => Some(guard.canonical_head),
            BlockNumberOrTag::Number(n) => {
                if n == guard.canonical_head.block_info.number {
                    Some(guard.canonical_head)
                } else {
                    guard.executed_headers.get(&n).map(|h| {
                        let block_hash = h.hash_slow();
                        L2BlockInfo {
                            block_info: BlockInfo {
                                hash: block_hash,
                                number: h.number,
                                parent_hash: h.parent_hash,
                                timestamp: h.timestamp,
                            },
                            l1_origin: Default::default(),
                            seq_num: 0,
                        }
                    })
                }
            }
            BlockNumberOrTag::Earliest => None,
        };
        Ok(info)
    }
}

#[async_trait]
impl OpEngineApi<Base, Http<HyperAuthClient>> for ActionEngineClient {
    async fn new_payload_v2(
        &self,
        payload: ExecutionPayloadInputV2,
    ) -> TransportResult<PayloadStatus> {
        let mut guard = self.inner.lock().expect("action engine inner lock poisoned");
        let block_hash =
            Self::execute_v1_inner(&mut guard, &self.block_registry, &payload.execution_payload)?;
        Ok(Self::make_valid(block_hash))
    }

    async fn new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        _parent_beacon_block_root: B256,
    ) -> TransportResult<PayloadStatus> {
        let mut guard = self.inner.lock().expect("action engine inner lock poisoned");
        let block_hash = Self::execute_v1_inner(
            &mut guard,
            &self.block_registry,
            &payload.payload_inner.payload_inner,
        )?;
        Ok(Self::make_valid(block_hash))
    }

    async fn new_payload_v4(
        &self,
        payload: OpExecutionPayloadV4,
        _parent_beacon_block_root: B256,
    ) -> TransportResult<PayloadStatus> {
        let mut guard = self.inner.lock().expect("action engine inner lock poisoned");
        let block_hash = Self::execute_v1_inner(
            &mut guard,
            &self.block_registry,
            &payload.payload_inner.payload_inner.payload_inner,
        )?;
        Ok(Self::make_valid(block_hash))
    }

    async fn fork_choice_updated_v2(
        &self,
        fork_choice_state: ForkchoiceState,
        _payload_attributes: Option<OpPayloadAttributes>,
    ) -> TransportResult<ForkchoiceUpdated> {
        let head = fork_choice_state.head_block_hash;
        let mut guard = self.inner.lock().expect("action engine inner lock poisoned");
        if let Some(h) = guard.executed_headers.values().find(|h| h.hash_slow() == head).cloned() {
            let block_hash = h.hash_slow();
            guard.canonical_head = L2BlockInfo {
                block_info: BlockInfo {
                    hash: block_hash,
                    number: h.number,
                    parent_hash: h.parent_hash,
                    timestamp: h.timestamp,
                },
                l1_origin: Default::default(),
                seq_num: 0,
            };
        }
        Ok(Self::make_fcu_valid(head))
    }

    async fn fork_choice_updated_v3(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<OpPayloadAttributes>,
    ) -> TransportResult<ForkchoiceUpdated> {
        self.fork_choice_updated_v2(fork_choice_state, payload_attributes).await
    }

    async fn get_payload_v2(
        &self,
        _payload_id: PayloadId,
    ) -> TransportResult<ExecutionPayloadEnvelopeV2> {
        Err(TransportError::from(TransportErrorKind::custom_str(
            "ActionEngineClient does not support get_payload_v2",
        )))
    }

    async fn get_payload_v3(
        &self,
        _payload_id: PayloadId,
    ) -> TransportResult<OpExecutionPayloadEnvelopeV3> {
        Err(TransportError::from(TransportErrorKind::custom_str(
            "ActionEngineClient does not support get_payload_v3",
        )))
    }

    async fn get_payload_v4(
        &self,
        _payload_id: PayloadId,
    ) -> TransportResult<OpExecutionPayloadEnvelopeV4> {
        Err(TransportError::from(TransportErrorKind::custom_str(
            "ActionEngineClient does not support get_payload_v4",
        )))
    }

    async fn get_payload_v5(
        &self,
        _payload_id: PayloadId,
    ) -> TransportResult<OpExecutionPayloadEnvelopeV5> {
        Err(TransportError::from(TransportErrorKind::custom_str(
            "ActionEngineClient does not support get_payload_v5",
        )))
    }

    async fn get_payload_bodies_by_hash_v1(
        &self,
        _block_hashes: Vec<BlockHash>,
    ) -> TransportResult<ExecutionPayloadBodiesV1> {
        Err(TransportError::from(TransportErrorKind::custom_str(
            "ActionEngineClient does not support get_payload_bodies_by_hash_v1",
        )))
    }

    async fn get_payload_bodies_by_range_v1(
        &self,
        _start: u64,
        _count: u64,
    ) -> TransportResult<ExecutionPayloadBodiesV1> {
        Err(TransportError::from(TransportErrorKind::custom_str(
            "ActionEngineClient does not support get_payload_bodies_by_range_v1",
        )))
    }

    async fn get_client_version_v1(
        &self,
        _client_version: ClientVersionV1,
    ) -> TransportResult<Vec<ClientVersionV1>> {
        Err(TransportError::from(TransportErrorKind::custom_str(
            "ActionEngineClient does not support get_client_version_v1",
        )))
    }

    async fn exchange_capabilities(
        &self,
        _capabilities: Vec<String>,
    ) -> TransportResult<Vec<String>> {
        Err(TransportError::from(TransportErrorKind::custom_str(
            "ActionEngineClient does not support exchange_capabilities",
        )))
    }
}
