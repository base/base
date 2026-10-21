use alloc::sync::Arc;
use core::{fmt::Debug, marker::PhantomData};

use alloy_consensus::{BlockHeader, Header};
#[cfg(feature = "std")]
use alloy_evm::{EvmFactory, FromRecoveredTx, FromTxWithEncoded};
#[cfg(feature = "std")]
use alloy_primitives::Bytes;
use base_common_chains::Upgrades;
use base_common_consensus::{BasePrimitives, DepositReceiptExt, EIP1559ParamError};
use base_common_evm::{
    BaseBlockExecutionCtx, BaseBlockExecutorFactory, BaseEvmFactory, BaseReceiptBuilder,
    BaseSpecId, BaseTransaction, BaseTxEnv,
};
#[cfg(not(feature = "std"))]
use base_common_rpc_types_engine as _;
#[cfg(feature = "std")]
use base_common_rpc_types_engine::ExecutionData;
use base_execution_chainspec::BaseChainSpec;
use reth_chainspec::EthChainSpec;
#[cfg(feature = "std")]
use reth_evm::{ConfigureEngineEvm, EvmEnvFor, ExecutableTxIterator, ExecutionCtxFor};
use reth_evm::{ConfigureEvm, EvmEnv, TransactionEnvMut, precompiles::PrecompilesMap};
use reth_primitives_traits::{NodePrimitives, SealedBlock, SealedHeader, SignedTransaction};
#[cfg(feature = "std")]
use reth_primitives_traits::{TxTy, WithEncoded};
#[cfg(not(feature = "std"))]
use reth_storage_errors as _;
#[cfg(feature = "std")]
use reth_storage_errors::any::AnyError;
use revm::{
    context::{BlockEnv, TxEnv},
    primitives::{Address, B256, Bytes as RevmBytes},
};

use crate::{BaseBlockAssembler, BaseEvmEnvBuilder, BaseRethReceiptBuilder};

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
    pub extra_data: RevmBytes,
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
pub struct BaseEvmConfig<
    ChainSpec = BaseChainSpec,
    N: NodePrimitives = BasePrimitives,
    R = BaseRethReceiptBuilder,
    EvmFactory = BaseEvmFactory,
> {
    /// Inner [`BaseBlockExecutorFactory`].
    pub executor_factory: BaseBlockExecutorFactory<R, Arc<ChainSpec>, EvmFactory>,
    /// Base block assembler.
    pub block_assembler: BaseBlockAssembler<ChainSpec>,
    #[doc(hidden)]
    pub _pd: PhantomData<N>,
}

/// Helper type with backwards compatible methods to obtain executor providers.
pub type BaseExecutorProvider = BaseEvmConfig;

impl<ChainSpec, N: NodePrimitives, R: Clone, EvmFactory: Clone> Clone
    for BaseEvmConfig<ChainSpec, N, R, EvmFactory>
{
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
        Self::new(chain_spec, BaseRethReceiptBuilder)
    }
}

impl<ChainSpec: Upgrades, N: NodePrimitives, R> BaseEvmConfig<ChainSpec, N, R> {
    /// Creates a new [`BaseEvmConfig`] with the given chain spec.
    pub fn new(chain_spec: Arc<ChainSpec>, receipt_builder: R) -> Self {
        let activation_admin_address = chain_spec.as_ref().activation_admin_address();
        Self {
            block_assembler: BaseBlockAssembler::new(Arc::clone(&chain_spec)),
            executor_factory: BaseBlockExecutorFactory::new(
                receipt_builder,
                chain_spec,
                BaseEvmFactory::new(activation_admin_address),
            ),
            _pd: PhantomData,
        }
    }
}

impl<ChainSpec, N, R, EvmFactory> BaseEvmConfig<ChainSpec, N, R, EvmFactory>
where
    ChainSpec: Upgrades,
    N: NodePrimitives,
{
    /// Returns the chain spec associated with this configuration.
    pub const fn chain_spec(&self) -> &Arc<ChainSpec> {
        self.executor_factory.spec()
    }
}

impl<ChainSpec, N, R, EvmF> ConfigureEvm for BaseEvmConfig<ChainSpec, N, R, EvmF>
where
    ChainSpec: EthChainSpec<Header = Header> + Upgrades,
    N: NodePrimitives<
            Receipt = R::Receipt,
            SignedTx = R::Transaction,
            BlockHeader = Header,
            BlockBody = alloy_consensus::BlockBody<R::Transaction>,
            Block = alloy_consensus::Block<R::Transaction>,
        >,
    BaseTransaction<TxEnv>: FromRecoveredTx<N::SignedTx> + FromTxWithEncoded<N::SignedTx>,
    R: BaseReceiptBuilder<Receipt: DepositReceiptExt, Transaction: SignedTransaction> + Clone,
    EvmF: EvmFactory<
            Tx: FromRecoveredTx<R::Transaction>
                    + FromTxWithEncoded<R::Transaction>
                    + TransactionEnvMut
                    + BaseTxEnv,
            Precompiles = PrecompilesMap,
            Spec = BaseSpecId,
            BlockEnv = BlockEnv,
        > + Debug,
    Self: Send + Sync + Unpin + Clone + 'static,
{
    type Primitives = N;
    type Error = EIP1559ParamError;
    type NextBlockEnvCtx = BaseNextBlockEnvAttributes;
    type BlockExecutorFactory = BaseBlockExecutorFactory<R, Arc<ChainSpec>, EvmF>;
    type BlockAssembler = BaseBlockAssembler<ChainSpec>;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.block_assembler
    }

    fn evm_env(&self, header: &Header) -> Result<EvmEnv<BaseSpecId>, Self::Error> {
        Ok(BaseEvmEnvBuilder::evm_env(header, self.chain_spec()))
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnv<BaseSpecId>, Self::Error> {
        let base_fee =
            self.chain_spec().next_block_base_fee(parent, attributes.timestamp).unwrap_or_default();

        Ok(BaseEvmEnvBuilder::next_evm_env(parent, attributes, base_fee, self.chain_spec()))
    }

    fn context_for_block(
        &self,
        block: &'_ SealedBlock<N::Block>,
    ) -> Result<BaseBlockExecutionCtx, Self::Error> {
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
impl<ChainSpec, N, R> ConfigureEngineEvm<ExecutionData> for BaseEvmConfig<ChainSpec, N, R>
where
    ChainSpec: EthChainSpec<Header = Header> + Upgrades,
    N: NodePrimitives<
            Receipt = R::Receipt,
            SignedTx = R::Transaction,
            BlockHeader = Header,
            BlockBody = alloy_consensus::BlockBody<R::Transaction>,
            Block = alloy_consensus::Block<R::Transaction>,
        >,
    BaseTransaction<TxEnv>: FromRecoveredTx<N::SignedTx> + FromTxWithEncoded<N::SignedTx>,
    R: BaseReceiptBuilder<Receipt: DepositReceiptExt, Transaction: SignedTransaction> + Clone,
    Self: Send + Sync + Unpin + Clone + 'static,
{
    fn evm_env_for_payload(&self, payload: &ExecutionData) -> Result<EvmEnvFor<Self>, Self::Error> {
        Ok(BaseEvmEnvBuilder::payload_evm_env(payload, self.chain_spec()))
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
    use base_common_evm::BaseSpecId;
    use base_common_genesis::BaseUpgrade;
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use reth_chainspec::ChainSpec;
    use reth_evm::{ConfigureEvm, EvmEnv, execute::ProviderError};
    use reth_primitives_traits::constants::MAX_TX_GAS_LIMIT_OSAKA;
    use revm::{
        context::{BlockEnv, CfgEnv},
        database::CacheDB,
        database_interface::EmptyDBTyped,
        inspector::NoOpInspector,
    };

    use super::BaseEvmConfig;

    fn test_evm_config() -> BaseEvmConfig {
        BaseEvmConfig::base(Arc::new(BaseChainSpec::mainnet()))
    }

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
            assert_eq!(env.block_env.timestamp, U256::from(timestamp), "override {time:?}");
            assert_eq!(env.cfg_env.spec, BaseSpecId::new(upgrade), "override {time:?}");
            assert_eq!(env.block_env.basefee, base_fee, "override {time:?}");
        }
    }

    #[test]
    fn test_evm_env_uses_azul_for_genesis_chain_spec() {
        let chain_spec = Arc::new(
            BaseChainSpecBuilder::default()
                .chain(0.into())
                .genesis(Genesis::default())
                .azul_activated()
                .build(),
        );
        let evm_config = BaseEvmConfig::base(chain_spec);
        let header = Header { timestamp: 0, ..Default::default() };
        let EvmEnv { cfg_env, .. } = evm_config.evm_env(&header).unwrap();
        assert_eq!(cfg_env.spec, BaseSpecId::new(BaseUpgrade::Azul));
        assert_eq!(cfg_env.tx_gas_limit_cap, Some(MAX_TX_GAS_LIMIT_OSAKA));
    }

    #[test]
    fn test_fill_cfg_and_block_env() {
        // Create a default header
        let header = Header::default();

        // Build the ChainSpec for Ethereum mainnet, activating London, Paris, and Shanghai
        // upgrades
        let chain_spec = ChainSpec::builder()
            .chain(0.into())
            .genesis(Genesis::default())
            .london_activated()
            .paris_activated()
            .shanghai_activated()
            .build();

        // Use the `BaseEvmConfig` to create the `cfg_env` and `block_env` based on the ChainSpec,
        // Header, and total difficulty
        let EvmEnv { cfg_env, .. } =
            BaseEvmConfig::base(Arc::new(BaseChainSpec::from(chain_spec.clone())))
                .evm_env(&header)
                .unwrap();

        // Assert that the chain ID in the `cfg_env` is correctly set to the chain ID of the
        // ChainSpec
        assert_eq!(cfg_env.chain_id, chain_spec.chain().id());
    }

    #[test]
    fn test_evm_with_env_default_spec() {
        let evm_config = test_evm_config();

        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        let evm_env = EvmEnv::default();

        let evm = evm_config.evm_with_env(db, evm_env.clone());

        // Check that the EVM environment
        assert_eq!(evm.cfg, evm_env.cfg_env);
    }

    #[test]
    fn test_evm_with_env_custom_cfg() {
        let evm_config = test_evm_config();

        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        // Create a custom configuration environment with a chain ID of 111
        let cfg = CfgEnv::new()
            .with_chain_id(111)
            .with_spec_and_mainnet_gas_params(BaseSpecId::default());

        let evm_env = EvmEnv { cfg_env: cfg.clone(), ..Default::default() };

        let evm = evm_config.evm_with_env(db, evm_env);

        // Check that the EVM environment is initialized with the custom environment
        assert_eq!(evm.cfg, cfg);
    }

    #[test]
    fn test_evm_with_env_custom_block_and_tx() {
        let evm_config = test_evm_config();

        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        // Create customs block and tx env
        let block = BlockEnv {
            basefee: 1000,
            gas_limit: 10_000_000,
            number: U256::from(42),
            ..Default::default()
        };

        let evm_env = EvmEnv { block_env: block, ..Default::default() };

        let evm = evm_config.evm_with_env(db, evm_env.clone());

        // Verify that the block and transaction environments are set correctly
        assert_eq!(evm.block, evm_env.block_env);
    }

    #[test]
    fn test_evm_with_spec_id() {
        let evm_config = test_evm_config();

        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        let evm_env = EvmEnv {
            cfg_env: CfgEnv::new()
                .with_spec_and_mainnet_gas_params(BaseSpecId::new(BaseUpgrade::Ecotone)),
            ..Default::default()
        };

        let evm = evm_config.evm_with_env(db, evm_env.clone());

        assert_eq!(evm.cfg, evm_env.cfg_env);
    }

    #[test]
    fn test_evm_with_env_and_default_inspector() {
        let evm_config = test_evm_config();
        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        let evm_env = EvmEnv { cfg_env: Default::default(), ..Default::default() };

        let evm = evm_config.evm_with_env_and_inspector(db, evm_env.clone(), NoOpInspector {});

        // Check that the EVM environment is set to default values
        assert_eq!(evm.block, evm_env.block_env);
        assert_eq!(evm.cfg, evm_env.cfg_env);
    }

    #[test]
    fn test_evm_with_env_inspector_and_custom_cfg() {
        let evm_config = test_evm_config();
        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        let cfg = CfgEnv::new()
            .with_chain_id(111)
            .with_spec_and_mainnet_gas_params(BaseSpecId::default());
        let block = BlockEnv::default();
        let evm_env = EvmEnv { block_env: block, cfg_env: cfg.clone() };

        let evm = evm_config.evm_with_env_and_inspector(db, evm_env.clone(), NoOpInspector {});

        // Check that the EVM environment is set with custom configuration
        assert_eq!(evm.cfg, cfg);
        assert_eq!(evm.block, evm_env.block_env);
    }

    #[test]
    fn test_evm_with_env_inspector_and_custom_block_tx() {
        let evm_config = test_evm_config();
        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        // Create custom block and tx environment
        let block = BlockEnv {
            basefee: 1000,
            gas_limit: 10_000_000,
            number: U256::from(42),
            ..Default::default()
        };
        let evm_env = EvmEnv { block_env: block, ..Default::default() };

        let evm = evm_config.evm_with_env_and_inspector(db, evm_env.clone(), NoOpInspector {});

        // Verify that the block and transaction environments are set correctly
        assert_eq!(evm.block, evm_env.block_env);
    }

    #[test]
    fn test_evm_with_env_inspector_and_spec_id() {
        let evm_config = test_evm_config();
        let db = CacheDB::<EmptyDBTyped<ProviderError>>::default();

        let evm_env = EvmEnv {
            cfg_env: CfgEnv::new()
                .with_spec_and_mainnet_gas_params(BaseSpecId::new(BaseUpgrade::Ecotone)),
            ..Default::default()
        };

        let evm = evm_config.evm_with_env_and_inspector(db, evm_env.clone(), NoOpInspector {});

        // Check that the spec ID is set properly
        assert_eq!(evm.cfg, evm_env.cfg_env);
        assert_eq!(evm.block, evm_env.block_env);
    }
}
