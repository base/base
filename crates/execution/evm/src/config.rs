use alloc::sync::Arc;
use core::marker::PhantomData;

use alloy_consensus::{BlockHeader, Header};
#[cfg(feature = "std")]
use alloy_primitives::Bytes;
use alloy_primitives::{Address, B256};
use base_common_chains::Upgrades;
use base_common_consensus::{
    BasePrimitives, BaseReceipt, BaseTransactionSigned, EIP1559ParamError,
};
use base_common_evm2::BaseBlockExecutionCtx;
#[cfg(not(feature = "std"))]
use base_common_rpc_types_engine as _;
#[cfg(feature = "std")]
use base_common_rpc_types_engine::ExecutionData;
use base_execution_chainspec::BaseChainSpec;
use reth_chainspec::EthChainSpec;
#[cfg(feature = "std")]
use reth_evm::{ConfigureEngineEvm, ExecutableTxIterator, ExecutionCtxFor};
use reth_evm::{ConfigureEvm, EvmEnvFor};
use reth_primitives_traits::{NodePrimitives, SealedBlock, SealedHeader};
#[cfg(feature = "std")]
use reth_primitives_traits::{SignedTransaction, TxTy, WithEncoded};
#[cfg(not(feature = "std"))]
use reth_storage_errors as _;
#[cfg(feature = "std")]
use reth_storage_errors::any::AnyError;

use crate::{BaseBlockAssembler, BaseBlockExecutorFactory, BaseEvmEnvBuilder};

/// Context relevant for execution of a next Base block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseNextBlockEnvAttributes {
    /// The timestamp of the next block.
    pub timestamp: u64,
    /// The suggested fee recipient for the next block.
    pub suggested_fee_recipient: Address,
    /// The randomness value for the next block.
    pub prev_randao: B256,
    /// Block gas limit.
    pub gas_limit: u64,
    /// The parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,
    /// Encoded EIP-1559 parameters to include into block's `extra_data` field.
    pub extra_data: alloy_primitives::Bytes,
}

#[cfg(feature = "rpc")]
impl<H: alloy_consensus::BlockHeader> reth_rpc_eth_api::helpers::pending_block::BuildPendingEnv<H>
    for BaseNextBlockEnvAttributes
{
    fn build_pending_env(
        parent: &SealedHeader<H>,
        block_overrides: Option<&alloy_rpc_types_eth::BlockOverrides>,
    ) -> Self {
        Self {
            timestamp: block_overrides
                .and_then(|overrides| overrides.time)
                .unwrap_or_else(|| parent.timestamp().saturating_add(12)),
            suggested_fee_recipient: parent.beneficiary(),
            prev_randao: B256::random(),
            gas_limit: parent.gas_limit(),
            parent_beacon_block_root: parent.parent_beacon_block_root(),
            extra_data: parent.extra_data().clone(),
        }
    }
}

/// Base EVM configuration.
#[derive(Debug)]
pub struct BaseEvmConfig<ChainSpec = BaseChainSpec, N: NodePrimitives = BasePrimitives> {
    /// Inner [`BaseBlockExecutorFactory`].
    pub executor_factory: BaseBlockExecutorFactory<ChainSpec>,
    /// Base block assembler.
    pub block_assembler: BaseBlockAssembler<ChainSpec>,
    #[doc(hidden)]
    pub _pd: PhantomData<N>,
}

/// Helper type with backwards compatible methods to obtain executor providers.
pub type BaseExecutorProvider = BaseEvmConfig;

impl<ChainSpec, N: NodePrimitives> Clone for BaseEvmConfig<ChainSpec, N> {
    fn clone(&self) -> Self {
        Self {
            executor_factory: self.executor_factory.clone(),
            block_assembler: self.block_assembler.clone(),
            _pd: self._pd,
        }
    }
}

impl<ChainSpec: Upgrades> BaseEvmConfig<ChainSpec> {
    /// Creates a new [`BaseEvmConfig`] with the given chain spec for Base chains.
    pub fn base(chain_spec: Arc<ChainSpec>) -> Self {
        Self::new(chain_spec)
    }
}

impl<ChainSpec: Upgrades, N: NodePrimitives> BaseEvmConfig<ChainSpec, N> {
    /// Creates a new [`BaseEvmConfig`] with the given chain spec.
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self {
            block_assembler: BaseBlockAssembler::new(Arc::clone(&chain_spec)),
            executor_factory: BaseBlockExecutorFactory::new(chain_spec),
            _pd: PhantomData,
        }
    }

    /// Returns the chain spec associated with this configuration.
    pub const fn chain_spec(&self) -> &Arc<ChainSpec> {
        self.executor_factory.chain_spec()
    }
}

impl<ChainSpec, N> ConfigureEvm for BaseEvmConfig<ChainSpec, N>
where
    ChainSpec: EthChainSpec<Header = Header> + Upgrades,
    N: NodePrimitives<
            Receipt = BaseReceipt,
            SignedTx = BaseTransactionSigned,
            BlockHeader = Header,
            BlockBody = alloy_consensus::BlockBody<BaseTransactionSigned>,
            Block = alloy_consensus::Block<BaseTransactionSigned>,
        >,
    Self: Send + Sync + Unpin + Clone + 'static,
{
    type Primitives = N;
    type Error = EIP1559ParamError;
    type NextBlockEnvCtx = BaseNextBlockEnvAttributes;
    type BlockExecutorFactory = BaseBlockExecutorFactory<ChainSpec>;
    type BlockAssembler = BaseBlockAssembler<ChainSpec>;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.block_assembler
    }

    fn evm_env(&self, header: &Header) -> Result<EvmEnvFor<Self>, Self::Error> {
        Ok(BaseEvmEnvBuilder::evm_env(header, self.chain_spec().as_ref()))
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        let base_fee =
            self.chain_spec().next_block_base_fee(parent, attributes.timestamp).unwrap_or_default();

        Ok(BaseEvmEnvBuilder::next_evm_env(
            parent,
            attributes,
            base_fee,
            self.chain_spec().as_ref(),
        ))
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<N::Block>,
    ) -> Result<BaseBlockExecutionCtx, Self::Error>
    where
        Self: 'a,
    {
        Ok(BaseBlockExecutionCtx {
            parent_hash: block.header().parent_hash(),
            parent_beacon_block_root: block.header().parent_beacon_block_root(),
            extra_data: block.header().extra_data().clone(),
        })
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader<N::BlockHeader>,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<BaseBlockExecutionCtx, Self::Error> {
        Ok(BaseBlockExecutionCtx {
            parent_hash: parent.hash(),
            parent_beacon_block_root: attributes.parent_beacon_block_root,
            extra_data: attributes.extra_data,
        })
    }
}

#[cfg(feature = "std")]
impl<ChainSpec, N> ConfigureEngineEvm<ExecutionData> for BaseEvmConfig<ChainSpec, N>
where
    ChainSpec: EthChainSpec<Header = Header> + Upgrades,
    N: NodePrimitives<
            Receipt = BaseReceipt,
            SignedTx = BaseTransactionSigned,
            BlockHeader = Header,
            BlockBody = alloy_consensus::BlockBody<BaseTransactionSigned>,
            Block = alloy_consensus::Block<BaseTransactionSigned>,
        >,
    Self: Send + Sync + Unpin + Clone + 'static,
{
    fn evm_env_for_payload(&self, payload: &ExecutionData) -> Result<EvmEnvFor<Self>, Self::Error> {
        Ok(BaseEvmEnvBuilder::payload_evm_env(payload, self.chain_spec().as_ref()))
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a ExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        Ok(BaseBlockExecutionCtx {
            parent_hash: payload.parent_hash(),
            parent_beacon_block_root: payload.sidecar.parent_beacon_block_root(),
            extra_data: payload.payload.as_v1().extra_data.clone(),
        })
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        let transactions = payload.payload.transactions().clone();
        let convert = |encoded: Bytes| {
            let tx = base_common_consensus::decode_2718_canonical::<TxTy<Self::Primitives>>(
                encoded.as_ref(),
            )
            .map_err(AnyError::new)?;
            let signer = tx.try_recover().map_err(AnyError::new)?;
            Ok::<_, AnyError>(WithEncoded::new(encoded, tx.with_signer(signer)))
        };

        Ok((transactions, convert))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::Header;
    use alloy_genesis::Genesis;
    use alloy_primitives::U256;
    use base_common_evm2::BaseSpecId;
    use base_common_genesis::BaseUpgrade;
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use evm2::evm::InMemoryDB;
    use reth_chainspec::ChainSpec;
    use reth_evm::{ConfigureEvm, EvmEnv};
    use reth_primitives_traits::constants::MAX_TX_GAS_LIMIT_OSAKA;

    use super::BaseEvmConfig;
    use crate::BaseEvmEnv;

    #[cfg(feature = "rpc")]
    #[test]
    fn pending_timestamp_override_selects_matching_fork_and_base_fee() {
        use alloy_rpc_types_eth::BlockOverrides;
        use reth_chainspec::{
            BaseFeeParams, BaseFeeParamsKind, EthereumHardfork, ForkCondition, Hardfork,
        };
        use reth_primitives_traits::SealedHeader;
        use reth_rpc_eth_api::helpers::pending_block::BuildPendingEnv;

        use super::BaseNextBlockEnvAttributes;

        let mut chain_spec = BaseChainSpecBuilder::default()
            .chain(0.into())
            .genesis(Genesis::default())
            .canyon_activated()
            .with_fork(BaseUpgrade::Azul, ForkCondition::Timestamp(10))
            .with_fork(EthereumHardfork::Osaka, ForkCondition::Timestamp(10))
            .build();
        chain_spec.inner.base_fee_params = BaseFeeParamsKind::Variable(
            vec![
                (EthereumHardfork::London.boxed(), BaseFeeParams::new(10, 2)),
                (BaseUpgrade::Azul.boxed(), BaseFeeParams::new(50, 2)),
            ]
            .into(),
        );
        let config = BaseEvmConfig::base(Arc::new(chain_spec));
        let parent = SealedHeader::seal_slow(Header {
            timestamp: 1,
            gas_limit: 20_000_000,
            base_fee_per_gas: Some(1_000),
            ..Default::default()
        });

        // An empty parent reduces the fee by 1/10 before Azul and 1/50 afterwards.
        for (time, timestamp, upgrade, base_fee) in [
            (Some(0), 0_u64, BaseUpgrade::Canyon, 900),
            (Some(1), 1, BaseUpgrade::Canyon, 900),
            (Some(9), 9, BaseUpgrade::Canyon, 900),
            (Some(10), 10, BaseUpgrade::Azul, 980),
            (Some(40), 40, BaseUpgrade::Azul, 980),
            (None, 13, BaseUpgrade::Azul, 980),
        ] {
            let overrides = BlockOverrides { time, ..Default::default() };
            let attributes =
                BaseNextBlockEnvAttributes::build_pending_env(&parent, Some(&overrides));
            let env = config.next_evm_env(&parent, &attributes).unwrap();
            assert_eq!(env.block.timestamp, U256::from(timestamp), "override {time:?}");
            assert_eq!(env.spec, BaseSpecId::new(upgrade), "override {time:?}");
            assert_eq!(env.block.basefee, U256::from(base_fee), "override {time:?}");
        }
    }

    #[test]
    fn evm_env_caps_transaction_gas_from_azul() {
        let chain_spec = |builder: BaseChainSpecBuilder| {
            Arc::new(builder.chain(0.into()).genesis(Genesis::default()).build())
        };
        let header = Header::default();

        let azul =
            BaseEvmConfig::base(chain_spec(BaseChainSpecBuilder::default().azul_activated()))
                .evm_env(&header)
                .unwrap();
        assert_eq!(azul.spec, BaseSpecId::new(BaseUpgrade::Azul));
        assert_eq!(azul.version.tx_gas_limit_cap, MAX_TX_GAS_LIMIT_OSAKA);
        assert_eq!(azul.transaction_validation_limits().tx_gas_limit_cap, MAX_TX_GAS_LIMIT_OSAKA);

        let jovian =
            BaseEvmConfig::base(chain_spec(BaseChainSpecBuilder::default().jovian_activated()))
                .evm_env(&header)
                .unwrap();
        assert_eq!(jovian.spec, BaseSpecId::new(BaseUpgrade::Jovian));
        assert_eq!(jovian.version.tx_gas_limit_cap, u64::MAX);
        assert_eq!(jovian.transaction_validation_limits().tx_gas_limit_cap, 0);
    }

    #[test]
    fn evm_env_reads_minimum_blob_base_fee() {
        let chain_spec = BaseChainSpecBuilder::default()
            .chain(0.into())
            .genesis(Genesis::default())
            .ecotone_activated()
            .build();

        let env = BaseEvmConfig::base(Arc::new(chain_spec)).evm_env(&Header::default()).unwrap();

        assert_eq!(env.block_blob_base_fee(), 1);
    }

    #[test]
    fn evm_env_uses_the_chain_id() {
        let chain_spec = ChainSpec::builder()
            .chain(0.into())
            .genesis(Genesis::default())
            .london_activated()
            .paris_activated()
            .shanghai_activated()
            .build();

        let env = BaseEvmConfig::base(Arc::new(BaseChainSpec::from(chain_spec.clone())))
            .evm_env(&Header::default())
            .unwrap();

        assert_eq!(env.chain_id(), chain_spec.chain().id());
    }

    #[test]
    fn evm_with_env_runs_in_the_given_environment() {
        let evm_config = BaseEvmConfig::base(Arc::new(BaseChainSpec::mainnet()));
        let env = BaseEvmEnv::new(
            BaseSpecId::new(BaseUpgrade::Ecotone),
            evm2::env::BlockEnv::<base_common_evm2::BaseEvmTypes> {
                basefee: U256::from(1000),
                gas_limit: U256::from(10_000_000),
                number: U256::from(42),
                ..Default::default()
            },
            111,
        )
        .with_nonce_check_disabled();

        let evm = evm_config.evm_with_env(InMemoryDB::default(), env.clone());

        assert_eq!(evm.config_spec_id(), env.spec);
        assert_eq!(*evm.block(), env.block);
        assert_eq!(evm.version().chain_id, 111);
        assert!(!evm.feature(evm2::EvmFeatures::NONCE_CHECK));
    }
}
