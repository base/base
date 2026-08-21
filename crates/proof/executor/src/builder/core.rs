//! Stateless Base L2 block builder implementation.
//!
//! The [`StatelessL2Builder`] provides a complete block building and execution engine
//! for Base L2 chains that operates in a stateless manner, pulling required state
//! data from a [`TrieDB`] during execution rather than maintaining full state.

use alloc::{string::ToString, vec::Vec};
use core::fmt::Debug;

use alloy_consensus::{Header, Sealed, crypto::RecoveryError};
use alloy_evm::{
    EvmFactory, FromRecoveredTx, FromTxWithEncoded,
    block::{BlockExecutionError, BlockExecutionResult, BlockExecutor, BlockExecutorFactory},
};
use base_common_consensus::{BaseReceiptEnvelope, BaseTxEnvelope};
use base_common_evm::{
    AlloyReceiptBuilder, BaseBlockExecutionCtx, BaseBlockExecutorFactory, BaseSpecId, BaseTime,
    BaseTxEnv,
};
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_proof_mpt::TrieHinter;
use base_protocol::BaseTimeUpdateTx;
use revm::{
    context::BlockEnv,
    database::{State, states::bundle_state::BundleRetention},
    database_interface::bal::EvmDatabaseError,
};

use crate::{ExecutorError, ExecutorResult, TrieDB, TrieDBError, TrieDBProvider};

/// Stateless Base L2 block builder that derives state from trie proofs during execution.
///
/// The [`StatelessL2Builder`] is a specialized block execution engine designed for fault proof
/// systems and stateless verification. Instead of maintaining full L2 state, it dynamically
/// retrieves required state data from a [`TrieDB`] backed by Merkle proofs and witnesses.
///
/// # Type Parameters
///
/// * `P` - Trie database provider implementing [`TrieDBProvider`]
/// * `H` - Trie hinter implementing [`TrieHinter`] for state access optimization
/// * `Evm` - EVM factory implementing [`EvmFactory`] for execution environment creation
#[derive(Debug)]
pub struct StatelessL2Builder<'a, P, H, Evm>
where
    P: TrieDBProvider,
    H: TrieHinter,
    Evm: EvmFactory,
{
    /// The rollup configuration containing chain parameters and activation heights.
    pub(crate) config: &'a RollupConfig,
    /// The trie database providing stateless access to L2 state via Merkle proofs.
    pub(crate) trie_db: TrieDB<P, H>,
    /// The block executor factory for creating Base execution environments.
    pub(crate) factory: BaseBlockExecutorFactory<AlloyReceiptBuilder, RollupConfig, Evm>,
}

impl<'a, P, H, Evm> StatelessL2Builder<'a, P, H, Evm>
where
    P: TrieDBProvider + Debug,
    H: TrieHinter + Debug,
    Evm: EvmFactory<Spec = BaseSpecId, BlockEnv = BlockEnv> + 'static,
    <Evm as EvmFactory>::Tx:
        FromTxWithEncoded<BaseTxEnvelope> + FromRecoveredTx<BaseTxEnvelope> + BaseTxEnv,
{
    /// Creates a new stateless L2 block builder instance.
    ///
    /// Initializes the builder with the necessary components for stateless block execution
    /// including the trie database, execution factory, and rollup configuration.
    ///
    /// # Arguments
    /// * `config` - Rollup configuration with chain parameters and activation heights
    /// * `evm_factory` - EVM factory for creating execution environments
    /// * `provider` - Trie database provider for state access
    /// * `hinter` - Trie hinter for optimizing state access patterns
    /// * `parent_header` - Sealed header of the parent block to build upon
    pub fn new(
        config: &'a RollupConfig,
        evm_factory: Evm,
        provider: P,
        hinter: H,
        parent_header: Sealed<Header>,
    ) -> Self {
        let trie_db = TrieDB::new(parent_header, provider, hinter);
        let factory = BaseBlockExecutorFactory::new(
            AlloyReceiptBuilder::default(),
            config.clone(),
            evm_factory,
        );
        Self { config, trie_db, factory }
    }

    /// Returns the stateless trie database.
    pub const fn trie_db(&self) -> &TrieDB<P, H> {
        &self.trie_db
    }

    /// Builds and executes a new L2 block using the provided payload attributes.
    ///
    /// This method performs the complete block building and execution process in a stateless
    /// manner, dynamically retrieving required state data via the trie database and producing
    /// a fully executed block with receipts and state commitments.
    ///
    /// # Arguments
    /// * `attrs` - Payload attributes containing transactions and block metadata
    ///
    /// # Returns
    /// * `Ok(BlockBuildingOutcome)` - Successfully built and executed block with receipts
    /// * `Err(ExecutorError)` - Block building or execution failure
    pub fn build_block(
        &mut self,
        attrs: BasePayloadAttributes,
    ) -> ExecutorResult<BlockBuildingOutcome> {
        // Step 1. Set up the execution environment.
        let (base_fee_params, min_base_fee) = Self::active_base_fee_params(
            self.config,
            self.trie_db.parent_block_header(),
            attrs.payload_attributes.timestamp,
        )?;
        let evm_env = self.evm_env(
            BaseSpecId::from_timestamp(self.config, attrs.payload_attributes.timestamp),
            self.trie_db.parent_block_header(),
            &attrs,
            &base_fee_params,
            min_base_fee,
        )?;
        let block_env = evm_env.block_env().clone();
        let parent_hash = self.trie_db.parent_block_header().seal();
        let parent_timestamp = self.trie_db.parent_block_header().timestamp;
        let block_number = block_env.number.saturating_to::<u64>();
        let block_timestamp = block_env.timestamp.saturating_to::<u64>();
        let (expected_timestamp, expected_timestamp_millis_part) =
            self.config.l2_block_timestamp_parts(block_number);
        let denim_active = self.config.is_denim_active(expected_timestamp);
        let parent_denim_active = self.config.is_denim_active(
            self.config.l2_block_timestamp(self.trie_db.parent_block_header().number),
        );

        let transactions = attrs
            .recovered_transactions_with_encoded()
            .collect::<Result<Vec<_>, RecoveryError>>()
            .map_err(ExecutorError::Recovery)?;
        let base_time = BaseTimeUpdateTx::validate_child_transactions(
            &transactions,
            block_number,
            denim_active,
        )?;
        if let Some(base_time) = base_time {
            if !parent_denim_active {
                base_time.validate_first_denim_anchor()?;
            }
            base_time.validate_scheduled_timestamp(
                block_timestamp,
                expected_timestamp,
                expected_timestamp_millis_part,
            )?;
        }

        // Attempt to send a payload witness hint to the host. This hint instructs the host to
        // populate its preimage store with the preimages required to statelessly execute
        // this payload. This feature is experimental, so if the hint fails, we continue
        // without it and fall back on on-demand preimage fetching for execution.
        self.trie_db
            .hinter
            .hint_execution_witness(parent_hash, &attrs)
            .map_err(|e| TrieDBError::Provider(e.to_string()))?;

        info!(
            target: "block_builder",
            block_number = %block_env.number,
            block_timestamp = %block_env.timestamp,
            block_gas_limit = block_env.gas_limit,
            transactions = attrs.transactions.as_ref().map_or(0, |txs| txs.len()),
            "Beginning block building."
        );

        // Step 2. Create the executor, using the trie database.
        let map_state_error = |error| match error {
            EvmDatabaseError::Database(error) => ExecutorError::TrieDBError(error),
            EvmDatabaseError::Bal(error) => {
                ExecutorError::ExecutionError(BlockExecutionError::other(error))
            }
        };
        let mut state =
            State::builder().with_database(&mut self.trie_db).with_bundle_update().build();
        if let Some(base_time) = base_time
            && parent_denim_active
        {
            let parent_millis =
                BaseTime::fetch_timestamp_millis_part(&mut state).map_err(map_state_error)?;
            base_time.validate_progression(parent_timestamp, parent_millis, block_timestamp)?;
        }
        let evm = self.factory.evm_factory().create_evm(&mut state, evm_env);
        let ctx = BaseBlockExecutionCtx {
            parent_hash,
            parent_beacon_block_root: attrs.payload_attributes.parent_beacon_block_root,
            // This field is unused for individual block building jobs.
            extra_data: Default::default(),
        };
        let executor = self.factory.create_executor(evm, ctx);

        // Step 3. Execute the block containing the transactions within the payload attributes.
        let ex_result = executor.execute_block(transactions.iter())?;

        if let Some(base_time) = base_time {
            BaseTimeUpdateTx::validate_receipts(transactions.len(), &ex_result.receipts)?;
            let child_millis =
                BaseTime::fetch_timestamp_millis_part(&mut state).map_err(map_state_error)?;
            base_time.validate_final_state(child_millis)?;
        }

        info!(
            target: "block_builder",
            gas_used = ex_result.gas_used,
            gas_limit = block_env.gas_limit,
            "Finished block building. Beginning sealing job."
        );

        // Step 4. Merge state transitions and seal the block.
        state.merge_transitions(BundleRetention::Reverts);
        let bundle = state.take_bundle();
        let header = self.seal_block(&attrs, parent_hash, &block_env, &ex_result, bundle)?;

        info!(
            target: "block_builder",
            number = header.number,
            hash = ?header.seal(),
            state_root = ?header.state_root,
            transactions_root = ?header.transactions_root,
            receipts_root = ?header.receipts_root,
            "Sealed new block",
        );

        // Update the parent block hash in the state database, preparing for the next block.
        self.trie_db.set_parent_block_header(header.clone());
        Ok((header, ex_result).into())
    }
}

/// The outcome of a block building operation, returning the sealed block [`Header`] and the
/// [`BlockExecutionResult`].
#[derive(Debug, Clone)]
pub struct BlockBuildingOutcome {
    /// The block header.
    pub header: Sealed<Header>,
    /// The block execution result.
    pub execution_result: BlockExecutionResult<BaseReceiptEnvelope>,
}

impl From<(Sealed<Header>, BlockExecutionResult<BaseReceiptEnvelope>)> for BlockBuildingOutcome {
    fn from(
        (header, execution_result): (Sealed<Header>, BlockExecutionResult<BaseReceiptEnvelope>),
    ) -> Self {
        Self { header, execution_result }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, RwLock},
    };

    use alloy_consensus::{Header, Sealable, SignableTransaction, TxLegacy};
    use alloy_eips::Encodable2718;
    use alloy_primitives::{B256, Bytes, Signature, U256, keccak256};
    use alloy_rlp::{Decodable, Encodable};
    use alloy_trie::{EMPTY_ROOT_HASH, HashBuilder, Nibbles, TrieAccount, proof::ProofRetainer};
    use base_common_consensus::{BaseTxEnvelope, Predeploys, SystemAddresses, TxDeposit};
    use base_common_evm::{BaseEvmFactory, BaseTime};
    use base_common_genesis::{BaseUpgradeConfig, ChainGenesis, RollupConfig, UpgradeConfig};
    use base_common_rpc_types_engine::BasePayloadAttributes;
    use base_proof_mpt::{NoopTrieHinter, TrieNode, TrieProvider};
    use base_protocol::{BaseTimeMetadataError, BaseTimeUpdateTx, BaseTimeValidationError};

    use crate::{ExecutorError, NoopTrieDBProvider, StatelessL2Builder, TrieDBProvider};

    #[derive(Debug, Clone)]
    struct MemoryTrieDBProvider {
        trie_nodes: Arc<RwLock<BTreeMap<B256, Bytes>>>,
        bytecodes: BTreeMap<B256, Bytes>,
    }

    impl MemoryTrieDBProvider {
        fn capture(&self, node: &TrieNode) {
            match node {
                TrieNode::Extension { node, .. } => self.capture(node),
                TrieNode::Branch { stack } => {
                    for node in stack {
                        self.capture(node);
                    }
                }
                TrieNode::Empty | TrieNode::Blinded { .. } | TrieNode::Leaf { .. } => {}
            }
            if !matches!(node, TrieNode::Empty | TrieNode::Blinded { .. }) {
                let encoded = rlp(node);
                self.trie_nodes.write().unwrap().insert(keccak256(&encoded), encoded);
            }
        }
    }

    impl TrieProvider for MemoryTrieDBProvider {
        type Error = String;

        fn trie_node_by_hash(&self, hash: B256) -> Result<TrieNode, Self::Error> {
            let bytes = self
                .trie_nodes
                .read()
                .unwrap()
                .get(&hash)
                .cloned()
                .ok_or_else(|| format!("missing trie node {hash}"))?;
            TrieNode::decode(&mut bytes.as_ref()).map_err(|error| error.to_string())
        }
    }

    impl TrieDBProvider for MemoryTrieDBProvider {
        fn bytecode_by_hash(&self, code_hash: B256) -> Result<Bytes, Self::Error> {
            self.bytecodes
                .get(&code_hash)
                .cloned()
                .ok_or_else(|| format!("missing bytecode {code_hash}"))
        }

        fn header_by_hash(&self, hash: B256) -> Result<Header, Self::Error> {
            Err(format!("missing header {hash}"))
        }
    }

    fn trie(mut leaves: Vec<(Nibbles, Bytes)>) -> (B256, BTreeMap<B256, Bytes>) {
        leaves.sort_by_key(|(path, _)| *path);
        let paths = leaves.iter().map(|(path, _)| *path).collect();
        let mut builder = HashBuilder::default().with_proof_retainer(ProofRetainer::new(paths));
        for (path, value) in leaves {
            builder.add_leaf(path, &value);
        }
        let root = builder.root();
        let nodes = builder
            .take_proof_nodes()
            .into_inner()
            .into_values()
            .map(|value| (keccak256(value.as_ref()), value))
            .collect();
        (root, nodes)
    }

    fn rlp(value: &impl Encodable) -> Bytes {
        let mut encoded = Vec::with_capacity(value.length());
        value.encode(&mut encoded);
        encoded.into()
    }

    fn state(
        timestamp_millis_part: u16,
        implementation: Option<Bytes>,
    ) -> (B256, MemoryTrieDBProvider) {
        let mut storage = vec![(
            Nibbles::unpack(keccak256(BaseTime::ADMIN_SLOT.to_be_bytes::<32>())),
            rlp(&U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice())),
        )];
        if timestamp_millis_part != 0 {
            storage.push((
                Nibbles::unpack(keccak256(
                    BaseTime::TIMESTAMP_MILLIS_PART_SLOT.to_be_bytes::<32>(),
                )),
                rlp(&U256::from(timestamp_millis_part)),
            ));
        }
        if implementation.is_some() {
            storage.push((
                Nibbles::unpack(keccak256(BaseTime::IMPLEMENTATION_SLOT.to_be_bytes::<32>())),
                rlp(&U256::from_be_slice(BaseTime::IMPLEMENTATION_ADDRESS.as_slice())),
            ));
        }
        let (storage_root, mut trie_nodes) = trie(storage);

        let proxy = BaseTime::proxy_bytecode();
        let mut accounts = vec![
            (
                Nibbles::unpack(keccak256(Predeploys::BASE_TIME)),
                rlp(&TrieAccount {
                    nonce: 1,
                    storage_root,
                    code_hash: keccak256(&proxy),
                    ..Default::default()
                }),
            ),
            (
                Nibbles::unpack(keccak256(Predeploys::L2_TO_L1_MESSAGE_PASSER)),
                rlp(&TrieAccount {
                    nonce: 1,
                    storage_root: EMPTY_ROOT_HASH,
                    code_hash: keccak256([]),
                    ..Default::default()
                }),
            ),
        ];
        let mut bytecodes = BTreeMap::from([
            (keccak256(&proxy), proxy),
            (BaseTime::IMPLEMENTATION_CODE_HASH, BaseTime::implementation_bytecode()),
        ]);
        if let Some(implementation) = implementation {
            let code_hash = keccak256(&implementation);
            accounts.push((
                Nibbles::unpack(keccak256(BaseTime::IMPLEMENTATION_ADDRESS)),
                rlp(&TrieAccount {
                    storage_root: EMPTY_ROOT_HASH,
                    code_hash,
                    ..Default::default()
                }),
            ));
            bytecodes.insert(code_hash, implementation);
        }
        let (state_root, state_nodes) = trie(accounts);
        trie_nodes.extend(state_nodes);

        (
            state_root,
            MemoryTrieDBProvider { trie_nodes: Arc::new(RwLock::new(trie_nodes)), bytecodes },
        )
    }

    fn config() -> RollupConfig {
        RollupConfig {
            genesis: ChainGenesis { l2_time: 10, ..Default::default() },
            block_time: 2,
            upgrades: UpgradeConfig {
                base: BaseUpgradeConfig { denim: Some(14), ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn encoded(transaction: BaseTxEnvelope) -> Bytes {
        transaction.encoded_2718().into()
    }

    fn l1_info() -> Bytes {
        encoded(
            TxDeposit {
                from: SystemAddresses::DEPOSITOR_ACCOUNT,
                to: Predeploys::L1_BLOCK_INFO.into(),
                gas_limit: 1_000_000,
                ..Default::default()
            }
            .seal_slow()
            .into(),
        )
    }

    fn base_time(block_number: u64, millis_part: u16) -> Bytes {
        encoded(BaseTimeUpdateTx::new(millis_part).unwrap().into_deposit_tx(block_number).into())
    }

    fn user_transaction() -> Bytes {
        encoded(BaseTxEnvelope::Legacy(
            TxLegacy::default().into_signed(Signature::test_signature()),
        ))
    }

    fn attributes(timestamp: u64, transactions: Vec<Bytes>) -> BasePayloadAttributes {
        let mut attributes = BasePayloadAttributes::default();
        attributes.payload_attributes.timestamp = timestamp;
        attributes.gas_limit = Some(30_000_000);
        attributes.transactions = Some(transactions);
        attributes
    }

    fn builder<'a>(
        config: &'a RollupConfig,
        parent_number: u64,
        parent_timestamp: u64,
        timestamp_millis_part: u16,
        implementation: Option<Bytes>,
    ) -> StatelessL2Builder<'a, MemoryTrieDBProvider, NoopTrieHinter, BaseEvmFactory> {
        let (state_root, provider) = state(timestamp_millis_part, implementation);
        let parent = Header {
            state_root,
            number: parent_number,
            timestamp: parent_timestamp,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000),
            ..Default::default()
        }
        .seal_slow();
        StatelessL2Builder::new(config, BaseEvmFactory::default(), provider, NoopTrieHinter, parent)
    }

    fn assert_rejected(
        parent_number: u64,
        parent_timestamp: u64,
        child_timestamp: u64,
        transactions: Vec<Bytes>,
        expected: BaseTimeValidationError,
    ) {
        let config = config();
        let parent = Header {
            number: parent_number,
            timestamp: parent_timestamp,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000),
            ..Default::default()
        }
        .seal_slow();
        let parent_hash = parent.hash();
        let mut builder = StatelessL2Builder::new(
            &config,
            BaseEvmFactory::default(),
            NoopTrieDBProvider,
            NoopTrieHinter,
            parent,
        );
        let error = builder.build_block(attributes(child_timestamp, transactions)).unwrap_err();

        let ExecutorError::BaseTimeValidation(actual) = error else {
            panic!("expected BaseTime validation error, got {error:?}");
        };
        assert_eq!(actual, expected);
        assert_eq!(builder.trie_db.parent_block_header().hash(), parent_hash);
        assert_eq!(builder.trie_db.parent_block_header().number, parent_number);
    }

    #[test]
    fn rejects_missing_first_denim_metadata_without_advancing_parent() {
        assert_rejected(
            1,
            12,
            14,
            vec![l1_info()],
            BaseTimeValidationError::Metadata(BaseTimeMetadataError::Missing),
        );
    }

    #[test]
    fn rejects_pre_activation_timestamp_at_first_denim_block_without_advancing_parent() {
        assert_rejected(
            1,
            12,
            13,
            vec![l1_info()],
            BaseTimeValidationError::Metadata(BaseTimeMetadataError::Missing),
        );
        assert_rejected(
            1,
            12,
            13,
            vec![l1_info(), base_time(2, 0)],
            BaseTimeValidationError::ScheduledTimestampMismatch {
                expected_timestamp_ms: 14_000,
                actual_timestamp_ms: 13_000,
            },
        );
    }

    #[test]
    fn rejects_nonzero_first_denim_anchor_without_advancing_parent() {
        assert_rejected(
            1,
            12,
            14,
            vec![l1_info(), base_time(2, 200)],
            BaseTimeValidationError::InvalidFirstDenimAnchor { timestamp_millis_part: 200 },
        );
    }

    #[test]
    fn rejects_wrong_denim_schedule_without_advancing_parent() {
        for (child_timestamp, millis_part, actual_timestamp_ms) in
            [(14, 400, 14_400), (15, 200, 15_200)]
        {
            assert_rejected(
                2,
                14,
                child_timestamp,
                vec![l1_info(), base_time(3, millis_part)],
                BaseTimeValidationError::ScheduledTimestampMismatch {
                    expected_timestamp_ms: 14_200,
                    actual_timestamp_ms,
                },
            );
        }
    }

    #[test]
    fn rejects_protocol_metadata_before_denim_without_advancing_parent() {
        assert_rejected(
            0,
            10,
            12,
            vec![l1_info(), base_time(1, 0)],
            BaseTimeValidationError::ProtocolSetterBeforeDenim { index: 1 },
        );
    }

    #[test]
    fn rejects_additional_protocol_setter_without_advancing_parent() {
        for duplicate_millis_part in [200, 400] {
            assert_rejected(
                2,
                14,
                14,
                vec![l1_info(), base_time(3, 200), base_time(3, duplicate_millis_part)],
                BaseTimeValidationError::AdditionalProtocolSetter { index: 2 },
            );
        }
    }

    #[test]
    fn rejects_reordered_metadata_without_advancing_parent() {
        assert_rejected(
            1,
            12,
            14,
            vec![l1_info(), user_transaction(), base_time(2, 0)],
            BaseTimeValidationError::Metadata(BaseTimeMetadataError::NotDeposit),
        );
    }

    #[test]
    fn rejection_is_atomic_and_valid_retry_succeeds() {
        let config = config();
        let mut builder = builder(&config, 1, 12, 0, None);
        let parent_hash = builder.trie_db.parent_block_header().hash();
        let state_root = builder.trie_db.root().blind();

        let error = builder.build_block(attributes(14, vec![l1_info()])).unwrap_err();
        assert!(matches!(
            error,
            ExecutorError::BaseTimeValidation(BaseTimeValidationError::Metadata(
                BaseTimeMetadataError::Missing
            ))
        ));
        assert_eq!(builder.trie_db.parent_block_header().hash(), parent_hash);
        assert_eq!(builder.trie_db.root().blind(), state_root);

        let outcome = builder
            .build_block(attributes(14, vec![l1_info(), base_time(2, 0)]))
            .expect("valid retry from the original parent should succeed");
        assert_eq!(outcome.header.number, 2);
        assert_ne!(outcome.header.hash(), parent_hash);
    }

    #[test]
    fn rejects_invalid_parent_progression_without_advancing_parent() {
        let config = config();
        let mut builder = builder(&config, 2, 14, 800, None);
        let parent_hash = builder.trie_db.parent_block_header().hash();

        let error =
            builder.build_block(attributes(14, vec![l1_info(), base_time(3, 200)])).unwrap_err();

        assert!(matches!(
            error,
            ExecutorError::BaseTimeValidation(BaseTimeValidationError::ProgressionMismatch {
                parent_timestamp_ms: 14_800,
                child_timestamp_ms: 14_200,
            })
        ));
        assert_eq!(builder.trie_db.parent_block_header().hash(), parent_hash);
    }

    #[test]
    fn rejects_failed_metadata_execution_without_advancing_parent() {
        let config = config();
        let mut builder =
            builder(&config, 1, 12, 0, Some(Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xfd])));
        let parent_hash = builder.trie_db.parent_block_header().hash();

        let error =
            builder.build_block(attributes(14, vec![l1_info(), base_time(2, 0)])).unwrap_err();

        assert!(matches!(
            error,
            ExecutorError::BaseTimeValidation(BaseTimeValidationError::MetadataExecutionFailed)
        ));
        assert_eq!(builder.trie_db.parent_block_header().hash(), parent_hash);
    }

    #[test]
    fn rejects_stale_final_state_without_advancing_parent() {
        let config = config();
        let mut builder = builder(&config, 2, 14, 0, Some(Bytes::from_static(&[0x00])));
        let parent_hash = builder.trie_db.parent_block_header().hash();

        let error =
            builder.build_block(attributes(14, vec![l1_info(), base_time(3, 200)])).unwrap_err();

        assert!(matches!(
            error,
            ExecutorError::BaseTimeValidation(BaseTimeValidationError::FinalStateMismatch {
                expected_timestamp_millis_part: 200,
                actual_timestamp_millis_part: 0,
            })
        ));
        assert_eq!(builder.trie_db.parent_block_header().hash(), parent_hash);
    }

    #[test]
    fn executes_six_canonical_base_time_slots() {
        let config = config();
        let mut builder = builder(&config, 1, 12, 0, None);
        let mut parent_hash = builder.trie_db.parent_block_header().hash();

        for (block_number, timestamp, millis_part) in
            [(2, 14, 0), (3, 14, 200), (4, 14, 400), (5, 14, 600), (6, 14, 800), (7, 15, 0)]
        {
            let metadata = base_time(block_number, millis_part);
            let outcome = builder
                .build_block(attributes(timestamp, vec![l1_info(), metadata.clone()]))
                .expect("canonical BaseTime block should execute");

            assert_eq!(outcome.header.number, block_number);
            assert_eq!(outcome.header.timestamp, timestamp);
            assert_eq!(outcome.header.parent_hash, parent_hash);
            assert!(outcome.execution_result.receipts[1].status());
            assert_eq!(
                BaseTime::fetch_timestamp_millis_part(&mut builder.trie_db).unwrap(),
                millis_part
            );
            assert_eq!(
                attributes(timestamp, vec![l1_info(), metadata]).transactions.unwrap()[1],
                base_time(block_number, millis_part)
            );
            assert_eq!(outcome.header.state_root, builder.trie_db.root().blind());
            assert_ne!(builder.compute_output_root().unwrap(), B256::ZERO);

            builder.trie_db.fetcher.capture(builder.trie_db.root());
            for storage_root in builder.trie_db.storage_roots().values() {
                builder.trie_db.fetcher.capture(storage_root);
            }
            parent_hash = outcome.header.hash();
        }
    }
}
