//! Engine-tree validation for locally built Base blocks.

use std::sync::Arc;

use alloy_consensus::{BlockHeader, Header};
use alloy_primitives::B256;
use base_common_consensus::{BaseBlock, BasePrimitives, BaseTxEnvelope};
use base_common_rpc_types_engine::ExecutionData;
use base_execution_chainspec::BaseChainSpec;
use base_execution_consensus::validate_base_time_metadata;
use base_execution_payload_builder::BasePayloadBuilderAttributes;
use reth_chain_state::ExecutedBlock;
use reth_engine_tree::tree::{
    CacheWaitDurations, EngineApiTreeState, EngineValidator, TreeConfig, WaitForCaches,
    payload_validator::{TreeCtx, ValidationOutcome},
};
use reth_node_api::{AddOnsContext, BuiltPayloadExecutedBlock, FullNodeComponents};
use reth_node_builder::rpc::{BasicEngineValidatorBuilder, EngineValidatorBuilder};
use reth_payload_builder::PayloadBuilderResources;
use reth_payload_primitives::{InvalidPayloadAttributesError, NewPayloadError};
use reth_primitives_traits::SealedBlock;
use reth_provider::{ProviderError, ProviderResult};
use reth_storage_overlay::OverlayManager;

use crate::{BaseEngineTypes, BaseNodeTypes};

/// Builds a [`BaseTreeEngineValidator`] around reth's standard validator.
#[derive(Debug, Clone, Default)]
pub struct BaseTreeEngineValidatorBuilder<EV>(BasicEngineValidatorBuilder<EV>);

impl<Node, EV> EngineValidatorBuilder<Node> for BaseTreeEngineValidatorBuilder<EV>
where
    Node: FullNodeComponents<Types: BaseNodeTypes>,
    BasicEngineValidatorBuilder<EV>: EngineValidatorBuilder<Node>,
    EV: Clone,
{
    type EngineValidator = BaseTreeEngineValidator<
        <BasicEngineValidatorBuilder<EV> as EngineValidatorBuilder<Node>>::EngineValidator,
    >;

    async fn build_tree_validator(
        self,
        ctx: &AddOnsContext<'_, Node>,
        tree_config: TreeConfig,
        overlay_manager: OverlayManager<BasePrimitives>,
    ) -> eyre::Result<Self::EngineValidator> {
        let chain_spec = Arc::clone(&ctx.config.chain);
        let inner = self.0.build_tree_validator(ctx, tree_config, overlay_manager).await?;
        Ok(BaseTreeEngineValidator::new(inner, chain_spec))
    }
}

/// Validates Base metadata before a locally executed block enters engine-tree caches.
#[derive(Debug)]
pub struct BaseTreeEngineValidator<V> {
    inner: V,
    chain_spec: Arc<BaseChainSpec>,
}

impl<V> BaseTreeEngineValidator<V> {
    /// Creates a validator around the stock engine-tree validator.
    pub const fn new(inner: V, chain_spec: Arc<BaseChainSpec>) -> Self {
        Self { inner, chain_spec }
    }
}

impl<V> WaitForCaches for BaseTreeEngineValidator<V>
where
    V: WaitForCaches,
{
    fn wait_for_caches(&self) -> CacheWaitDurations {
        self.inner.wait_for_caches()
    }
}

impl<V> EngineValidator<BaseEngineTypes, BasePrimitives> for BaseTreeEngineValidator<V>
where
    V: EngineValidator<BaseEngineTypes, BasePrimitives>,
{
    fn validate_payload_attributes_against_header(
        &self,
        attr: &BasePayloadBuilderAttributes<BaseTxEnvelope>,
        header: &Header,
    ) -> Result<(), InvalidPayloadAttributesError> {
        self.inner.validate_payload_attributes_against_header(attr, header)
    }

    fn convert_payload_to_block(
        &self,
        payload: ExecutionData,
    ) -> Result<SealedBlock<BaseBlock>, NewPayloadError> {
        self.inner.convert_payload_to_block(payload)
    }

    fn validate_payload(
        &mut self,
        payload: ExecutionData,
        ctx: TreeCtx<'_, BasePrimitives>,
    ) -> ValidationOutcome<BasePrimitives> {
        self.inner.validate_payload(payload, ctx)
    }

    fn validate_block(
        &mut self,
        block: SealedBlock<BaseBlock>,
        ctx: TreeCtx<'_, BasePrimitives>,
    ) -> ValidationOutcome<BasePrimitives> {
        self.inner.validate_block(block, ctx)
    }

    fn on_inserted_executed_block(
        &self,
        block: BuiltPayloadExecutedBlock<BasePrimitives>,
    ) -> ProviderResult<ExecutedBlock<BasePrimitives>> {
        let recovered = &block.recovered_block;
        validate_base_time_metadata(
            &self.chain_spec,
            recovered.header().timestamp(),
            recovered.header().number(),
            &recovered.body().transactions,
        )
        .map_err(ProviderError::other)?;
        self.inner.on_inserted_executed_block(block)
    }

    fn on_canonical_head_changed(&self, hash: B256, state: &EngineApiTreeState<BasePrimitives>) {
        self.inner.on_canonical_head_changed(hash, state);
    }

    fn payload_builder_resources(
        &self,
        parent_hash: B256,
        parent_header: &Header,
        timestamp: u64,
        state: &mut EngineApiTreeState<BasePrimitives>,
    ) -> PayloadBuilderResources {
        self.inner.payload_builder_resources(parent_hash, parent_header, timestamp, state)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_consensus::{BlockBody, Sealable};
    use alloy_primitives::Address;
    use base_common_consensus::TxDeposit;
    use base_protocol::BaseTimeUpdateTx;
    use reth_ethereum_forks::ForkCondition;
    use reth_primitives_traits::{RecoveredBlock, SealedHeader};
    use rstest::rstest;

    use super::*;

    // This trait belongs to reth, so use mock! rather than annotating it with automock.
    mockall::mock! {
        pub Inner {}

        impl EngineValidator<BaseEngineTypes, BasePrimitives> for Inner {
            fn validate_payload_attributes_against_header(&self, attr: &BasePayloadBuilderAttributes<BaseTxEnvelope>, header: &Header) -> Result<(), InvalidPayloadAttributesError>;
            fn convert_payload_to_block(&self, payload: ExecutionData) -> Result<SealedBlock<BaseBlock>, NewPayloadError>;
            fn validate_payload<'a>(&mut self, payload: ExecutionData, ctx: TreeCtx<'a, BasePrimitives>) -> ValidationOutcome<BasePrimitives>;
            fn validate_block<'a>(&mut self, block: SealedBlock<BaseBlock>, ctx: TreeCtx<'a, BasePrimitives>) -> ValidationOutcome<BasePrimitives>;
            fn on_inserted_executed_block(&self, block: BuiltPayloadExecutedBlock<BasePrimitives>) -> ProviderResult<ExecutedBlock<BasePrimitives>>;
            fn on_canonical_head_changed(&self, hash: B256, state: &EngineApiTreeState<BasePrimitives>);
            fn payload_builder_resources(&self, parent_hash: B256, parent_header: &Header, timestamp: u64, state: &mut EngineApiTreeState<BasePrimitives>) -> PayloadBuilderResources;
        }

        impl WaitForCaches for Inner {
            fn wait_for_caches(&self) -> CacheWaitDurations;
        }
    }

    #[rstest]
    #[case(10, 107, Some(0), false)]
    #[case(10, 104, Some(0), false)]
    #[case(11, 106, Some(400), false)]
    #[case(11, 106, None, false)]
    #[case(10, 106, Some(0), true)]
    #[case(11, 106, Some(200), true)]
    #[case(15, 107, Some(0), true)]
    #[case(9, 104, None, true)]
    fn cached_insertion_checks_schedule_before_delegating(
        #[case] number: u64,
        #[case] timestamp: u64,
        #[case] millis: Option<u16>,
        #[case] valid: bool,
    ) {
        let mut chain_spec = BaseChainSpec::mainnet();
        chain_spec.inner.genesis_header =
            SealedHeader::seal_slow(Header { number: 7, timestamp: 100, ..Default::default() });
        chain_spec.set_fork(base_common_chains::BaseUpgrade::Denim, ForkCondition::Timestamp(105));
        let transactions = millis.map_or_else(Vec::new, |millis| {
            vec![
                TxDeposit::default().seal_slow().into(),
                BaseTimeUpdateTx::new(millis).unwrap().into_deposit_tx(number).into(),
            ]
        });
        let signers = vec![Address::ZERO; transactions.len()];
        let block = BuiltPayloadExecutedBlock {
            recovered_block: Arc::new(RecoveredBlock::new_sealed(
                SealedBlock::seal_slow(BaseBlock {
                    header: Header { number, timestamp, ..Default::default() },
                    body: BlockBody { transactions, ..Default::default() },
                }),
                signers,
            )),
            execution_output: Default::default(),
            hashed_state: Default::default(),
            trie_updates: Default::default(),
        };
        let execution_output = Arc::clone(&block.execution_output);
        let recovered_block = Arc::clone(&block.recovered_block);
        let mut inner = MockInner::new();
        inner.expect_on_inserted_executed_block().times(usize::from(valid)).returning(|block| {
            Ok(ExecutedBlock::new(
                block.recovered_block,
                block.execution_output,
                Default::default(),
            ))
        });
        let validator = BaseTreeEngineValidator::new(inner, Arc::new(chain_spec));
        let result = validator.on_inserted_executed_block(block);
        if valid {
            let executed = result.unwrap();
            assert!(Arc::ptr_eq(&executed.execution_output, &execution_output));
            assert!(Arc::ptr_eq(&executed.recovered_block, &recovered_block));
        } else {
            assert!(result.is_err());
        }
    }

    #[test]
    fn forwards_cache_waits() {
        let mut inner = MockInner::new();
        inner.expect_wait_for_caches().once().returning(|| CacheWaitDurations {
            execution_cache: Duration::from_millis(7),
            sparse_trie: Duration::from_millis(11),
        });
        let validator = BaseTreeEngineValidator::new(inner, Arc::new(BaseChainSpec::mainnet()));
        let waits = validator.wait_for_caches();
        assert_eq!(waits.execution_cache, Duration::from_millis(7));
        assert_eq!(waits.sparse_trie, Duration::from_millis(11));
    }
}
