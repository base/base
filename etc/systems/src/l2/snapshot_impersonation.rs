//! Impersonated transaction injection for interactive L1-free snapshot devnets.
//!
//! `dev_impersonateTransaction` queues an unsigned call as a synthetic deposit transaction with an
//! arbitrary sender and `mint = 0`. The standalone sequencer appends queued deposits after its
//! mandatory deposits until the transaction appears in the canonical chain.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use alloy_consensus::Transaction as _;
use alloy_eips::{
    BlockNumHash,
    eip2718::{Decodable2718, Encodable2718},
};
use alloy_primitives::{Address, B256, Bytes, TxKind, U64, U256, keccak256};
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;
use base_common_consensus::{BaseTxEnvelope, REGOLITH_SYSTEM_TX_GAS, TxDeposit};
use base_common_network::Base;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_consensus_derive::{AttributesBuilder, BuilderError, PipelineError, PipelineResult};
use base_consensus_node::StandaloneAttributesBuilder;
use base_node_runner::{BaseNodeExtension, BaseRpcContext, NodeHooks};
use base_protocol::{BlockInfo, L2BlockInfo};
use jsonrpsee::{
    RpcModule,
    types::{
        ErrorObjectOwned,
        error::{INVALID_PARAMS_CODE, SERVER_IS_BUSY_CODE},
    },
};
use serde::Deserialize;

/// Parameters accepted by `dev_impersonateTransaction`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotImpersonationRequest {
    /// Impersonated sender. Its balance pays `value`; no ETH is minted.
    pub from: Address,
    /// Call recipient.
    pub to: Address,
    /// Gas limit, as a hex quantity.
    pub gas: U64,
    /// Wei transferred from `from`, as a hex quantity.
    #[serde(default)]
    pub value: U256,
    /// Call data.
    #[serde(default)]
    pub data: Bytes,
}

/// A queued impersonated deposit.
#[derive(Debug)]
pub struct ImpersonatedDeposit {
    hash: B256,
    gas_limit: u64,
    encoded: Bytes,
    /// Lowest block number this deposit was offered for since the last canonical reconciliation.
    offered_for: Option<u64>,
}

/// Bounded in-memory queue of impersonated deposits, registered as the builder's
/// `dev_impersonateTransaction` RPC.
///
/// Requests are retained until they are observed in the canonical chain, so failed or abandoned
/// builds re-offer them. Claims are per-process and do not survive restarts or arbitrary reorgs.
#[derive(Debug, Clone)]
pub struct SnapshotImpersonation {
    block_gas_limit: u64,
    entries: Arc<Mutex<Vec<ImpersonatedDeposit>>>,
}

impl SnapshotImpersonation {
    /// JSON-RPC method name.
    pub const METHOD: &'static str = "dev_impersonateTransaction";
    /// Gas reserved for the L1-info and base-time deposits that precede injected transactions.
    pub const MANDATORY_DEPOSIT_GAS: u64 = 2 * REGOLITH_SYSTEM_TX_GAS;
    /// Minimum accepted gas limit: the intrinsic cost of a plain call.
    pub const MIN_GAS: u64 = 21_000;
    /// Maximum accepted call data length, matching the default transaction-pool size limit.
    pub const MAX_INPUT_BYTES: usize = 128 * 1024;
    /// Maximum number of queued requests.
    pub const MAX_PENDING: usize = 1_024;
    /// Maximum total encoded size of queued requests, keeping payload attributes well below the
    /// Engine API request limit.
    pub const MAX_PENDING_BYTES: usize = 4 * 1024 * 1024;

    /// Creates an empty queue for blocks with the given gas limit.
    pub fn new(block_gas_limit: u64) -> Self {
        Self { block_gas_limit, entries: Arc::default() }
    }

    /// Validates and queues a request, returning the deposit transaction hash.
    pub fn submit(&self, request: SnapshotImpersonationRequest) -> Result<B256, ErrorObjectOwned> {
        let gas_limit = request.gas.to::<u64>();
        let max_gas = self.block_gas_limit.saturating_sub(Self::MANDATORY_DEPOSIT_GAS);
        if !(Self::MIN_GAS..=max_gas).contains(&gas_limit) {
            return Err(Self::error(
                INVALID_PARAMS_CODE,
                format!("gas must be between {} and {max_gas}", Self::MIN_GAS),
            ));
        }
        if request.data.len() > Self::MAX_INPUT_BYTES {
            return Err(Self::error(
                INVALID_PARAMS_CODE,
                format!("data exceeds {} bytes", Self::MAX_INPUT_BYTES),
            ));
        }

        // A random source hash keeps identical requests distinct, within and across runs.
        let deposit = TxDeposit {
            source_hash: B256::from(rand::random::<[u8; 32]>()),
            from: request.from,
            to: TxKind::Call(request.to),
            mint: 0,
            value: request.value,
            gas_limit,
            is_system_transaction: false,
            input: request.data,
        };
        let encoded = Bytes::from(deposit.encoded_2718());
        let hash = keccak256(&encoded);

        let mut entries = self.entries.lock().expect("impersonation queue lock poisoned");
        let pending_bytes: usize = entries.iter().map(|entry| entry.encoded.len()).sum();
        if entries.len() >= Self::MAX_PENDING
            || pending_bytes + encoded.len() > Self::MAX_PENDING_BYTES
        {
            return Err(Self::error(SERVER_IS_BUSY_CODE, "impersonation queue is full".into()));
        }
        entries.push(ImpersonatedDeposit { hash, gas_limit, encoded, offered_for: None });
        Ok(hash)
    }

    /// Retires deposits included in `parent` or any canonical ancestor they may have been offered
    /// for.
    ///
    /// Walks back from the exact parent hash, so it does not depend on canonical-head
    /// notifications. Performs no I/O unless a deposit was offered for a block at or below
    /// `parent`.
    pub async fn reconcile(
        &self,
        provider: &RootProvider<Base>,
        parent: BlockInfo,
    ) -> PipelineResult<()> {
        let (oldest, mut unresolved) = {
            let entries = self.entries.lock().expect("impersonation queue lock poisoned");
            let offered = entries.iter().filter_map(|entry| {
                entry.offered_for.filter(|number| *number <= parent.number).map(|n| (n, entry.hash))
            });
            let (numbers, hashes): (Vec<u64>, HashSet<B256>) = offered.unzip();
            let Some(oldest) = numbers.into_iter().min() else { return Ok(()) };
            (oldest, hashes)
        };

        let mut included = HashSet::new();
        let mut hash = parent.hash;
        while !unresolved.is_empty() {
            let block = provider
                .get_block_by_hash(hash)
                .await
                .map_err(|error| {
                    tracing::warn!(error = %error, block = %hash, "impersonation reconciliation failed");
                    PipelineError::Provider(error.to_string()).temp()
                })?
                .ok_or_else(|| {
                    tracing::warn!(block = %hash, "impersonation reconciliation block unavailable");
                    PipelineError::Provider(format!("canonical block {hash} not found")).temp()
                })?;
            for tx_hash in block.transactions.hashes() {
                if unresolved.remove(&tx_hash) {
                    included.insert(tx_hash);
                }
            }
            if block.header.number <= oldest {
                break;
            }
            hash = block.header.parent_hash;
        }

        let mut entries = self.entries.lock().expect("impersonation queue lock poisoned");
        entries.retain(|entry| !included.contains(&entry.hash));
        for entry in entries.iter_mut() {
            if entry.offered_for.is_some_and(|number| number <= parent.number) {
                entry.offered_for = None;
            }
        }
        Ok(())
    }

    /// Returns the longest FIFO prefix of queued deposits fitting `gas_budget`, marking them as
    /// offered for `block_number`. Deposits stay queued until [`Self::reconcile`] retires them.
    pub fn select(&self, block_number: u64, mut gas_budget: u64) -> Vec<Bytes> {
        let mut entries = self.entries.lock().expect("impersonation queue lock poisoned");
        let mut selected = Vec::new();
        for entry in entries.iter_mut() {
            if entry.gas_limit > gas_budget {
                break;
            }
            gas_budget -= entry.gas_limit;
            entry.offered_for = Some(block_number);
            selected.push(entry.encoded.clone());
        }
        selected
    }

    /// Builds the `dev_impersonateTransaction` RPC module.
    pub fn into_rpc(self) -> RpcModule<Self> {
        let mut module = RpcModule::new(self);
        module
            .register_method(Self::METHOD, |params, queue, _| queue.submit(params.one()?))
            .expect("method is registered once on a new module");
        module
    }

    fn error(code: i32, message: String) -> ErrorObjectOwned {
        ErrorObjectOwned::owned(code, message, None::<()>)
    }
}

impl BaseNodeExtension for SnapshotImpersonation {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
            ctx.modules.merge_configured(self.into_rpc())?;
            Ok(())
        })
    }
}

/// Appends queued impersonated deposits to standalone snapshot payload attributes.
#[derive(Debug, Clone)]
pub struct SnapshotImpersonationAttributesBuilder {
    inner: StandaloneAttributesBuilder,
    queue: SnapshotImpersonation,
    provider: RootProvider<Base>,
}

impl SnapshotImpersonationAttributesBuilder {
    /// Wraps the standalone builder; `provider` reads the builder EL's canonical blocks.
    pub const fn new(
        inner: StandaloneAttributesBuilder,
        queue: SnapshotImpersonation,
        provider: RootProvider<Base>,
    ) -> Self {
        Self { inner, queue, provider }
    }
}

#[async_trait]
impl AttributesBuilder for SnapshotImpersonationAttributesBuilder {
    async fn prepare_payload_attributes(
        &mut self,
        l2_parent: L2BlockInfo,
        epoch: BlockNumHash,
    ) -> PipelineResult<BasePayloadAttributes> {
        let mut attributes = self.inner.prepare_payload_attributes(l2_parent, epoch).await?;
        self.queue.reconcile(&self.provider, l2_parent.block_info).await?;

        let gas_limit = attributes.gas_limit.unwrap_or(self.queue.block_gas_limit);
        let transactions = attributes.transactions.get_or_insert_default();
        let mut mandatory_gas = 0u64;
        for mut encoded in transactions.iter().map(Bytes::as_ref) {
            let tx = BaseTxEnvelope::decode_2718(&mut encoded).map_err(|error| {
                PipelineError::AttributesBuilder(BuilderError::Custom(error.to_string())).crit()
            })?;
            mandatory_gas = mandatory_gas.saturating_add(tx.gas_limit());
        }
        transactions.extend(self.queue.select(
            l2_parent.block_info.number.saturating_add(1),
            gas_limit.saturating_sub(mandatory_gas),
        ));
        Ok(attributes)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_eips::BlockNumHash;
    use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
    use alloy_provider::{RootProvider, mock::Asserter};
    use alloy_rpc_client::RpcClient;
    use alloy_rpc_types_eth::{Block, BlockTransactions, Header};
    use base_common_genesis::{BaseUpgrade, RollupConfig, SystemConfig};
    use base_consensus_derive::AttributesBuilder;
    use base_consensus_node::StandaloneAttributesBuilder;
    use base_protocol::{BlockInfo, L1BlockInfoBedrock, L1BlockInfoTx, L2BlockInfo};
    use serde_json::json;

    use super::{
        SnapshotImpersonation, SnapshotImpersonationAttributesBuilder, SnapshotImpersonationRequest,
    };

    const BLOCK_GAS_LIMIT: u64 = 30_000_000;

    fn request(gas: u64) -> SnapshotImpersonationRequest {
        SnapshotImpersonationRequest {
            from: Address::repeat_byte(0x01),
            to: Address::repeat_byte(0x02),
            gas: alloy_primitives::U64::from(gas),
            value: U256::ZERO,
            data: Bytes::new(),
        }
    }

    fn l2_block(number: u64) -> L2BlockInfo {
        let origin = BlockNumHash { number: 100, hash: B256::repeat_byte(0x11) };
        let hash = B256::with_last_byte(number as u8);
        let parent_hash = B256::with_last_byte(number as u8 - 1);
        L2BlockInfo::new(BlockInfo::new(hash, number, parent_hash, 2_000), origin, number)
    }

    fn canonical_block(number: u64, transactions: Vec<B256>) -> Block {
        let parent = l2_block(number).block_info;
        let mut header = Header::new(alloy_consensus::Header {
            number,
            parent_hash: parent.parent_hash,
            ..Default::default()
        });
        header.hash = parent.hash;
        Block::empty(header).with_transactions(BlockTransactions::Hashes(transactions))
    }

    fn builder(
        queue: &SnapshotImpersonation,
        asserter: &Asserter,
    ) -> SnapshotImpersonationAttributesBuilder {
        let l1_info = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::new(
            100,
            1_000,
            7,
            B256::repeat_byte(0x11),
            9,
            Address::repeat_byte(0x22),
            U256::from(3),
            U256::from(4),
        ));
        let mut rollup = RollupConfig { block_time: 2, ..Default::default() };
        rollup.genesis.l2_time = 1_000;
        rollup.set_upgrade_activation_timestamp(BaseUpgrade::Regolith, 0);
        let inner = StandaloneAttributesBuilder::new(
            Arc::new(rollup),
            l1_info,
            SystemConfig { gas_limit: BLOCK_GAS_LIMIT, ..Default::default() },
            None,
        );
        SnapshotImpersonationAttributesBuilder::new(
            inner,
            queue.clone(),
            RootProvider::new(RpcClient::mocked(asserter.clone())),
        )
    }

    async fn injected(builder: &mut impl AttributesBuilder, parent: L2BlockInfo) -> Vec<B256> {
        let attributes =
            builder.prepare_payload_attributes(parent, parent.l1_origin).await.unwrap();
        // The first transaction is the mandatory L1-info deposit.
        attributes.transactions.unwrap()[1..].iter().map(keccak256).collect()
    }

    #[tokio::test]
    async fn rpc_validates_requests_and_keeps_identical_requests_distinct() {
        let module = SnapshotImpersonation::new(BLOCK_GAS_LIMIT).into_rpc();
        let call = |params: serde_json::Value| {
            let module = module.clone();
            async move { module.call::<_, B256>(SnapshotImpersonation::METHOD, [params]).await }
        };
        let from = Address::repeat_byte(0x01);
        let to = Address::repeat_byte(0x02);

        let first = call(json!({ "from": from, "to": to, "gas": "0x5208" })).await.unwrap();
        let second = call(json!({ "from": from, "to": to, "gas": "0x5208" })).await.unwrap();
        assert_ne!(first, second);

        let max_gas = BLOCK_GAS_LIMIT - SnapshotImpersonation::MANDATORY_DEPOSIT_GAS;
        let oversized = vec![0_u8; SnapshotImpersonation::MAX_INPUT_BYTES + 1];
        for invalid in [
            json!({ "from": from, "to": to }),
            json!({ "from": from, "to": to, "gas": "0x5207" }),
            json!({ "from": from, "to": to, "gas": format!("{:#x}", max_gas + 1) }),
            json!({ "from": from, "to": to, "gas": "0x5208", "input": "0x" }),
            json!({ "from": from, "to": to, "gas": "0x5208", "data": Bytes::from(oversized) }),
        ] {
            assert!(call(invalid.clone()).await.is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn queue_rejects_requests_beyond_capacity() {
        let queue = SnapshotImpersonation::new(BLOCK_GAS_LIMIT);
        for _ in 0..SnapshotImpersonation::MAX_PENDING {
            queue.submit(request(21_000)).unwrap();
        }

        assert!(queue.submit(request(21_000)).is_err());
    }

    #[tokio::test]
    async fn retains_requests_until_canonical_inclusion() {
        let queue = SnapshotImpersonation::new(BLOCK_GAS_LIMIT);
        let asserter = Asserter::new();
        let mut builder = builder(&queue, &asserter);
        let first = queue.submit(request(100_000)).unwrap();

        // Repeated attempts on the same parent re-offer the request without RPC I/O.
        assert_eq!(injected(&mut builder, l2_block(10)).await, vec![first]);
        assert_eq!(injected(&mut builder, l2_block(10)).await, vec![first]);

        // A canonical child without the request means the build was abandoned; re-offer it.
        asserter.push_success(&canonical_block(11, vec![]));
        assert_eq!(injected(&mut builder, l2_block(11)).await, vec![first]);

        // Inclusion in an intervening canonical ancestor retires it without duplication.
        let second = queue.submit(request(100_000)).unwrap();
        asserter.push_success(&canonical_block(13, vec![]));
        asserter.push_success(&canonical_block(12, vec![first]));
        assert_eq!(injected(&mut builder, l2_block(13)).await, vec![second]);
        assert!(asserter.pop_response().is_none());
    }

    #[tokio::test]
    async fn selects_fifo_prefix_within_block_gas() {
        let queue = SnapshotImpersonation::new(BLOCK_GAS_LIMIT);
        let mut builder = builder(&queue, &Asserter::new());
        let first = queue.submit(request(20_000_000)).unwrap();
        queue.submit(request(20_000_000)).unwrap();
        queue.submit(request(21_000)).unwrap();

        assert_eq!(injected(&mut builder, l2_block(10)).await, vec![first]);
    }
}
