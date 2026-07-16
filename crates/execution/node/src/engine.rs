use std::{marker::PhantomData, sync::Arc};

use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_engine::{ExecutionPayloadEnvelopeV2, ExecutionPayloadV1};
use base_common_chains::{BaseUpgrade, ChainConfig, Upgrades};
use base_common_consensus::{BaseBlock, BaseTransaction, Predeploys};
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
use reth_chainspec::{EthChainSpec, ForkCondition};
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
    ChainSpec: EthChainSpec + Upgrades,
{
    /// Returns the chain spec used by the validator.
    #[inline]
    pub fn chain_spec(&self) -> &ChainSpec {
        self.inner.chain_spec()
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

    /// Verifies the `BaseTime` claim, committed state, and expected timestamp after block execution.
    pub fn validate_base_time_post_execution<DB>(
        &self,
        state_updates: &HashedPostState,
        parent_state: &DB,
        block: &RecoveredBlock<alloy_consensus::Block<Tx>>,
    ) -> Result<(), ConsensusError>
    where
        DB: StateProvider + ?Sized,
        Tx: BaseTransaction + SignedTransaction,
    {
        let claimed_base_time =
            BaseTimeUpdateTx::extract_from_transactions(&block.body().transactions, block.number())
                .map_err(ConsensusError::other)?;
        let claimed_millis_part = claimed_base_time.timestamp_millis_part();

        // TODO: Validate against the parent timestamp once BasicEngineValidatorBuilder provides it.
        let chain_id = self.chain_spec().chain().id();
        let unavailable = |reason| {
            ConsensusError::other(BaseConsensusError::BaseTimeTimestampUnavailable {
                chain_id,
                block_number: block.number(),
                reason,
            })
        };
        let config = ChainConfig::by_chain_id(chain_id)
            .ok_or_else(|| unavailable("unknown chain configuration"))?;
        let ForkCondition::Timestamp(activation_timestamp) =
            self.chain_spec().upgrade_activation(BaseUpgrade::Zombie)
        else {
            return Err(unavailable("non-timestamp activation"));
        };
        let activation_offset = activation_timestamp
            .checked_sub(config.genesis_l2_time)
            .ok_or_else(|| unavailable("activation before genesis"))?;
        if config.block_time == 0 {
            return Err(unavailable("zero block time"));
        }
        if !activation_offset.is_multiple_of(config.block_time) {
            return Err(unavailable("misaligned activation"));
        }
        let activation_block = activation_offset / config.block_time;
        let elapsed_blocks = block
            .number()
            .checked_sub(activation_block)
            .ok_or_else(|| unavailable("block before activation"))?;
        let expected_timestamp_ms = u128::from(activation_timestamp) * 1_000
            + u128::from(elapsed_blocks) * u128::from(BaseTimeUpdateTx::BLOCK_INTERVAL_MILLIS);
        let actual_timestamp_ms =
            u128::from(block.timestamp()) * 1_000 + u128::from(claimed_millis_part);
        if actual_timestamp_ms != expected_timestamp_ms {
            return Err(ConsensusError::other(BaseConsensusError::BaseTimeTimestampMismatch {
                expected_timestamp_ms,
                actual_timestamp_ms,
            }));
        }

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

        Ok(())
    }
}

impl<P, Tx, ChainSpec, Types> PayloadValidator<Types> for BaseEngineValidator<P, Tx, ChainSpec>
where
    P: StateProviderFactory + Unpin + 'static,
    Tx: BaseTransaction + SignedTransaction + Unpin + 'static,
    ChainSpec: EthChainSpec + Upgrades + Send + Sync + 'static,
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

        if !self.chain_spec().is_isthmus_active_at_timestamp(timestamp) {
            return Ok(());
        }

        let parent_state =
            self.provider.state_by_block_hash(block.parent_hash()).map_err(|err| {
                ConsensusError::Other(Arc::from(Box::<dyn core::error::Error + Send + Sync>::from(
                    format!("failed to load parent state for post-execution validation: {err}"),
                )))
            })?;

        self.validate_isthmus_post_execution(state_updates, &parent_state, block.header())?;

        if self.chain_spec().is_zombie_active_at_timestamp(timestamp) {
            self.validate_base_time_post_execution(state_updates, &parent_state, block)?;
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

        // The parent header does not contain its millisecond component. The exact 200ms slot is
        // enforced against the hardfork schedule during post-execution validation.
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
    ChainSpec: EthChainSpec + Upgrades + Send + Sync + 'static,
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

    const ZOMBIE_TIMESTAMP: u64 = 1_800_000_001;
    const ZOMBIE_ACTIVATION_BLOCK: u64 = 56_605_327;

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
                .with_fork(BaseUpgrade::Zombie, ForkCondition::Timestamp(ZOMBIE_TIMESTAMP))
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
        let attributes = zombie_attributes(ZOMBIE_TIMESTAMP);

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
        let mut attributes = zombie_attributes(ZOMBIE_TIMESTAMP);
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
        let mut attributes = zombie_attributes(ZOMBIE_TIMESTAMP);
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

        assert!(
            validate_against_parent(&validator, ZOMBIE_TIMESTAMP, Some(200), ZOMBIE_TIMESTAMP,)
                .is_ok()
        );
    }

    #[test]
    fn test_payload_attributes_post_zombie_accept_next_second() {
        let validator = zombie_validator();

        assert!(
            validate_against_parent(&validator, ZOMBIE_TIMESTAMP + 1, Some(0), ZOMBIE_TIMESTAMP,)
                .is_ok()
        );
    }

    #[test]
    fn test_payload_attributes_post_zombie_reject_backwards_seconds() {
        let validator = zombie_validator();

        assert!(matches!(
            validate_against_parent(&validator, ZOMBIE_TIMESTAMP, Some(800), ZOMBIE_TIMESTAMP + 1,),
            Err(InvalidPayloadAttributesError::InvalidTimestamp)
        ));
    }

    #[test]
    fn test_payload_attributes_post_zombie_require_millis_part() {
        let validator = zombie_validator();

        assert!(matches!(
            validate_against_parent(&validator, ZOMBIE_TIMESTAMP, None, ZOMBIE_TIMESTAMP),
            Err(InvalidPayloadAttributesError::InvalidTimestamp)
        ));
    }

    #[test]
    fn test_payload_attributes_post_zombie_reject_invalid_millis_part() {
        let validator = zombie_validator();

        assert!(matches!(
            validate_against_parent(&validator, ZOMBIE_TIMESTAMP, Some(999), ZOMBIE_TIMESTAMP,),
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
        if millis_part.is_none() && !wiped {
            return state;
        }
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

    fn validate_base_time_with(
        validator: &BaseEngineValidator<NoopProvider, BaseTxEnvelope, BaseChainSpec>,
        block_number: u64,
        parent_millis_part: u16,
        child_timestamp: u64,
        claimed_millis_part: u16,
        committed_millis_part: Option<u16>,
        wiped: bool,
    ) -> Result<(), ConsensusError> {
        validator.validate_base_time_post_execution(
            &state_updates(committed_millis_part, wiped),
            &parent_state(parent_millis_part),
            &base_time_block(block_number, child_timestamp, claimed_millis_part, EMPTY_ROOT_HASH),
        )
    }

    fn validate_base_time(
        block_number: u64,
        parent_millis_part: u16,
        child_timestamp: u64,
        claimed_millis_part: u16,
        committed_millis_part: Option<u16>,
        wiped: bool,
    ) -> Result<(), ConsensusError> {
        validate_base_time_with(
            &zombie_validator(),
            block_number,
            parent_millis_part,
            child_timestamp,
            claimed_millis_part,
            committed_millis_part,
            wiped,
        )
    }

    fn base_consensus_error(error: &ConsensusError) -> Option<&BaseConsensusError> {
        let ConsensusError::Other(error) = error else {
            return None;
        };
        error.downcast_ref()
    }

    #[test]
    fn base_time_post_execution_accepts_first_scheduled_slot_without_storage_update() {
        validate_base_time(ZOMBIE_ACTIVATION_BLOCK, 0, ZOMBIE_TIMESTAMP, 0, None, false).unwrap();
    }

    #[test]
    fn base_time_post_execution_accepts_scheduled_same_second_slot() {
        validate_base_time(
            ZOMBIE_ACTIVATION_BLOCK + 2,
            200,
            ZOMBIE_TIMESTAMP,
            400,
            Some(400),
            false,
        )
        .unwrap();
    }

    #[test]
    fn base_time_post_execution_accepts_scheduled_second_boundary() {
        validate_base_time(
            ZOMBIE_ACTIVATION_BLOCK + 5,
            800,
            ZOMBIE_TIMESTAMP + 1,
            0,
            Some(0),
            false,
        )
        .unwrap();
    }

    #[test]
    fn base_time_post_execution_rejects_millis_not_scheduled_for_block_number() {
        let error = validate_base_time(
            ZOMBIE_ACTIVATION_BLOCK + 2,
            200,
            ZOMBIE_TIMESTAMP,
            600,
            Some(600),
            false,
        )
        .unwrap_err();
        assert!(matches!(
            base_consensus_error(&error),
            Some(BaseConsensusError::BaseTimeTimestampMismatch {
                expected_timestamp_ms: 1_800_000_001_400,
                actual_timestamp_ms: 1_800_000_001_600,
            })
        ));
    }

    #[test]
    fn base_time_post_execution_rejects_unknown_chain_config() {
        let chain_spec = BaseChainSpecBuilder::base_mainnet()
            .chain(Default::default())
            .with_fork(BaseUpgrade::Zombie, ForkCondition::Timestamp(ZOMBIE_TIMESTAMP))
            .build();
        assert!(ChainConfig::by_chain_id(chain_spec.chain().id()).is_none());

        let validator = validator_with_chain_spec(chain_spec);
        let error = validate_base_time_with(
            &validator,
            ZOMBIE_ACTIVATION_BLOCK,
            0,
            ZOMBIE_TIMESTAMP,
            0,
            None,
            false,
        )
        .unwrap_err();

        assert!(matches!(
            base_consensus_error(&error),
            Some(BaseConsensusError::BaseTimeTimestampUnavailable {
                block_number: ZOMBIE_ACTIVATION_BLOCK,
                reason: "unknown chain configuration",
                ..
            })
        ));
    }

    #[test]
    fn base_time_post_execution_rejects_misaligned_zombie_activation() {
        let validator = validator_with_chain_spec(
            BaseChainSpecBuilder::base_mainnet()
                .with_fork(BaseUpgrade::Zombie, ForkCondition::Timestamp(ZOMBIE_TIMESTAMP + 1))
                .build(),
        );
        let error = validate_base_time_with(
            &validator,
            ZOMBIE_ACTIVATION_BLOCK,
            0,
            ZOMBIE_TIMESTAMP + 1,
            0,
            None,
            false,
        )
        .unwrap_err();

        assert!(matches!(
            base_consensus_error(&error),
            Some(BaseConsensusError::BaseTimeTimestampUnavailable {
                chain_id: 8453,
                block_number: ZOMBIE_ACTIVATION_BLOCK,
                reason: "misaligned activation",
            })
        ));
    }

    #[test]
    fn base_time_post_execution_rejects_claim_committed_mismatch() {
        let error = validate_base_time(
            ZOMBIE_ACTIVATION_BLOCK + 2,
            200,
            ZOMBIE_TIMESTAMP,
            400,
            Some(600),
            false,
        )
        .unwrap_err();
        assert!(matches!(
            base_consensus_error(&error),
            Some(BaseConsensusError::BaseTimeClaimCommittedMismatch { claim: 400, committed: 600 })
        ));
    }

    #[test]
    fn base_time_post_execution_uses_parent_when_child_slot_is_absent() {
        let error = validate_base_time(
            ZOMBIE_ACTIVATION_BLOCK + 2,
            200,
            ZOMBIE_TIMESTAMP,
            400,
            None,
            false,
        )
        .unwrap_err();
        assert!(matches!(
            base_consensus_error(&error),
            Some(BaseConsensusError::BaseTimeClaimCommittedMismatch { claim: 400, committed: 200 })
        ));
    }

    #[test]
    fn base_time_post_execution_treats_wiped_storage_as_zero() {
        validate_base_time(ZOMBIE_ACTIVATION_BLOCK + 5, 800, ZOMBIE_TIMESTAMP + 1, 0, None, true)
            .unwrap();
    }

    #[test]
    fn generic_post_execution_validation_checks_isthmus_before_zombie() {
        let block = base_time_block(ZOMBIE_ACTIVATION_BLOCK, ZOMBIE_TIMESTAMP, 0, EMPTY_ROOT_HASH);
        let error =
            PayloadValidator::<BaseEngineTypes>::validate_block_post_execution_with_hashed_state(
                &zombie_validator(),
                &HashedPostState::default(),
                &block,
            )
            .unwrap_err();

        assert!(matches!(
            base_consensus_error(&error),
            Some(BaseConsensusError::L2WithdrawalsRootMismatch { .. })
        ));
    }

    #[test]
    fn generic_post_execution_validation_accepts_derived_child_timestamp() {
        let block = base_time_block(ZOMBIE_ACTIVATION_BLOCK + 1, ZOMBIE_TIMESTAMP, 200, B256::ZERO);

        PayloadValidator::<BaseEngineTypes>::validate_block_post_execution_with_hashed_state(
            &zombie_validator(),
            &state_updates(Some(200), false),
            &block,
        )
        .unwrap();
    }

    #[test]
    fn generic_post_execution_validation_rejects_inexact_derived_child_timestamp() {
        let block =
            base_time_block(ZOMBIE_ACTIVATION_BLOCK + 1, ZOMBIE_TIMESTAMP + 1, 200, B256::ZERO);
        let error =
            PayloadValidator::<BaseEngineTypes>::validate_block_post_execution_with_hashed_state(
                &zombie_validator(),
                &state_updates(Some(200), false),
                &block,
            )
            .unwrap_err();

        assert!(matches!(
            base_consensus_error(&error),
            Some(BaseConsensusError::BaseTimeTimestampMismatch {
                expected_timestamp_ms: 1_800_000_001_200,
                actual_timestamp_ms: 1_800_000_002_200,
            })
        ));
    }
}
