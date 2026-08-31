//! Sequencer engine backend that produces blocks with the production Flashblocks builder.
//!
//! [`ActionEngineClient`](crate::ActionEngineClient) assembles blocks with reth's execution-side
//! `BasePayloadBuilder` and a `NoopTransactionPool`, forcing `no_tx_pool = true` — so it never
//! exercises the production Base builder's block-construction policy (pool tx selection/ordering,
//! DA/gas limits, metering, revert/rejection semantics).
//!
//! [`BuilderBackedEngineClient`] instead launches an in-process
//! [`LocalInstance`](base_builder_core::test_utils::LocalInstance) running the production
//! [`FlashblocksServiceBuilder`](base_builder_core::FlashblocksServiceBuilder) with a real
//! transaction pool, against the harness's rollup-config-derived genesis. It implements the
//! production [`SequencerEngineClient`] seam, so the harness's real `SequencerActor` drives the real
//! builder over the Engine API — while the verifier still re-executes the derived blocks, keeping
//! block hashes and state roots aligned with `rollup_config.genesis.l2.hash`.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_consensus::transaction::SignerRecoverable;
use alloy_eips::{eip2718::Encodable2718, eip7685::Requests};
use alloy_provider::{Identity, ProviderBuilder};
use alloy_rpc_types_engine::PayloadId;
use async_trait::async_trait;
use base_builder_core::{
    BuilderConfig,
    test_utils::{
        ChainDriver, EngineApi, Ipc, LocalInstance, LocalInstanceBuilder,
        node_config_with_chain_spec,
    },
};
use base_common_consensus::BaseTxEnvelope;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadEnvelope};
use base_consensus_engine::EngineGetPayloadVersion;
use base_consensus_node::{
    EngineClientError, EngineClientResult, ResetReason, SequencerEngineClient,
};
use base_execution_chainspec::BaseChainSpec;
use base_execution_payload_builder::BasePayloadBuilderAttributes;
use base_execution_txpool::BasePooledTransaction;
use base_protocol::{AttributesWithParent, L2BlockInfo};

use super::ExecutionPayloadConverter;
use crate::{ActionEngineClient, SequencerEngineBackend, SharedBlockHashRegistry};

/// A sequencer engine backend backed by the production Flashblocks builder running in-process.
///
/// Built against the same genesis [`ActionEngineClient`] derives from the rollup config, so blocks
/// it produces are byte-for-byte compatible with the harness's verifier nodes and batcher.
#[derive(Debug)]
pub struct BuilderBackedEngineClient {
    /// Keeps the in-process builder node alive (its `Drop` shuts the node down) and makes this type
    /// `Sync` (`LocalInstance` is `Send` but not `Sync`).
    instance: Mutex<LocalInstance>,
    /// The authenticated Engine API IPC socket path, used to build engine clients on demand.
    auth_ipc: String,
    /// How long to let the flashblocks build loop run before sealing a block.
    block_time: Duration,
    rollup_config: Arc<RollupConfig>,
    /// The current unsafe head, advanced as payloads are inserted. Initialized to genesis.
    head: Mutex<L2BlockInfo>,
    /// Registry of produced block hashes / state roots, shared with the harness for
    /// sequencer↔verifier state-root cross-checks.
    block_registry: SharedBlockHashRegistry,
}

impl BuilderBackedEngineClient {
    /// Launch an in-process builder node against the genesis derived from `rollup_config`.
    ///
    /// The node runs the production flashblocks payload service and a real transaction pool over an
    /// IPC engine endpoint (no HTTP/WS/P2P), keeping it close to the action harness's in-process,
    /// socket-light ethos while still exercising the real builder.
    pub async fn new(
        rollup_config: Arc<RollupConfig>,
        genesis_head: L2BlockInfo,
    ) -> eyre::Result<Self> {
        let genesis = ActionEngineClient::build_genesis_for_rollup(&rollup_config);
        let chain_spec = Arc::new(BaseChainSpec::from_genesis(genesis));
        let node_config = node_config_with_chain_spec(chain_spec);
        let instance = LocalInstanceBuilder::new(BuilderConfig::for_tests())
            .with_node_config(node_config)
            .build()
            .await?;

        let auth_ipc = instance.auth_ipc().to_string();
        let block_time = instance.builder_config().block_time;

        // The caller supplies the genesis head, anchored to the real L1 genesis origin, so the
        // sequencer's origin selector can resolve it (the rollup config's `genesis.l1` is often
        // zeroed). `get_unsafe_head` returns this until payloads are inserted.
        Ok(Self {
            instance: Mutex::new(instance),
            auth_ipc,
            block_time,
            rollup_config,
            head: Mutex::new(genesis_head),
            block_registry: SharedBlockHashRegistry::new(),
        })
    }

    /// The rollup config this backend was launched for.
    pub fn rollup_config(&self) -> Arc<RollupConfig> {
        Arc::clone(&self.rollup_config)
    }

    /// An Engine API client for the in-process builder node's authenticated IPC endpoint.
    fn engine(&self) -> EngineApi<Ipc> {
        EngineApi::<Ipc>::with_ipc(&self.auth_ipc)
    }

    /// A [`ChainDriver`] for driving block production over the builder's engine API directly
    /// (bypassing the sequencer actor) — useful for lower-level tests.
    pub async fn driver(&self) -> eyre::Result<ChainDriver<Ipc>> {
        // Read the RPC IPC path under a short lock, then release it before connecting so no lock is
        // held across an await point.
        let rpc_ipc = self.instance.lock().expect("instance lock").rpc_ipc().to_string();
        let provider = ProviderBuilder::<Identity, Identity, Base>::default()
            .connect_ipc(rpc_ipc.into())
            .await
            .map_err(|e| eyre::eyre!("failed to connect builder provider: {e}"))?;
        Ok(ChainDriver::<Ipc>::remote(provider, self.engine()))
    }
}

/// Drives the production Flashblocks builder over the Engine API on behalf of the harness's
/// production `SequencerActor`, mapping each [`SequencerEngineClient`] call onto a real engine
/// round-trip: `forkchoiceUpdated`-with-attributes to start a build, `getPayload` to seal it, and
/// `newPayload` + canonical `forkchoiceUpdated` to import it.
#[async_trait]
impl SequencerEngineClient for BuilderBackedEngineClient {
    async fn reset_engine_forkchoice(&self, _reason: ResetReason) -> EngineClientResult<()> {
        let head = self.head.lock().expect("head lock").block_info.hash;
        self.engine()
            .update_forkchoice(head, head, None)
            .await
            .map_err(|e| EngineClientError::ResetForkchoiceError(e.to_string()))?;
        Ok(())
    }

    async fn start_build_block(
        &self,
        attributes: AttributesWithParent,
    ) -> EngineClientResult<PayloadId> {
        let parent = attributes.parent.block_info.hash;
        // BasePayloadBuilderAttributes currently computes payload IDs with version 3 in its
        // PayloadAttributes implementation. Keep this constructor in sync with that engine-side
        // contract; changing only this value would make getPayload miss the registered build.
        let builder_attrs = BasePayloadBuilderAttributes::<BaseTxEnvelope>::try_new(
            parent,
            attributes.attributes,
            3,
        )
        .map_err(|e| EngineClientError::RequestError(e.to_string()))?;
        let fcu = self
            .engine()
            .update_forkchoice(parent, parent, Some(builder_attrs))
            .await
            .map_err(|e| EngineClientError::RequestError(e.to_string()))?;
        if !fcu.payload_status.is_valid() {
            return Err(EngineClientError::ResponseError(format!(
                "engine rejected build-block forkchoice: {:?}",
                fcu.payload_status
            )));
        }
        fcu.payload_id
            .ok_or_else(|| EngineClientError::ResponseError("no payload id returned".into()))
    }

    async fn get_sealed_payload(
        &self,
        payload_id: PayloadId,
        attributes: AttributesWithParent,
    ) -> EngineClientResult<BaseExecutionPayloadEnvelope> {
        // Give the flashblocks build loop a full block time to assemble the block before resolving
        // it, matching the production sequencer's start-of-slot to end-of-slot cadence.
        tokio::time::sleep(self.block_time).await;
        let timestamp = attributes.attributes.payload_attributes.timestamp;
        // Base Azul requires `engine_getPayloadV5` (still imported via `newPayloadV4` in
        // `insert_unsafe_payload`); earlier upgrades use V4. Both envelopes carry the same
        // `BaseExecutionPayloadV4` payload and `execution_requests` shape, so only the retrieval
        // call differs.
        let (execution_payload, execution_requests) =
            match EngineGetPayloadVersion::from_cfg(&self.rollup_config, timestamp) {
                EngineGetPayloadVersion::V5 => {
                    let envelope = self
                        .engine()
                        .get_payload_v5(payload_id)
                        .await
                        .map_err(|e| EngineClientError::ResponseError(e.to_string()))?;
                    (envelope.execution_payload, envelope.execution_requests)
                }
                _ => {
                    let envelope = self
                        .engine()
                        .get_payload(payload_id)
                        .await
                        .map_err(|e| EngineClientError::ResponseError(e.to_string()))?;
                    (envelope.execution_payload, envelope.execution_requests)
                }
            };
        // The production builder never populates EIP-7685 requests for Base blocks (L2 has no
        // EL-triggered requests), and post-Isthmus header validation independently rejects any
        // non-empty `requests_hash`, so a non-empty response here would indicate the builder or
        // chain config changed in a way this backend doesn't yet support.
        if !execution_requests.is_empty() {
            return Err(EngineClientError::ResponseError(
                "builder backend does not support non-empty EIP-7685 execution requests".into(),
            ));
        }
        Ok(BaseExecutionPayloadEnvelope {
            parent_beacon_block_root: attributes
                .attributes
                .payload_attributes
                .parent_beacon_block_root,
            execution_payload: BaseExecutionPayload::V4(execution_payload),
        })
    }

    async fn insert_unsafe_payload(
        &self,
        payload: BaseExecutionPayloadEnvelope,
    ) -> EngineClientResult<L2BlockInfo> {
        let block = ExecutionPayloadConverter::block_from_envelope(&payload)
            .map_err(|e| EngineClientError::RequestError(e.to_string()))?;
        let new_hash = block.header.hash_slow();
        let parent_beacon_block_root = payload.parent_beacon_block_root.unwrap_or_default();
        let BaseExecutionPayload::V4(v4) = payload.execution_payload else {
            return Err(EngineClientError::RequestError(
                "builder backend expects a V4 execution payload".into(),
            ));
        };

        let engine = self.engine();
        let status = engine
            .new_payload(v4, vec![], parent_beacon_block_root, Requests::default())
            .await
            .map_err(|e| EngineClientError::RequestError(e.to_string()))?;
        if !status.is_valid() {
            return Err(EngineClientError::RequestError(format!(
                "engine rejected payload: {status:?}"
            )));
        }

        // Known limitation: this immediately promotes the prior head to both `safe` and
        // `finalized` on the builder's internal engine, rather than tracking the harness's
        // actual (lagging) safe/finalized state. The builder node has no other consumer of those
        // fields today, so this backend cannot yet model unsafe L2 reorgs; revisit once the
        // backend needs to drive the builder through a real safe/finalized forkchoice sequence.
        let current_head = self.head.lock().expect("head lock").block_info.hash;
        let fcu = engine
            .update_forkchoice(current_head, new_hash, None)
            .await
            .map_err(|e| EngineClientError::RequestError(e.to_string()))?;
        if !fcu.payload_status.is_valid() {
            return Err(EngineClientError::RequestError(format!(
                "engine rejected forkchoice update: {:?}",
                fcu.payload_status
            )));
        }

        // Record the produced block so the harness verifier can cross-check its re-executed
        // state root against the builder-produced one.
        self.block_registry.insert(block.header.number, new_hash, Some(block.header.state_root));

        let info = L2BlockInfo::from_block_and_genesis(&block, &self.rollup_config.genesis)
            .map_err(|e| EngineClientError::ResponseError(e.to_string()))?;
        *self.head.lock().expect("head lock") = info;
        Ok(info)
    }

    async fn get_unsafe_head(&self) -> EngineClientResult<L2BlockInfo> {
        Ok(*self.head.lock().expect("head lock"))
    }

    async fn el_sync_finished(&self) -> EngineClientResult<bool> {
        // The in-process builder node is launched fresh against the harness genesis for each
        // test and never performs background execution-layer sync.
        Ok(true)
    }
}

#[async_trait]
impl SequencerEngineBackend for BuilderBackedEngineClient {
    fn block_hash_registry(&self) -> SharedBlockHashRegistry {
        self.block_registry.clone()
    }

    fn uses_transaction_pool(&self) -> bool {
        true
    }

    async fn inject_pool_transactions(&self, txs: Vec<BaseTxEnvelope>) -> EngineClientResult<()> {
        // Clone the pool handle under a short lock, then release it before awaiting.
        let pool = self.instance.lock().expect("instance lock").pool_handle();
        for tx in txs {
            let recovered = tx
                .try_into_recovered()
                .map_err(|e| EngineClientError::RequestError(format!("recover signer: {e}")))?;
            let encoded_length = recovered.encode_2718_len();
            let pooled = BasePooledTransaction::new(recovered, encoded_length);
            pool.add_external_transaction(pooled)
                .await
                .map_err(|e| EngineClientError::RequestError(e.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::BlockNumberOrTag;
    use alloy_primitives::B256;
    use base_common_genesis::UpgradeConfig;
    use base_protocol::BlockInfo;

    use super::*;
    use crate::TestRollupConfigBuilder;

    /// A rollup config with every fork through Jovian active at genesis (v4 payloads, no Azul).
    ///
    /// Jovian is required because the builder [`ChainDriver`] always sends `min_base_fee`, a
    /// Jovian-gated payload attribute; Azul is left off so the engine `getPayload` V4 path is used.
    fn jovian_rollup_config() -> Arc<RollupConfig> {
        let mut config = TestRollupConfigBuilder::mainnet();
        config.upgrades = UpgradeConfig {
            canyon_time: Some(0),
            delta_time: Some(0),
            ecotone_time: Some(0),
            fjord_time: Some(0),
            granite_time: Some(0),
            holocene_time: Some(0),
            isthmus_time: Some(0),
            jovian_time: Some(0),
            ..Default::default()
        };
        config.genesis.l2_time = 0;
        Arc::new(config)
    }

    /// The genesis head anchored at the harness-derived genesis hash.
    fn genesis_head(rollup_config: &RollupConfig) -> L2BlockInfo {
        let cg = &rollup_config.genesis;
        L2BlockInfo::new(
            BlockInfo::new(
                ActionEngineClient::compute_l2_genesis_hash(rollup_config),
                cg.l2.number,
                B256::ZERO,
                cg.l2_time,
            ),
            cg.l1,
            0,
        )
    }

    /// The in-process builder node must initialize the exact genesis the harness derives from the
    /// rollup config, and produce a first block that builds on that genesis — proving hash/state
    /// alignment between the real builder backend and the harness verifier.
    #[tokio::test(flavor = "multi_thread")]
    async fn builder_backend_genesis_alignment() -> eyre::Result<()> {
        let rollup_config = jovian_rollup_config();
        let expected_genesis = ActionEngineClient::compute_l2_genesis_hash(&rollup_config);

        let backend = BuilderBackedEngineClient::new(
            Arc::clone(&rollup_config),
            genesis_head(&rollup_config),
        )
        .await?;
        let driver = backend.driver().await?;

        // The builder node's genesis hash must match the harness-derived genesis hash.
        let genesis_block = driver
            .get_block(BlockNumberOrTag::Number(0))
            .await?
            .expect("genesis block should exist");
        assert_eq!(
            genesis_block.header.hash, expected_genesis,
            "builder node genesis hash must match the harness-derived genesis",
        );

        // The real Flashblocks builder must produce a first block that builds on that genesis.
        let block = driver.build_new_block().await?;
        assert_eq!(block.header.number, 1, "first built block must be block 1");
        assert_eq!(
            block.header.parent_hash, expected_genesis,
            "first built block must build on the harness-derived genesis",
        );

        Ok(())
    }

    /// Driving the backend through the production [`SequencerEngineClient`] seam advances the unsafe
    /// head: `get_unsafe_head` starts at genesis and reflects each inserted block.
    #[tokio::test(flavor = "multi_thread")]
    async fn builder_backend_get_unsafe_head_starts_at_genesis() -> eyre::Result<()> {
        let rollup_config = jovian_rollup_config();
        let backend = BuilderBackedEngineClient::new(
            Arc::clone(&rollup_config),
            genesis_head(&rollup_config),
        )
        .await?;

        let head = backend.get_unsafe_head().await?;
        assert_eq!(head.block_info.number, 0, "unsafe head must start at genesis");
        assert_eq!(
            head.block_info.hash,
            ActionEngineClient::compute_l2_genesis_hash(&rollup_config),
            "genesis head hash must match the harness-derived genesis",
        );

        Ok(())
    }
}
