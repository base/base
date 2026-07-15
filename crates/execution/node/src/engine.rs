use std::{marker::PhantomData, sync::Arc};

use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_engine::{ExecutionPayloadEnvelopeV2, ExecutionPayloadV1};
use base_common_chains::Upgrades;
use base_common_consensus::{BaseBlock, Predeploys};
use base_common_evm::BaseTime;
use base_common_rpc_types_engine::{
    BaseExecutionPayloadEnvelopeV3, BaseExecutionPayloadEnvelopeV4, BaseExecutionPayloadEnvelopeV5,
    ExecutionData,
};
use base_execution_consensus::{BaseConsensusError, isthmus};
use base_execution_payload_builder::{
    Attributes, BaseExecutionPayloadValidator, BasePayloadBuilderAttributes, BasePayloadTypes,
};
use base_protocol::BaseTimeUpdateTx;
use reth_consensus::ConsensusError;
use reth_node_api::{
    BuiltPayload, EngineApiValidator, EngineTypes, NodePrimitives, PayloadValidator,
    payload::{
        EngineApiMessageVersion, EngineObjectValidationError, MessageValidationKind,
        NewPayloadError, PayloadOrAttributes, PayloadTypes, VersionSpecificValidationError,
        validate_parent_beacon_block_root_presence,
    },
    validate_version_specific_fields,
};
use reth_payload_primitives::{InvalidPayloadAttributesError, PayloadAttributes};
use reth_primitives_traits::{Block, RecoveredBlock, SealedBlock, SignedTransaction};
use reth_provider::{StateProvider, StateProviderFactory};
use reth_trie_common::{HashedPostState, KeyHasher};

/// The types used in the Base beacon consensus engine.
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub struct BaseEngineTypes<T: PayloadTypes = BasePayloadTypes> {
    _marker: PhantomData<T>,
}

impl<T: PayloadTypes<ExecutionData = ExecutionData>> PayloadTypes for BaseEngineTypes<T>
where
    ExecutionData: From<T::BuiltPayload>,
{
    type ExecutionData = T::ExecutionData;
    type BuiltPayload = T::BuiltPayload;
    type PayloadAttributes = T::PayloadAttributes;

    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
        bal: Option<Bytes>,
    ) -> <T as PayloadTypes>::ExecutionData {
        ExecutionData::from_block_unchecked_with_extras(
            block.hash(),
            &block.into_block().into_ethereum_block(),
            bal,
        )
    }
}

impl<T: PayloadTypes<ExecutionData = ExecutionData>> EngineTypes for BaseEngineTypes<T>
where
    ExecutionData: From<T::BuiltPayload>,
    T::BuiltPayload: BuiltPayload<Primitives: NodePrimitives<Block = BaseBlock>>
        + TryInto<ExecutionPayloadV1>
        + TryInto<ExecutionPayloadEnvelopeV2>
        + TryInto<BaseExecutionPayloadEnvelopeV3>
        + TryInto<BaseExecutionPayloadEnvelopeV4>
        + TryInto<BaseExecutionPayloadEnvelopeV5>,
{
    type ExecutionPayloadEnvelopeV1 = ExecutionPayloadV1;
    type ExecutionPayloadEnvelopeV2 = ExecutionPayloadEnvelopeV2;
    type ExecutionPayloadEnvelopeV3 = BaseExecutionPayloadEnvelopeV3;
    type ExecutionPayloadEnvelopeV4 = BaseExecutionPayloadEnvelopeV4;
    type ExecutionPayloadEnvelopeV5 = BaseExecutionPayloadEnvelopeV5;
    type ExecutionPayloadEnvelopeV6 = BaseExecutionPayloadEnvelopeV5;
}

/// Validator for Base engine API.
#[derive(Debug)]
pub struct BaseEngineValidator<P, Tx, ChainSpec> {
    inner: BaseExecutionPayloadValidator<ChainSpec>,
    provider: P,
    hashed_addr_l2tol1_msg_passer: B256,
    hashed_addr_base_time: B256,
    hashed_slot_base_time_millis: B256,
    phantom: PhantomData<Tx>,
}

impl<P, Tx, ChainSpec> BaseEngineValidator<P, Tx, ChainSpec> {
    /// Instantiates a new validator.
    pub fn new<KH: KeyHasher>(chain_spec: Arc<ChainSpec>, provider: P) -> Self {
        let hashed_addr_l2tol1_msg_passer = KH::hash_key(Predeploys::L2_TO_L1_MESSAGE_PASSER);
        let hashed_addr_base_time = KH::hash_key(Predeploys::BASE_TIME);
        let hashed_slot_base_time_millis =
            KH::hash_key(B256::from(BaseTime::TIMESTAMP_MILLIS_PART_SLOT));
        Self {
            inner: BaseExecutionPayloadValidator::new(chain_spec),
            provider,
            hashed_addr_l2tol1_msg_passer,
            hashed_addr_base_time,
            hashed_slot_base_time_millis,
            phantom: PhantomData,
        }
    }
}

impl<P, Tx, ChainSpec> Clone for BaseEngineValidator<P, Tx, ChainSpec>
where
    P: Clone,
    ChainSpec: Upgrades,
{
    fn clone(&self) -> Self {
        Self {
            inner: BaseExecutionPayloadValidator::new(self.inner.clone()),
            provider: self.provider.clone(),
            hashed_addr_l2tol1_msg_passer: self.hashed_addr_l2tol1_msg_passer,
            hashed_addr_base_time: self.hashed_addr_base_time,
            hashed_slot_base_time_millis: self.hashed_slot_base_time_millis,
            phantom: Default::default(),
        }
    }
}

impl<P, Tx, ChainSpec> BaseEngineValidator<P, Tx, ChainSpec>
where
    ChainSpec: Upgrades,
{
    /// Returns the chain spec used by the validator.
    #[inline]
    pub fn chain_spec(&self) -> &ChainSpec {
        self.inner.chain_spec()
    }

    /// Verifies upgrade-gated post-execution rules against the supplied parent state.
    ///
    /// Authoritative implementation of all Base-specific post-execution checks that require
    /// access to parent state. Callers supply `parent_state` explicitly so engine pipelines can
    /// pass in-memory-aware overlay providers when the parent block isn't canonical yet.
    ///
    /// To add a check for a future upgrade, extend the body with another
    /// `if chain_spec.is_<X>_active_at_timestamp(...)` arm.
    pub fn validate_block_post_execution_with_state<DB>(
        &self,
        state_updates: &HashedPostState,
        parent_state: DB,
        parent_timestamp: u64,
        block: &RecoveredBlock<BaseBlock>,
    ) -> Result<(), ConsensusError>
    where
        DB: StateProvider,
    {
        let timestamp = block.timestamp();

        if self.chain_spec().is_isthmus_active_at_timestamp(timestamp) {
            self.validate_isthmus_post_execution(state_updates, &parent_state, block.header())?;
        }

        if self.chain_spec().is_zombie_active_at_timestamp(timestamp) {
            self.validate_base_time_post_execution(
                state_updates,
                &parent_state,
                parent_timestamp,
                block,
            )?;
        }

        Ok(())
    }

    /// Verifies the Isthmus L2-to-L1 message-passer storage root after block execution.
    pub fn validate_isthmus_post_execution<DB, H>(
        &self,
        state_updates: &HashedPostState,
        parent_state: &DB,
        header: H,
    ) -> Result<(), ConsensusError>
    where
        DB: StateProvider + ?Sized,
        H: BlockHeader,
    {
        let predeploy_storage_updates = state_updates
            .storages
            .get(&self.hashed_addr_l2tol1_msg_passer)
            .cloned()
            .unwrap_or_default();
        isthmus::verify_withdrawals_root_prehashed(predeploy_storage_updates, parent_state, header)
            .map_err(ConsensusError::other)
    }

    /// Verifies the `BaseTime` claim, committed state, and block-to-block progression after block
    /// execution.
    pub fn validate_base_time_post_execution<DB>(
        &self,
        state_updates: &HashedPostState,
        parent_state: &DB,
        parent_timestamp: u64,
        block: &RecoveredBlock<BaseBlock>,
    ) -> Result<(), ConsensusError>
    where
        DB: StateProvider + ?Sized,
    {
        let claimed_base_time =
            BaseTimeUpdateTx::extract_from_transactions(&block.body().transactions, block.number())
                .map_err(ConsensusError::other)?;
        let claimed_millis_part = claimed_base_time.timestamp_millis_part();

        let read_parent_millis_part = || {
            parent_state
                .storage(Predeploys::BASE_TIME, BaseTime::TIMESTAMP_MILLIS_PART_SLOT.into())
                .map(|word| BaseTime::decode_timestamp_millis_part(word.unwrap_or_default()))
                .map_err(ConsensusError::other)
        };

        let storage_updates = state_updates.storages.get(&self.hashed_addr_base_time);
        let updated_word = storage_updates
            .and_then(|storage| storage.storage.get(&self.hashed_slot_base_time_millis));
        let committed_millis_part = match updated_word {
            Some(word) => BaseTime::decode_timestamp_millis_part(*word),
            None if storage_updates.is_some_and(|storage| storage.wiped) => 0,
            None => read_parent_millis_part()?,
        };

        if claimed_millis_part != committed_millis_part {
            return Err(ConsensusError::other(
                BaseConsensusError::BaseTimeClaimCommittedMismatch {
                    claim: claimed_millis_part,
                    committed: committed_millis_part,
                },
            ));
        }

        // A parent at the activation second is itself Zombie-active; its committed millis part
        // distinguishes same-second slots. Skip progression only when the parent predates Zombie.
        if self.chain_spec().is_zombie_active_at_timestamp(parent_timestamp) {
            let parent_millis_part = read_parent_millis_part()?;
            let parent_base_time = BaseTimeUpdateTx::new(parent_millis_part).map_err(|_| {
                ConsensusError::other(BaseConsensusError::InvalidParentBaseTimeMillis(
                    parent_millis_part,
                ))
            })?;
            BaseTimeUpdateTx::validate_progression(
                parent_timestamp,
                &parent_base_time,
                block.timestamp(),
                &claimed_base_time,
            )
            .map_err(ConsensusError::other)?;
        }

        Ok(())
    }
}

/// Extension trait that exposes [`BaseEngineValidator::validate_block_post_execution_with_state`]
/// through generic engine pipelines.
///
/// The inherent method on [`BaseEngineValidator`] is the source of truth; this trait is just a
/// dispatch surface so callers that don't know the concrete validator type can still invoke it.
pub trait BasePostExecutionValidator<Types: PayloadTypes>:
    PayloadValidator<Types, Block = BaseBlock>
{
    /// See [`BaseEngineValidator::validate_block_post_execution_with_state`].
    fn validate_block_post_execution_with_parent_state<DB: StateProvider>(
        &self,
        state_updates: &HashedPostState,
        parent_state: DB,
        parent_timestamp: u64,
        block: &RecoveredBlock<BaseBlock>,
    ) -> Result<(), ConsensusError>;
}

impl<Types, P, Tx, ChainSpec> BasePostExecutionValidator<Types>
    for BaseEngineValidator<P, Tx, ChainSpec>
where
    Types: PayloadTypes,
    Self: PayloadValidator<Types, Block = BaseBlock>,
    ChainSpec: Upgrades,
{
    fn validate_block_post_execution_with_parent_state<DB: StateProvider>(
        &self,
        state_updates: &HashedPostState,
        parent_state: DB,
        parent_timestamp: u64,
        block: &RecoveredBlock<BaseBlock>,
    ) -> Result<(), ConsensusError> {
        Self::validate_block_post_execution_with_state(
            self,
            state_updates,
            parent_state,
            parent_timestamp,
            block,
        )
    }
}

impl<P, Tx, ChainSpec, Types> PayloadValidator<Types> for BaseEngineValidator<P, Tx, ChainSpec>
where
    P: StateProviderFactory + Unpin + 'static,
    Tx: SignedTransaction + Unpin + 'static,
    ChainSpec: Upgrades + Send + Sync + 'static,
    Types: PayloadTypes<ExecutionData = ExecutionData>,
    Types::PayloadAttributes: Attributes<Transaction = Tx>,
{
    type Block = alloy_consensus::Block<Tx>;

    fn validate_block_post_execution_with_hashed_state(
        &self,
        state_updates: &HashedPostState,
        block: &RecoveredBlock<Self::Block>,
    ) -> Result<(), ConsensusError> {
        let timestamp = block.timestamp();
        if self.chain_spec().is_isthmus_active_at_timestamp(timestamp) {
            let parent_state =
                self.provider.state_by_block_hash(block.parent_hash()).map_err(|err| {
                    ConsensusError::Other(Arc::from(Box::<dyn core::error::Error + Send + Sync>::from(
                        format!(
                            "failed to load parent state for Isthmus withdrawals root validation: {err}"
                        ),
                    )))
                })?;

            self.validate_isthmus_post_execution(state_updates, &parent_state, block.header())?;
        }

        if self.chain_spec().is_zombie_active_at_timestamp(timestamp) {
            return Err(ConsensusError::other(
                BaseConsensusError::BaseTimeValidationContextRequired,
            ));
        }

        Ok(())
    }

    fn convert_payload_to_block(
        &self,
        payload: ExecutionData,
    ) -> Result<SealedBlock<Self::Block>, NewPayloadError> {
        self.inner.ensure_well_formed_payload(payload).map_err(NewPayloadError::other)
    }

    fn validate_payload_attributes_against_header(
        &self,
        attributes: &Types::PayloadAttributes,
        header: &<Self::Block as Block>::Header,
    ) -> Result<(), InvalidPayloadAttributesError> {
        let timestamp = attributes.timestamp();
        if !self.chain_spec().is_zombie_active_at_timestamp(timestamp) {
            return (timestamp > header.timestamp())
                .then_some(())
                .ok_or(InvalidPayloadAttributesError::InvalidTimestamp);
        }

        let timestamp_millis_part = attributes
            .timestamp_millis_part()
            .ok_or(InvalidPayloadAttributesError::InvalidTimestamp)?;
        if !BaseTimeUpdateTx::is_valid_timestamp_millis_part(timestamp_millis_part) {
            return Err(InvalidPayloadAttributesError::InvalidTimestamp);
        }

        // The parent header does not contain its millisecond component. Exact 200ms progression is
        // enforced against committed BaseTime state during post-execution validation.
        (timestamp >= header.timestamp())
            .then_some(())
            .ok_or(InvalidPayloadAttributesError::InvalidTimestamp)
    }
}

impl<Types, P, Tx, ChainSpec> EngineApiValidator<Types> for BaseEngineValidator<P, Tx, ChainSpec>
where
    Types: PayloadTypes<
            PayloadAttributes = BasePayloadBuilderAttributes<Tx>,
            ExecutionData = ExecutionData,
            BuiltPayload: BuiltPayload<Primitives: NodePrimitives<SignedTx = Tx>>,
        >,
    P: StateProviderFactory + Unpin + 'static,
    Tx: SignedTransaction + Unpin + 'static,
    ChainSpec: Upgrades + Send + Sync + 'static,
{
    fn validate_version_specific_fields(
        &self,
        version: EngineApiMessageVersion,
        payload_or_attrs: PayloadOrAttributes<
            '_,
            Types::ExecutionData,
            <Types as PayloadTypes>::PayloadAttributes,
        >,
    ) -> Result<(), EngineObjectValidationError> {
        validate_withdrawals_presence(
            self.chain_spec(),
            version,
            payload_or_attrs.message_validation_kind(),
            payload_or_attrs.timestamp(),
            payload_or_attrs.withdrawals().is_some(),
        )?;
        validate_parent_beacon_block_root_presence(
            self.chain_spec(),
            version,
            payload_or_attrs.message_validation_kind(),
            payload_or_attrs.timestamp(),
            payload_or_attrs.parent_beacon_block_root().is_some(),
        )
    }

    fn ensure_well_formed_attributes(
        &self,
        version: EngineApiMessageVersion,
        attributes: &<Types as PayloadTypes>::PayloadAttributes,
    ) -> Result<(), EngineObjectValidationError> {
        validate_version_specific_fields(
            self.chain_spec(),
            version,
            PayloadOrAttributes::<ExecutionData, Types::PayloadAttributes>::PayloadAttributes(
                attributes,
            ),
        )?;

        if attributes.gas_limit.is_none() {
            return Err(EngineObjectValidationError::InvalidParams(
                "MissingGasLimitInPayloadAttributes".to_string().into(),
            ));
        }

        if self
            .chain_spec()
            .is_holocene_active_at_timestamp(attributes.payload_attributes.timestamp)
        {
            let (elasticity, denominator) =
                attributes.decode_eip_1559_params().ok_or_else(|| {
                    EngineObjectValidationError::InvalidParams(
                        "MissingEip1559ParamsInPayloadAttributes".to_string().into(),
                    )
                })?;

            if elasticity != 0 && denominator == 0 {
                return Err(EngineObjectValidationError::InvalidParams(
                    "Eip1559ParamsDenominatorZero".to_string().into(),
                ));
            } else if denominator != 0 && elasticity == 0 {
                return Err(EngineObjectValidationError::InvalidParams(
                    "Eip1559ParamsElasticityZero".to_string().into(),
                ));
            }
        }

        if self.chain_spec().is_jovian_active_at_timestamp(attributes.payload_attributes.timestamp)
        {
            if attributes.min_base_fee.is_none() {
                return Err(EngineObjectValidationError::InvalidParams(
                    "MissingMinBaseFeeInPayloadAttributes".to_string().into(),
                ));
            }
        } else if attributes.min_base_fee.is_some() {
            return Err(EngineObjectValidationError::InvalidParams(
                "MinBaseFeeNotAllowedBeforeJovian".to_string().into(),
            ));
        }

        let zombie_active = self
            .chain_spec()
            .is_zombie_active_at_timestamp(attributes.payload_attributes.timestamp);
        match attributes.timestamp_millis_part {
            Some(_) if !zombie_active => {
                return Err(EngineObjectValidationError::InvalidParams(
                    "TimestampMillisPartNotAllowed".to_string().into(),
                ));
            }
            None if zombie_active => {
                return Err(EngineObjectValidationError::InvalidParams(
                    "MissingTimestampMillisPartInPayloadAttributes".to_string().into(),
                ));
            }
            Some(value) if !BaseTimeUpdateTx::is_valid_timestamp_millis_part(value) => {
                return Err(EngineObjectValidationError::InvalidParams(
                    "InvalidTimestampMillisPartInPayloadAttributes".to_string().into(),
                ));
            }
            _ => {}
        }

        Ok(())
    }
}

/// Validates the presence of the `withdrawals` field according to the payload timestamp.
///
/// After Canyon, withdrawals field must be [Some].
/// Before Canyon, withdrawals field must be [None];
///
/// Canyon activates the Shanghai EIPs, see the Canyon specs for more details:
/// <https://github.com/ethereum-optimism/optimism/blob/ab926c5fd1e55b5c864341c44842d6d1ca679d99/specs/superchain-upgrades.md#canyon>
pub fn validate_withdrawals_presence(
    chain_spec: impl Upgrades,
    version: EngineApiMessageVersion,
    message_validation_kind: MessageValidationKind,
    timestamp: u64,
    has_withdrawals: bool,
) -> Result<(), EngineObjectValidationError> {
    let is_shanghai = chain_spec.is_canyon_active_at_timestamp(timestamp);

    match version {
        EngineApiMessageVersion::V1 => {
            if has_withdrawals {
                return Err(message_validation_kind
                    .to_error(VersionSpecificValidationError::WithdrawalsNotSupportedInV1));
            }
            if is_shanghai {
                return Err(message_validation_kind
                    .to_error(VersionSpecificValidationError::NoWithdrawalsPostShanghai));
            }
        }
        EngineApiMessageVersion::V2
        | EngineApiMessageVersion::V3
        | EngineApiMessageVersion::V4
        | EngineApiMessageVersion::V5
        | EngineApiMessageVersion::V6 => {
            if is_shanghai && !has_withdrawals {
                return Err(message_validation_kind
                    .to_error(VersionSpecificValidationError::NoWithdrawalsPostShanghai));
            }
            if !is_shanghai && has_withdrawals {
                return Err(message_validation_kind
                    .to_error(VersionSpecificValidationError::HasWithdrawalsPreShanghai));
            }
        }
    };

    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{BlockBody, EMPTY_ROOT_HASH, Header, Sealable};
    use alloy_primitives::{Address, B64, B256, U256, b64, keccak256};
    use alloy_rpc_types_engine::PayloadAttributes;
    use base_common_chains::{BaseUpgrade, ChainConfig};
    use base_common_consensus::{BasePrimitives, BaseTxEnvelope, TxDeposit};
    use base_common_rpc_types_engine::BasePayloadAttributes;
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use reth_ethereum_forks::ForkCondition;
    use reth_provider::{
        noop::NoopProvider,
        test_utils::{ExtendedAccount, MockEthProvider},
    };
    use reth_trie_common::{HashedStorage, KeccakKeyHasher};

    use super::*;
    use crate::engine;

    fn validator_with_chain_spec(
        chain_spec: BaseChainSpec,
    ) -> BaseEngineValidator<NoopProvider, BaseTxEnvelope, BaseChainSpec> {
        BaseEngineValidator::<NoopProvider, BaseTxEnvelope, BaseChainSpec>::new::<KeccakKeyHasher>(
            Arc::new(chain_spec),
            NoopProvider::default(),
        )
    }

    fn validator() -> BaseEngineValidator<NoopProvider, BaseTxEnvelope, BaseChainSpec> {
        validator_with_chain_spec(BaseChainSpec::sepolia())
    }

    fn zombie_validator() -> BaseEngineValidator<NoopProvider, BaseTxEnvelope, BaseChainSpec> {
        validator_with_chain_spec(
            BaseChainSpecBuilder::base_mainnet()
                .cobalt_activated()
                .with_fork(BaseUpgrade::Zombie, ForkCondition::Timestamp(42))
                .build(),
        )
    }

    macro_rules! assert_invalid_params_error {
        ($result:expr, $msg:expr) => {{
            let err = $result.expect_err("expected InvalidParams error");
            match err {
                EngineObjectValidationError::InvalidParams(inner) => {
                    assert_eq!(inner.to_string(), $msg);
                }
                other => panic!("expected InvalidParams, got {other:?}"),
            }
        }};
    }

    fn get_attributes(
        eip_1559_params: Option<B64>,
        min_base_fee: Option<u64>,
        timestamp: u64,
    ) -> BasePayloadBuilderAttributes<BaseTxEnvelope> {
        BasePayloadBuilderAttributes::try_new(
            B256::ZERO,
            BasePayloadAttributes {
                gas_limit: Some(1000),
                eip_1559_params,
                min_base_fee,
                transactions: None,
                no_tx_pool: None,
                timestamp_millis_part: None,
                payload_attributes: PayloadAttributes {
                    timestamp,
                    prev_randao: B256::ZERO,
                    suggested_fee_recipient: Address::ZERO,
                    withdrawals: Some(vec![]),
                    parent_beacon_block_root: Some(B256::ZERO),
                    slot_number: None,
                },
            },
            3,
        )
        .expect("valid test payload attributes")
    }

    fn zombie_attributes(timestamp: u64) -> BasePayloadBuilderAttributes<BaseTxEnvelope> {
        get_attributes(Some(b64!("0000000000000000")), Some(1), timestamp)
    }

    #[test]
    fn test_well_formed_attributes_pre_holocene() {
        let validator = validator();
        let attributes = get_attributes(None, None, 1732633199);

        let result = <engine::BaseEngineValidator<_, _, _> as EngineApiValidator<
            BaseEngineTypes,
        >>::ensure_well_formed_attributes(
            &validator, EngineApiMessageVersion::V3, &attributes
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_well_formed_attributes_holocene_no_eip1559_params() {
        let validator = validator();
        let attributes = get_attributes(None, None, 1732633200);

        let result = <engine::BaseEngineValidator<_, _, _> as EngineApiValidator<
            BaseEngineTypes,
        >>::ensure_well_formed_attributes(
            &validator, EngineApiMessageVersion::V3, &attributes
        );
        assert_invalid_params_error!(result, "MissingEip1559ParamsInPayloadAttributes");
    }

    #[test]
    fn test_well_formed_attributes_holocene_eip1559_params_zero_denominator() {
        let validator = validator();
        let attributes = get_attributes(Some(b64!("0000000000000008")), None, 1732633200);

        let result = <engine::BaseEngineValidator<_, _, _> as EngineApiValidator<
            BaseEngineTypes,
        >>::ensure_well_formed_attributes(
            &validator, EngineApiMessageVersion::V3, &attributes
        );
        assert_invalid_params_error!(result, "Eip1559ParamsDenominatorZero");
    }

    #[test]
    fn test_well_formed_attributes_holocene_eip1559_params_zero_elasticity() {
        let validator = validator();
        let attributes = get_attributes(Some(b64!("0000000800000000")), None, 1732633200);

        let result = <engine::BaseEngineValidator<_, _, _> as EngineApiValidator<
            BaseEngineTypes,
        >>::ensure_well_formed_attributes(
            &validator, EngineApiMessageVersion::V3, &attributes
        );
        assert_invalid_params_error!(result, "Eip1559ParamsElasticityZero");
    }

    #[test]
    fn test_well_formed_attributes_holocene_valid() {
        let validator = validator();
        let attributes = get_attributes(Some(b64!("0000000800000008")), None, 1732633200);

        let result = <engine::BaseEngineValidator<_, _, _> as EngineApiValidator<
            BaseEngineTypes,
        >>::ensure_well_formed_attributes(
            &validator, EngineApiMessageVersion::V3, &attributes
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_well_formed_attributes_holocene_valid_all_zero() {
        let validator = validator();
        let attributes = get_attributes(Some(b64!("0000000000000000")), None, 1732633200);

        let result = <engine::BaseEngineValidator<_, _, _> as EngineApiValidator<
            BaseEngineTypes,
        >>::ensure_well_formed_attributes(
            &validator, EngineApiMessageVersion::V3, &attributes
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_well_formed_attributes_jovian_valid() {
        let validator = validator();
        let attributes = get_attributes(
            Some(b64!("0000000000000000")),
            Some(1),
            ChainConfig::sepolia().jovian_timestamp,
        );

        let result = <engine::BaseEngineValidator<_, _, _> as EngineApiValidator<
            BaseEngineTypes,
        >>::ensure_well_formed_attributes(
            &validator, EngineApiMessageVersion::V3, &attributes
        );
        assert!(result.is_ok());
    }

    /// After Jovian (and holocene), eip1559 params must be Some
    #[test]
    fn test_malformed_attributes_jovian_with_eip_1559_params_none() {
        let validator = validator();
        let attributes = get_attributes(None, Some(1), ChainConfig::sepolia().jovian_timestamp);

        let result = <engine::BaseEngineValidator<_, _, _> as EngineApiValidator<
            BaseEngineTypes,
        >>::ensure_well_formed_attributes(
            &validator, EngineApiMessageVersion::V3, &attributes
        );
        assert_invalid_params_error!(result, "MissingEip1559ParamsInPayloadAttributes");
    }

    /// Before Jovian, min base fee must be None
    #[test]
    fn test_malformed_attributes_pre_jovian_with_min_base_fee() {
        let validator = validator();
        let attributes = get_attributes(Some(b64!("0000000000000000")), Some(1), 1732633200);

        let result = <engine::BaseEngineValidator<_, _, _> as EngineApiValidator<
            BaseEngineTypes,
        >>::ensure_well_formed_attributes(
            &validator, EngineApiMessageVersion::V3, &attributes
        );
        assert_invalid_params_error!(result, "MinBaseFeeNotAllowedBeforeJovian");
    }

    /// After Jovian, min base fee must be Some
    #[test]
    fn test_malformed_attributes_post_jovian_with_min_base_fee_none() {
        let validator = validator();
        let attributes = get_attributes(
            Some(b64!("0000000000000000")),
            None,
            ChainConfig::sepolia().jovian_timestamp,
        );

        let result = <engine::BaseEngineValidator<_, _, _> as EngineApiValidator<
            BaseEngineTypes,
        >>::ensure_well_formed_attributes(
            &validator, EngineApiMessageVersion::V3, &attributes
        );
        assert_invalid_params_error!(result, "MissingMinBaseFeeInPayloadAttributes");
    }

    #[test]
    fn test_malformed_attributes_with_timestamp_millis_part() {
        let validator = validator();
        let mut attributes = get_attributes(None, None, 1732633199);
        attributes.timestamp_millis_part = Some(200);

        let result = <engine::BaseEngineValidator<_, _, _> as EngineApiValidator<
            BaseEngineTypes,
        >>::ensure_well_formed_attributes(
            &validator, EngineApiMessageVersion::V3, &attributes
        );
        assert_invalid_params_error!(result, "TimestampMillisPartNotAllowed");
    }

    #[test]
    fn test_malformed_attributes_post_zombie_without_timestamp_millis_part() {
        let validator = zombie_validator();
        let attributes = zombie_attributes(42);

        let result = <engine::BaseEngineValidator<_, _, _> as EngineApiValidator<
            BaseEngineTypes,
        >>::ensure_well_formed_attributes(
            &validator, EngineApiMessageVersion::V3, &attributes
        );
        assert_invalid_params_error!(result, "MissingTimestampMillisPartInPayloadAttributes");
    }

    #[test]
    fn test_malformed_attributes_post_zombie_with_invalid_timestamp_millis_part() {
        let validator = zombie_validator();
        let mut attributes = zombie_attributes(42);
        attributes.timestamp_millis_part = Some(100);

        let result = <engine::BaseEngineValidator<_, _, _> as EngineApiValidator<
            BaseEngineTypes,
        >>::ensure_well_formed_attributes(
            &validator, EngineApiMessageVersion::V3, &attributes
        );
        assert_invalid_params_error!(result, "InvalidTimestampMillisPartInPayloadAttributes");
    }

    #[test]
    fn test_well_formed_attributes_post_zombie_with_valid_timestamp_millis_part() {
        let validator = zombie_validator();
        let mut attributes = zombie_attributes(42);
        attributes.timestamp_millis_part = Some(200);

        let result = <engine::BaseEngineValidator<_, _, _> as EngineApiValidator<
            BaseEngineTypes,
        >>::ensure_well_formed_attributes(
            &validator, EngineApiMessageVersion::V3, &attributes
        );
        assert!(result.is_ok());
    }

    fn validate_against_parent(
        validator: &BaseEngineValidator<NoopProvider, BaseTxEnvelope, BaseChainSpec>,
        timestamp: u64,
        timestamp_millis_part: Option<u16>,
        parent_timestamp: u64,
    ) -> Result<(), InvalidPayloadAttributesError> {
        let mut attributes = zombie_attributes(timestamp);
        attributes.timestamp_millis_part = timestamp_millis_part;
        let header = Header { timestamp: parent_timestamp, ..Default::default() };

        <engine::BaseEngineValidator<_, _, _> as PayloadValidator<BaseEngineTypes>>::
            validate_payload_attributes_against_header(validator, &attributes, &header)
    }

    #[test]
    fn test_payload_attributes_post_zombie_accept_same_second() {
        let validator = zombie_validator();

        assert!(validate_against_parent(&validator, 42, Some(200), 42).is_ok());
    }

    #[test]
    fn test_payload_attributes_post_zombie_accept_next_second() {
        let validator = zombie_validator();

        assert!(validate_against_parent(&validator, 43, Some(0), 42).is_ok());
    }

    #[test]
    fn test_payload_attributes_post_zombie_reject_backwards_seconds() {
        let validator = zombie_validator();

        assert!(matches!(
            validate_against_parent(&validator, 42, Some(800), 43),
            Err(InvalidPayloadAttributesError::InvalidTimestamp)
        ));
    }

    #[test]
    fn test_payload_attributes_post_zombie_require_millis_part() {
        let validator = zombie_validator();

        assert!(matches!(
            validate_against_parent(&validator, 42, None, 42),
            Err(InvalidPayloadAttributesError::InvalidTimestamp)
        ));
    }

    #[test]
    fn test_payload_attributes_post_zombie_reject_invalid_millis_part() {
        let validator = zombie_validator();

        assert!(matches!(
            validate_against_parent(&validator, 42, Some(999), 42),
            Err(InvalidPayloadAttributesError::InvalidTimestamp)
        ));
    }

    fn parent_state(millis_part: u16) -> MockEthProvider<BasePrimitives> {
        let provider = MockEthProvider::<BasePrimitives>::new();
        provider.add_account(
            Predeploys::BASE_TIME,
            ExtendedAccount::new(0, U256::ZERO).extend_storage([(
                BaseTime::TIMESTAMP_MILLIS_PART_SLOT.into(),
                U256::from(millis_part),
            )]),
        );
        provider
    }

    fn state_updates(millis_part: Option<u16>, wiped: bool) -> HashedPostState {
        let mut state = HashedPostState::default();
        let storage = HashedStorage::from_iter(
            wiped,
            millis_part.into_iter().map(|millis_part| {
                (
                    keccak256(B256::from(BaseTime::TIMESTAMP_MILLIS_PART_SLOT)),
                    U256::from(millis_part),
                )
            }),
        );
        state.storages.insert(keccak256(Predeploys::BASE_TIME), storage);
        state
    }

    fn base_time_block(
        number: u64,
        timestamp: u64,
        millis_part: u16,
        withdrawals_root: B256,
    ) -> RecoveredBlock<BaseBlock> {
        let metadata = BaseTimeUpdateTx::new(millis_part)
            .expect("valid BaseTime metadata")
            .into_deposit_tx(number);
        let block = BaseBlock {
            header: Header {
                number,
                timestamp,
                withdrawals_root: Some(withdrawals_root),
                ..Default::default()
            },
            body: BlockBody {
                transactions: vec![TxDeposit::default().seal_slow().into(), metadata.into()],
                ..Default::default()
            },
        };
        RecoveredBlock::new_sealed(
            SealedBlock::seal_slow(block),
            vec![Address::ZERO, Address::ZERO],
        )
    }

    fn validate_base_time(
        parent_timestamp: u64,
        parent_millis_part: u16,
        child_timestamp: u64,
        claimed_millis_part: u16,
        committed_millis_part: Option<u16>,
        wiped: bool,
    ) -> Result<(), ConsensusError> {
        zombie_validator().validate_block_post_execution_with_state(
            &state_updates(committed_millis_part, wiped),
            parent_state(parent_millis_part),
            parent_timestamp,
            &base_time_block(9, child_timestamp, claimed_millis_part, EMPTY_ROOT_HASH),
        )
    }

    #[test]
    fn base_time_post_execution_accepts_first_active_child() {
        validate_base_time(41, 0, 42, 600, Some(600), false).unwrap();
    }

    #[test]
    fn base_time_post_execution_accepts_same_second_200ms_progression() {
        validate_base_time(42, 200, 42, 400, Some(400), false).unwrap();
    }

    #[test]
    fn base_time_post_execution_accepts_second_boundary_progression() {
        validate_base_time(42, 800, 43, 0, Some(0), false).unwrap();
    }

    #[test]
    fn base_time_post_execution_rejects_skipped_slot() {
        let error = validate_base_time(42, 200, 42, 600, Some(600), false).unwrap_err();
        assert!(matches!(
            error,
            ConsensusError::Other(error)
                if error.downcast_ref::<base_protocol::BaseTimeProgressionError>().is_some()
        ));
    }

    #[test]
    fn base_time_post_execution_rejects_claim_committed_mismatch() {
        let error = validate_base_time(42, 200, 42, 400, Some(600), false).unwrap_err();
        assert!(matches!(
            error,
            ConsensusError::Other(error)
                if matches!(
                    error.downcast_ref::<BaseConsensusError>(),
                    Some(BaseConsensusError::BaseTimeClaimCommittedMismatch {
                        claim: 400,
                        committed: 600,
                    })
                )
        ));
    }

    #[test]
    fn base_time_post_execution_uses_parent_when_child_slot_is_absent() {
        let error = validate_base_time(42, 200, 42, 400, None, false).unwrap_err();
        assert!(matches!(
            error,
            ConsensusError::Other(error)
                if matches!(
                    error.downcast_ref::<BaseConsensusError>(),
                    Some(BaseConsensusError::BaseTimeClaimCommittedMismatch {
                        claim: 400,
                        committed: 200,
                    })
                )
        ));
    }

    #[test]
    fn base_time_post_execution_treats_wiped_storage_as_zero() {
        validate_base_time(42, 800, 43, 0, None, true).unwrap();
    }

    #[test]
    fn base_time_post_execution_rejects_invalid_parent_state() {
        let error = validate_base_time(42, 100, 42, 200, Some(200), false).unwrap_err();
        assert!(matches!(
            error,
            ConsensusError::Other(error)
                if matches!(
                    error.downcast_ref::<BaseConsensusError>(),
                    Some(BaseConsensusError::InvalidParentBaseTimeMillis(100))
                )
        ));
    }

    #[test]
    fn generic_post_execution_validation_checks_isthmus_before_zombie() {
        let block = base_time_block(9, 42, 200, EMPTY_ROOT_HASH);
        let error =
            PayloadValidator::<BaseEngineTypes>::validate_block_post_execution_with_hashed_state(
                &zombie_validator(),
                &HashedPostState::default(),
                &block,
            )
            .unwrap_err();

        assert!(matches!(
            error,
            ConsensusError::Other(error)
                if matches!(
                    error.downcast_ref::<BaseConsensusError>(),
                    Some(BaseConsensusError::L2WithdrawalsRootMismatch { .. })
                )
        ));
    }

    #[test]
    fn generic_post_execution_validation_fails_closed_after_valid_isthmus() {
        let block = base_time_block(9, 42, 200, B256::ZERO);
        let error =
            PayloadValidator::<BaseEngineTypes>::validate_block_post_execution_with_hashed_state(
                &zombie_validator(),
                &HashedPostState::default(),
                &block,
            )
            .unwrap_err();

        assert!(matches!(
            error,
            ConsensusError::Other(error)
                if matches!(
                    error.downcast_ref::<BaseConsensusError>(),
                    Some(BaseConsensusError::BaseTimeValidationContextRequired)
                )
        ));
    }
}
