use alloc::{sync::Arc, vec::Vec};
use core::fmt::Debug;

#[cfg(feature = "std")]
#[cfg(feature = "std")]
use alloy_primitives::Bytes;
use base_common_chain_config::BaseChainSpec;
use base_common_chain_config::Upgrades;
use base_common_types_chain::{BaseTxEnvelope, BlockHeader, EIP1559ParamError, Header};
#[cfg(not(feature = "std"))]
use base_common_types_payload as _;
#[cfg(feature = "std")]
use base_common_types_payload::ExecutionData;
use base_execution_evm_runtime::{
    BaseBlockExecutionCtx, BaseBlockExecutorFactory, BaseEvmFactory, BaseSpecId,
};
use base_execution_evm_runtime::{
    BlockExecutionError, BlockExecutorFactory, BlockExecutorFor, Database, EvmFactory, IntoTxEnv,
};
use base_execution_evm_runtime::{
    database::State,
    primitives::{Address, B256, Bytes as RevmBytes},
};
#[cfg(feature = "std")]
use reth_primitives_traits::WithEncoded;
use reth_primitives_traits::{SealedBlock, SealedHeader, SignedTransaction};
#[cfg(not(feature = "std"))]
use reth_storage_errors as _;
#[cfg(feature = "std")]
use reth_storage_errors::any::AnyError;

#[cfg(feature = "std")]
use crate::ExecutableTxIterator;
use crate::{
    BaseBlockAssembler, BaseEvmEnvBuilder, BlockExecutorForEvm, EvmEnv, EvmEnvFor, EvmFactoryFor,
    EvmFor, InspectorFor, TxEnvFor,
    execute::{BasicBlockBuilder, BasicBlockExecutor, BlockBuilder, Executor},
};

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

/// Executor factory used by the Base node.
pub type BaseExecutorFactory = BaseBlockExecutorFactory<Arc<BaseChainSpec>, BaseEvmFactory>;

/// Base EVM configuration.
#[derive(Debug, Clone)]
pub struct BaseEvmConfig {
    /// Factory for Base block executors.
    pub executor_factory: BaseExecutorFactory,
    /// Base block assembler.
    pub block_assembler: BaseBlockAssembler,
}

impl Default for BaseEvmConfig {
    fn default() -> Self {
        Self::new(Arc::new(BaseChainSpec::mainnet()))
    }
}

impl BaseEvmConfig {
    /// Creates an EVM configuration for the supplied Base chain.
    pub fn new(chain_spec: Arc<BaseChainSpec>) -> Self {
        let activation_admin_address = chain_spec.activation_admin_address();
        Self {
            block_assembler: BaseBlockAssembler::new(Arc::clone(&chain_spec)),
            executor_factory: BaseBlockExecutorFactory::new(
                chain_spec,
                BaseEvmFactory::new(activation_admin_address),
            ),
        }
    }

    /// Returns the chain specification used by this EVM configuration.
    pub const fn chain_spec(&self) -> &Arc<BaseChainSpec> {
        self.executor_factory.spec()
    }

    /// Returns the Base executor factory.
    pub fn block_executor_factory(&self) -> &BaseExecutorFactory {
        &self.executor_factory
    }

    /// Returns the Base block assembler.
    pub fn block_assembler(&self) -> &BaseBlockAssembler {
        &self.block_assembler
    }

    /// Builds the execution environment for a block header.
    pub fn evm_env(&self, header: &Header) -> Result<EvmEnv<BaseSpecId>, EIP1559ParamError> {
        Ok(BaseEvmEnvBuilder::evm_env(header, self.chain_spec()))
    }

    /// Builds the execution environment for the next block.
    pub fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &BaseNextBlockEnvAttributes,
    ) -> Result<EvmEnv<BaseSpecId>, EIP1559ParamError> {
        let base_fee =
            self.chain_spec().next_block_base_fee(parent, attributes.timestamp).unwrap_or_default();

        Ok(BaseEvmEnvBuilder::next_evm_env(parent, attributes, base_fee, self.chain_spec()))
    }

    /// Builds the context for an existing block.
    pub fn context_for_block(
        &self,
        block: &'_ SealedBlock,
    ) -> Result<BaseBlockExecutionCtx, EIP1559ParamError> {
        Ok(BaseBlockExecutionCtx {
            parent_hash: block.header().parent_hash(),
            parent_beacon_block_root: block.header().parent_beacon_block_root(),
            extra_data: block.header().extra_data().clone(),
        })
    }

    /// Builds the context for the next block.
    pub fn context_for_next_block(
        &self,
        parent: &SealedHeader,
        attributes: BaseNextBlockEnvAttributes,
    ) -> Result<BaseBlockExecutionCtx, EIP1559ParamError> {
        Ok(BaseBlockExecutionCtx {
            parent_hash: parent.hash(),
            parent_beacon_block_root: attributes.parent_beacon_block_root,
            extra_data: attributes.extra_data,
        })
    }

    #[cfg(feature = "std")]
    /// Builds the execution environment for a Base payload.
    pub fn evm_env_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<EvmEnvFor, EIP1559ParamError> {
        Ok(BaseEvmEnvBuilder::payload_evm_env(payload, self.chain_spec()))
    }

    #[cfg(feature = "std")]
    /// Builds the execution context for a Base payload.
    pub fn context_for_payload<'a>(
        &self,
        payload: &'a ExecutionData,
    ) -> Result<crate::ExecutionCtxFor, EIP1559ParamError> {
        Ok(BaseBlockExecutionCtx {
            parent_hash: payload.parent_hash(),
            parent_beacon_block_root: payload.sidecar.parent_beacon_block_root(),
            extra_data: payload.payload.as_v1().extra_data.clone(),
        })
    }

    #[cfg(feature = "std")]
    /// Decodes and recovers the transactions in a Base payload.
    pub fn tx_iterator_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<impl ExecutableTxIterator, EIP1559ParamError> {
        let transactions = payload.payload.transactions().clone();
        let convert = |encoded: Bytes| {
            let tx =
                base_common_types_chain::decode_2718_canonical::<BaseTxEnvelope>(encoded.as_ref())
                    .map_err(AnyError::new)?;
            let signer = tx.try_recover().map_err(AnyError::new)?;
            Ok::<_, AnyError>(WithEncoded::new(encoded, tx.with_signer(signer)))
        };

        Ok((transactions, convert))
    }

    /// Returns a [`EvmFactory::Tx`] from a transaction.
    pub fn tx_env(&self, transaction: impl IntoTxEnv<TxEnvFor>) -> TxEnvFor {
        transaction.into_tx_env()
    }

    /// Provides a reference to [`EvmFactory`] implementation.
    pub fn evm_factory(&self) -> &EvmFactoryFor {
        self.block_executor_factory().evm_factory()
    }

    /// Returns a new EVM with the given database configured with the given environment settings,
    /// including the spec id and transaction environment.
    ///
    /// This will preserve any handler modifications
    pub fn evm_with_env<DB: Database>(&self, db: DB, evm_env: EvmEnvFor) -> EvmFor<DB> {
        self.evm_factory().create_evm(db, evm_env)
    }

    /// Returns a new EVM with the given database configured with `cfg` and `block_env`
    /// configuration derived from the given header. Relies on
    /// [`BaseEvmConfig::evm_env`].
    ///
    /// # Caution
    ///
    /// This does not initialize the tx environment.
    pub fn evm_for_block<DB: Database>(
        &self,
        db: DB,
        header: &base_common_types_chain::Header,
    ) -> Result<EvmFor<DB>, EIP1559ParamError> {
        let evm_env = self.evm_env(header)?;
        Ok(self.evm_with_env(db, evm_env))
    }

    /// Returns a new EVM with the given database configured with the given environment settings,
    /// including the spec id.
    ///
    /// This will use the given external inspector as the EVM external context.
    ///
    /// This will preserve any handler modifications
    pub fn evm_with_env_and_inspector<DB, I>(
        &self,
        db: DB,
        evm_env: EvmEnvFor,
        inspector: I,
    ) -> EvmFor<DB, I>
    where
        DB: Database,
        I: InspectorFor<DB>,
    {
        self.evm_factory().create_evm_with_inspector(db, evm_env, inspector)
    }

    /// Creates a strategy with given EVM and execution context.
    pub fn create_executor<'a, DB, I>(
        &'a self,
        evm: EvmFor<&'a mut State<DB>, I>,
        ctx: <BaseExecutorFactory as BlockExecutorFactory>::ExecutionCtx<'a>,
    ) -> BlockExecutorForEvm<'a, DB, I>
    where
        DB: Database,
        I: InspectorFor<&'a mut State<DB>> + 'a,
    {
        self.block_executor_factory().create_executor(evm, ctx)
    }

    /// Creates a strategy with a DB state borrow that can be shorter than the execution context.
    pub fn create_executor_with_state<'a, 'db, DB, I>(
        &'a self,
        evm: EvmFor<&'db mut State<DB>, I>,
        ctx: <BaseExecutorFactory as BlockExecutorFactory>::ExecutionCtx<'a>,
    ) -> BlockExecutorFor<'a, BaseExecutorFactory, &'db mut State<DB>, I>
    where
        DB: Database,
        I: InspectorFor<&'db mut State<DB>>,
    {
        self.block_executor_factory().create_executor(evm, ctx)
    }

    /// Creates a strategy for execution of a given block.
    pub fn executor_for_block<'a, DB: Database>(
        &'a self,
        db: &'a mut State<DB>,
        block: &'a SealedBlock,
    ) -> Result<BlockExecutorForEvm<'a, DB>, EIP1559ParamError> {
        let evm = self.evm_for_block(db, block.header())?;
        let ctx = self.context_for_block(block)?;
        Ok(self.create_executor(evm, ctx))
    }

    /// Creates a [`BlockBuilder`]. Should be used when building a new block.
    ///
    /// Block builder wraps an inner [`base_execution_evm_runtime::BlockExecutor`] and has a similar
    /// interface. Builder collects all of the executed transactions, and once
    /// [`BlockBuilder::finish`] is called, it invokes the configured [`crate::BaseBlockAssembler`] to
    /// create a block.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Create a builder with specific EVM configuration
    /// let evm = evm_config.evm_with_env(&mut state_db, evm_env);
    /// let ctx = evm_config.context_for_next_block(&parent, attributes);
    /// let builder = evm_config.create_block_builder(evm, &parent, ctx);
    /// ```
    pub fn create_block_builder<'a, DB, I>(
        &'a self,
        evm: EvmFor<&'a mut State<DB>, I>,
        parent: &'a SealedHeader,
        ctx: <BaseExecutorFactory as BlockExecutorFactory>::ExecutionCtx<'a>,
    ) -> impl BlockBuilder<Executor = BlockExecutorForEvm<'a, DB, I>>
    where
        DB: Database,
        I: InspectorFor<&'a mut State<DB>> + 'a,
    {
        BasicBlockBuilder {
            executor: self.create_executor(evm, ctx.clone()),
            ctx,
            assembler: self.block_assembler(),
            parent,
            transactions: Vec::new(),
        }
    }

    /// Creates a [`BlockBuilder`] for building of a new block. This is a helper to invoke
    /// [`BaseEvmConfig::create_block_builder`].
    ///
    /// This is the primary method for building new blocks. It combines:
    /// 1. Creating the EVM environment for the next block
    /// 2. Setting up the execution context from attributes
    /// 3. Initializing the block builder with proper configuration
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Build a block with specific attributes
    /// let mut builder = evm_config.builder_for_next_block(
    ///     &mut state_db,
    ///     &parent_header,
    ///     attributes
    /// )?;
    ///
    /// // Execute system calls (e.g., beacon root update)
    /// builder.apply_pre_execution_changes()?;
    ///
    /// // Execute transactions
    /// for tx in transactions {
    ///     builder.execute_transaction(tx)?;
    /// }
    ///
    /// // Complete block building
    /// let outcome = builder.finish(state_provider, None)?;
    /// ```
    pub fn builder_for_next_block<'a, DB: Database + 'a>(
        &'a self,
        db: &'a mut State<DB>,
        parent: &'a SealedHeader,
        attributes: BaseNextBlockEnvAttributes,
    ) -> Result<impl BlockBuilder<Executor = BlockExecutorForEvm<'a, DB>>, EIP1559ParamError> {
        let evm_env = self.next_evm_env(parent, &attributes)?;
        let evm = self.evm_with_env(db, evm_env);
        let ctx = self.context_for_next_block(parent, attributes)?;
        Ok(self.create_block_builder(evm, parent, ctx))
    }

    /// Returns a new [`Executor`] for executing blocks.
    ///
    /// The executor processes complete blocks including:
    /// - All transactions in order
    /// - Block rewards and fees
    /// - Block level system calls
    /// - State transitions
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Create an executor
    /// let mut executor = evm_config.executor(state_db);
    ///
    /// // Execute a single block
    /// let output = executor.execute(&block)?;
    ///
    /// // Execute multiple blocks
    /// let batch_output = executor.execute_batch(&blocks)?;
    /// ```
    pub fn executor<DB: Database>(&self, db: DB) -> impl Executor<DB, Error = BlockExecutionError> {
        BasicBlockExecutor::new(self.clone(), db)
    }

    /// Returns a new [`BasicBlockExecutor`].
    pub fn batch_executor<DB: Database>(
        &self,
        db: DB,
    ) -> impl Executor<DB, Error = BlockExecutionError> {
        BasicBlockExecutor::new(self.clone(), db)
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;
    use std::sync::Arc;

    use alloy_eips::eip7685::Requests;
    use alloy_genesis::Genesis;
    use alloy_primitives::{
        Address, B256, LogData, U256, bytes,
        map::{AddressMap, B256Map, HashMap},
    };
    use base_common_chain_config::BaseUpgrade;
    use base_common_chain_config::{BaseChainSpec, BaseChainSpecBuilder};
    use base_common_types_chain::{BaseBlock, BaseReceipt, Header, Receipt};
    use base_evm_context::{BlockEnv, CfgEnv};
    use base_execution_evm_runtime::BaseSpecId;
    use base_execution_evm_runtime::NoOpInspector;
    use base_execution_evm_runtime::{
        database::EmptyDBTyped,
        database::{BundleState, CacheDB},
        primitives::Log,
        state::AccountInfo,
    };
    use reth_execution_types::{
        AccountRevertInit, BundleStateInit, Chain, ExecutionOutcome, RevertsInit,
    };
    use reth_primitives_traits::{Account, RecoveredBlock, constants::MAX_TX_GAS_LIMIT_OSAKA};

    use super::BaseEvmConfig;
    use crate::{EvmEnv, execute::ProviderError};

    fn test_evm_config() -> BaseEvmConfig {
        BaseEvmConfig::new(Arc::new(BaseChainSpec::mainnet()))
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
        let evm_config = BaseEvmConfig::new(chain_spec);
        let header = Header { timestamp: 0, ..Default::default() };
        let EvmEnv { cfg_env, .. } = evm_config.evm_env(&header).unwrap();
        assert_eq!(cfg_env.spec, BaseSpecId::new(BaseUpgrade::Azul));
        assert_eq!(cfg_env.tx_gas_limit_cap, Some(MAX_TX_GAS_LIMIT_OSAKA));
    }

    #[test]
    fn test_fill_cfg_and_block_env() {
        // Create a default header
        let header = Header::default();

        // Build the BaseChainSpec for Ethereum mainnet, activating London, Paris, and Shanghai
        // upgrades
        let chain_spec = base_common_chain_config::BaseChainSpecBuilder::default()
            .chain(0.into())
            .genesis(Genesis::default())
            .bedrock_activated()
            .bedrock_activated()
            .canyon_activated()
            .build();

        // Use the `BaseEvmConfig` to create the `cfg_env` and `block_env` based on the BaseChainSpec,
        // Header, and total difficulty
        let EvmEnv { cfg_env, .. } =
            BaseEvmConfig::new(Arc::new(chain_spec.clone())).evm_env(&header).unwrap();

        // Assert that the chain ID in the `cfg_env` is correctly set to the chain ID of the
        // BaseChainSpec
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

    #[test]
    fn receipts_by_block_hash() {
        let block1_hash = B256::new([0x01; 32]);
        let block2_hash = B256::new([0x02; 32]);
        let block1 = RecoveredBlock::new(
            BaseBlock { header: Header { number: 10, ..Default::default() }, ..Default::default() },
            vec![],
            block1_hash,
        );
        let block2 = RecoveredBlock::new(
            BaseBlock { header: Header { number: 11, ..Default::default() }, ..Default::default() },
            vec![],
            block2_hash,
        );

        // Create a random receipt object, receipt1
        let receipt1 = BaseReceipt::Legacy(Receipt::<Log> {
            cumulative_gas_used: 46913,
            logs: vec![],
            status: true.into(),
        });

        // Create another random receipt object, receipt2
        let receipt2 = BaseReceipt::Legacy(Receipt::<Log> {
            cumulative_gas_used: 1325345,
            logs: vec![],
            status: true.into(),
        });

        // Create a Receipts object with a vector of receipt vectors
        let receipts = vec![vec![receipt1.clone()], vec![receipt2]];

        // Create an ExecutionOutcome object with the created bundle, receipts, an empty requests
        // vector, and first_block set to 10
        let execution_outcome = ExecutionOutcome {
            bundle: Default::default(),
            receipts,
            requests: vec![],
            first_block: 10,
        };

        // Create a Chain object with a BTreeMap of blocks mapped to their block numbers,
        // including block1_hash and block2_hash, and the execution_outcome
        let chain: Chain = Chain::new([block1, block2], execution_outcome.clone(), BTreeMap::new());

        // Assert that the proper receipt vector is returned for block1_hash
        assert_eq!(chain.receipts_by_block_hash(block1_hash), Some(vec![&receipt1]));

        // Create an ExecutionOutcome object with a single receipt vector containing receipt1
        let execution_outcome1 = ExecutionOutcome {
            bundle: Default::default(),
            receipts: vec![vec![receipt1]],
            requests: vec![],
            first_block: 10,
        };

        // Assert that the execution outcome at the first block contains only the first receipt
        assert_eq!(chain.execution_outcome_at_block(10), Some(execution_outcome1));

        // Assert that the execution outcome at the tip block contains the whole execution outcome
        assert_eq!(chain.execution_outcome_at_block(11), Some(execution_outcome));
    }

    #[test]
    fn test_initialization() {
        // Create a new BundleState object with initial data
        let bundle = BundleState::new(
            vec![(Address::new([2; 20]), None, Some(AccountInfo::default()), HashMap::default())],
            vec![vec![(Address::new([2; 20]), None, vec![])]],
            vec![],
        );

        // Create a Receipts object with a vector of receipt vectors
        let receipts = vec![vec![BaseReceipt::Legacy(Receipt::<Log> {
            cumulative_gas_used: 46913,
            logs: vec![],
            status: true.into(),
        })]];

        // Create a Requests object with a vector of requests
        let requests = vec![Requests::new(vec![bytes!("dead"), bytes!("beef"), bytes!("beebee")])];

        // Define the first block number
        let first_block = 123;

        // Create a ExecutionOutcome object with the created bundle, receipts, requests, and
        // first_block
        let exec_res = ExecutionOutcome {
            bundle: bundle.clone(),
            receipts: receipts.clone(),
            requests: requests.clone(),
            first_block,
        };

        // Assert that creating a new ExecutionOutcome using the constructor matches exec_res
        assert_eq!(
            ExecutionOutcome::new(bundle, receipts.clone(), first_block, requests.clone()),
            exec_res
        );

        // Create a BundleStateInit object and insert initial data
        let mut state_init: BundleStateInit = AddressMap::default();
        state_init
            .insert(Address::new([2; 20]), (None, Some(Account::default()), B256Map::default()));

        // Create an AddressMap for account reverts and insert initial data
        let mut revert_inner: AddressMap<AccountRevertInit> = AddressMap::default();
        revert_inner.insert(Address::new([2; 20]), (None, vec![]));

        // Create a RevertsInit object and insert the revert_inner data
        let mut revert_init: RevertsInit = HashMap::default();
        revert_init.insert(123, revert_inner);

        // Assert that creating a new ExecutionOutcome using the new_init method matches
        // exec_res
        assert_eq!(
            ExecutionOutcome::new_init(
                state_init,
                revert_init,
                vec![],
                receipts,
                first_block,
                requests,
            ),
            exec_res
        );
    }

    #[test]
    fn test_block_number_to_index() {
        // Create a Receipts object with a vector of receipt vectors
        let receipts = vec![vec![BaseReceipt::Legacy(Receipt::<Log> {
            cumulative_gas_used: 46913,
            logs: vec![],
            status: true.into(),
        })]];

        // Define the first block number
        let first_block = 123;

        // Create a ExecutionOutcome object with the created bundle, receipts, requests, and
        // first_block
        let exec_res = ExecutionOutcome {
            bundle: Default::default(),
            receipts,
            requests: vec![],
            first_block,
        };

        // Test before the first block
        assert_eq!(exec_res.block_number_to_index(12), None);

        // Test after the first block but index larger than receipts length
        assert_eq!(exec_res.block_number_to_index(133), None);

        // Test after the first block
        assert_eq!(exec_res.block_number_to_index(123), Some(0));
    }

    #[test]
    fn test_get_logs() {
        // Create a Receipts object with a vector of receipt vectors
        let receipts = vec![vec![BaseReceipt::Legacy(Receipt::<Log> {
            cumulative_gas_used: 46913,
            logs: vec![Log::<LogData>::default()],
            status: true.into(),
        })]];

        // Define the first block number
        let first_block = 123;

        // Create a ExecutionOutcome object with the created bundle, receipts, requests, and
        // first_block
        let exec_res = ExecutionOutcome {
            bundle: Default::default(),
            receipts,
            requests: vec![],
            first_block,
        };

        // Get logs for block number 123
        let logs: Vec<&Log> = exec_res.logs(123).unwrap().collect();

        // Assert that the logs match the expected logs
        assert_eq!(logs, vec![&Log::<LogData>::default()]);
    }

    #[test]
    fn test_receipts_by_block() {
        // Create a Receipts object with a vector of receipt vectors
        let receipts = vec![vec![BaseReceipt::Legacy(Receipt::<Log> {
            cumulative_gas_used: 46913,
            logs: vec![Log::<LogData>::default()],
            status: true.into(),
        })]];

        // Define the first block number
        let first_block = 123;

        // Create a ExecutionOutcome object with the created bundle, receipts, requests, and
        // first_block
        let exec_res = ExecutionOutcome {
            bundle: Default::default(), // Default value for bundle
            receipts,                   // Include the created receipts
            requests: vec![],           // Empty vector for requests
            first_block,                // Set the first block number
        };

        // Get receipts for block number 123 and convert the result into a vector
        let receipts_by_block: Vec<_> = exec_res.receipts_by_block(123).iter().collect();

        // Assert that the receipts for block number 123 match the expected receipts
        assert_eq!(
            receipts_by_block,
            vec![&BaseReceipt::Legacy(Receipt::<Log> {
                cumulative_gas_used: 46913,
                logs: vec![Log::<LogData>::default()],
                status: true.into(),
            })]
        );
    }

    #[test]
    fn test_receipts_len() {
        // Create a Receipts object with a vector of receipt vectors
        let receipts = vec![vec![BaseReceipt::Legacy(Receipt::<Log> {
            cumulative_gas_used: 46913,
            logs: vec![Log::<LogData>::default()],
            status: true.into(),
        })]];

        // Create an empty Receipts object
        let receipts_empty = vec![];

        // Define the first block number
        let first_block = 123;

        // Create a ExecutionOutcome object with the created bundle, receipts, requests, and
        // first_block
        let exec_res = ExecutionOutcome {
            bundle: Default::default(), // Default value for bundle
            receipts,                   // Include the created receipts
            requests: vec![],           // Empty vector for requests
            first_block,                // Set the first block number
        };

        // Assert that the length of receipts in exec_res is 1
        assert_eq!(exec_res.len(), 1);

        // Assert that exec_res is not empty
        assert!(!exec_res.is_empty());

        // Create a ExecutionOutcome object with an empty Receipts object
        let exec_res_empty_receipts: ExecutionOutcome = ExecutionOutcome {
            bundle: Default::default(), // Default value for bundle
            receipts: receipts_empty,   // Include the empty receipts
            requests: vec![],           // Empty vector for requests
            first_block,                // Set the first block number
        };

        // Assert that the length of receipts in exec_res_empty_receipts is 0
        assert_eq!(exec_res_empty_receipts.len(), 0);

        // Assert that exec_res_empty_receipts is empty
        assert!(exec_res_empty_receipts.is_empty());
    }

    #[test]
    fn test_revert_to() {
        // Create a random receipt object
        let receipt = BaseReceipt::Legacy(Receipt::<Log> {
            cumulative_gas_used: 46913,
            logs: vec![],
            status: true.into(),
        });

        // Create a Receipts object with a vector of receipt vectors
        let receipts = vec![vec![receipt.clone()], vec![receipt.clone()]];

        // Define the first block number
        let first_block = 123;

        // Create a request.
        let request = bytes!("deadbeef");

        // Create a vector of Requests containing the request.
        let requests =
            vec![Requests::new(vec![request.clone()]), Requests::new(vec![request.clone()])];

        // Create a ExecutionOutcome object with the created bundle, receipts, requests, and
        // first_block
        let mut exec_res =
            ExecutionOutcome { bundle: Default::default(), receipts, requests, first_block };

        // Assert that the revert_to method returns true when reverting to the initial block number.
        assert!(exec_res.revert_to(123));

        // Assert that the receipts are properly cut after reverting to the initial block number.
        assert_eq!(exec_res.receipts, vec![vec![receipt]]);

        // Assert that the requests are properly cut after reverting to the initial block number.
        assert_eq!(exec_res.requests, vec![Requests::new(vec![request])]);

        // Assert that the revert_to method returns false when attempting to revert to a block
        // number greater than the initial block number.
        assert!(!exec_res.revert_to(133));

        // Assert that the revert_to method returns false when attempting to revert to a block
        // number less than the initial block number.
        assert!(!exec_res.revert_to(10));
    }

    #[test]
    fn test_extend_execution_outcome() {
        // Create a Receipt object with specific attributes.
        let receipt = BaseReceipt::Legacy(Receipt::<Log> {
            cumulative_gas_used: 46913,
            logs: vec![],
            status: true.into(),
        });

        // Create a Receipts object containing the receipt.
        let receipts = vec![vec![receipt.clone()]];

        // Create a request.
        let request = bytes!("deadbeef");

        // Create a vector of Requests containing the request.
        let requests = vec![Requests::new(vec![request.clone()])];

        // Define the initial block number.
        let first_block = 123;

        // Create an ExecutionOutcome object.
        let mut exec_res =
            ExecutionOutcome { bundle: Default::default(), receipts, requests, first_block };

        // Extend the ExecutionOutcome object by itself.
        exec_res.extend(exec_res.clone());

        // Assert the extended ExecutionOutcome matches the expected outcome.
        assert_eq!(
            exec_res,
            ExecutionOutcome {
                bundle: Default::default(),
                receipts: vec![vec![receipt.clone()], vec![receipt]],
                requests: vec![Requests::new(vec![request.clone()]), Requests::new(vec![request])],
                first_block: 123,
            }
        );
    }

    #[test]
    fn test_split_at_execution_outcome() {
        // Create a random receipt object
        let receipt = BaseReceipt::Legacy(Receipt::<Log> {
            cumulative_gas_used: 46913,
            logs: vec![],
            status: true.into(),
        });

        // Create a Receipts object with a vector of receipt vectors
        let receipts = vec![vec![receipt.clone()], vec![receipt.clone()], vec![receipt.clone()]];

        // Define the first block number
        let first_block = 123;

        // Create a request.
        let request = bytes!("deadbeef");

        // Create a vector of Requests containing the request.
        let requests = vec![
            Requests::new(vec![request.clone()]),
            Requests::new(vec![request.clone()]),
            Requests::new(vec![request.clone()]),
        ];

        // Create a ExecutionOutcome object with the created bundle, receipts, requests, and
        // first_block
        let exec_res =
            ExecutionOutcome { bundle: Default::default(), receipts, requests, first_block };

        // Split the ExecutionOutcome at block number 124
        let result = exec_res.clone().split_at(124);

        // Define the expected lower ExecutionOutcome after splitting
        let lower_execution_outcome = ExecutionOutcome {
            bundle: Default::default(),
            receipts: vec![vec![receipt.clone()]],
            requests: vec![Requests::new(vec![request.clone()])],
            first_block,
        };

        // Define the expected higher ExecutionOutcome after splitting
        let higher_execution_outcome = ExecutionOutcome {
            bundle: Default::default(),
            receipts: vec![vec![receipt.clone()], vec![receipt]],
            requests: vec![Requests::new(vec![request.clone()]), Requests::new(vec![request])],
            first_block: 124,
        };

        // Assert that the split result matches the expected lower and higher outcomes
        assert_eq!(result.0, Some(lower_execution_outcome));
        assert_eq!(result.1, higher_execution_outcome);

        // Assert that splitting at the first block number returns None for the lower outcome
        assert_eq!(exec_res.clone().split_at(123), (None, exec_res));
    }
}
