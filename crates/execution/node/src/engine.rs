use std::{marker::PhantomData, sync::Arc};

use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_engine::{ExecutionPayloadEnvelopeV2, ExecutionPayloadV1};
use base_common_chains::Upgrades;
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
use base_protocol::{BaseTimeMetadataError, BaseTimeUpdateTx};
use reth_chainspec::EthChainSpec;
use reth_consensus::ConsensusError;
use reth_node_api::{
    BuiltPayload, EngineApiValidator, EngineTypes, InsertBlockErrorKind, NodePrimitives,
    PayloadValidator,
    payload::{
        EngineApiMessageVersion, EngineObjectValidationError, MessageValidationKind,
        NewPayloadError, PayloadOrAttributes, PayloadTypes, VersionSpecificValidationError,
        validate_parent_beacon_block_root_presence,
    },
    validate_version_specific_fields,
};
use reth_payload_primitives::{InvalidPayloadAttributesError, PayloadAttributes};
use reth_primitives_traits::{Block, RecoveredBlock, SealedBlock, SealedHeader, SignedTransaction};
use reth_provider::StateProvider;
use reth_storage_api::{StateProviderBox, errors::ProviderResult};
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
pub struct BaseEngineValidator<Tx, ChainSpec> {
    inner: BaseExecutionPayloadValidator<ChainSpec>,
    hashed_addr_l2tol1_msg_passer: B256,
    phantom: PhantomData<Tx>,
}

impl<Tx, ChainSpec> BaseEngineValidator<Tx, ChainSpec> {
    /// Instantiates a new validator.
    pub fn new<KH: KeyHasher>(chain_spec: Arc<ChainSpec>) -> Self {
        let hashed_addr_l2tol1_msg_passer = KH::hash_key(Predeploys::L2_TO_L1_MESSAGE_PASSER);
        Self {
            inner: BaseExecutionPayloadValidator::new(chain_spec),
            hashed_addr_l2tol1_msg_passer,
            phantom: PhantomData,
        }
    }
}

impl<Tx, ChainSpec> Clone for BaseEngineValidator<Tx, ChainSpec>
where
    ChainSpec: Upgrades,
{
    fn clone(&self) -> Self {
        Self {
            inner: BaseExecutionPayloadValidator::new(self.inner.clone()),
            hashed_addr_l2tol1_msg_passer: self.hashed_addr_l2tol1_msg_passer,
            phantom: Default::default(),
        }
    }
}

impl<Tx, ChainSpec> BaseEngineValidator<Tx, ChainSpec>
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
}

impl<Tx, ChainSpec, Types> PayloadValidator<Types> for BaseEngineValidator<Tx, ChainSpec>
where
    Tx: BaseTransaction + SignedTransaction + Unpin + 'static,
    ChainSpec: EthChainSpec + Upgrades + Send + Sync + 'static,
    Types: PayloadTypes<ExecutionData = ExecutionData>,
    Types::PayloadAttributes: Attributes<Transaction = Tx>,
{
    type Block = alloy_consensus::Block<Tx>;

    fn validate_block_post_execution_with_hashed_state<'a>(
        &self,
        state_updates: impl FnOnce() -> &'a HashedPostState,
        block: &RecoveredBlock<Self::Block>,
        parent_header: &SealedHeader<<Self::Block as Block>::Header>,
        parent_state: impl FnOnce() -> ProviderResult<StateProviderBox>,
    ) -> Result<(), InsertBlockErrorKind> {
        let timestamp = block.timestamp();
        if !self.chain_spec().is_isthmus_active_at_timestamp(timestamp) {
            return Ok(());
        }

        let parent_state = parent_state()?;
        let state_updates = state_updates();
        self.validate_isthmus_post_execution(state_updates, parent_state.as_ref(), block.header())?;

        if !self.chain_spec().is_cobalt_active_at_timestamp(timestamp) {
            return Ok(());
        }

        let child_millis =
            BaseTimeUpdateTx::extract_from_transactions(&block.body().transactions, block.number())
                .map_err(ConsensusError::other)?
                .timestamp_millis_part();

        if !self.chain_spec().is_cobalt_active_at_timestamp(parent_header.timestamp()) {
            // The legacy parent has no millisecond component to anchor the 200ms progression
            // check, so the activation block must claim exactly 0 instead.
            if child_millis != 0 {
                return Err(ConsensusError::other(
                    BaseConsensusError::BaseTimeActivationMillisNonZero {
                        timestamp_millis_part: child_millis,
                    },
                )
                .into());
            }
            return Ok(());
        }

        let parent_millis = BaseTime::decode_timestamp_millis_part(
            parent_state
                .storage(Predeploys::BASE_TIME, BaseTime::TIMESTAMP_MILLIS_PART_SLOT.into())?
                .unwrap_or_default(),
        );
        let parent_timestamp_ms =
            u128::from(parent_header.timestamp()) * 1_000 + u128::from(parent_millis);
        let child_timestamp_ms = u128::from(timestamp) * 1_000 + u128::from(child_millis);

        if child_timestamp_ms
            != parent_timestamp_ms + u128::from(BaseTimeUpdateTx::BLOCK_INTERVAL_MILLIS)
        {
            return Err(ConsensusError::other(BaseConsensusError::BaseTimeProgressionInvalid {
                parent_timestamp_ms,
                child_timestamp_ms,
            })
            .into());
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
        if !self.chain_spec().is_cobalt_active_at_timestamp(timestamp) {
            return (timestamp > header.timestamp())
                .then_some(())
                .ok_or(InvalidPayloadAttributesError::InvalidTimestamp);
        }

        let invalid_metadata = |error: BaseTimeMetadataError| {
            InvalidPayloadAttributesError::InvalidParams(Box::new(error))
        };
        let transaction = attributes
            .sequencer_transactions()
            .get(1)
            .ok_or_else(|| invalid_metadata(BaseTimeMetadataError::Missing))?;
        let deposit = transaction
            .value()
            .as_deposit()
            .ok_or_else(|| invalid_metadata(BaseTimeMetadataError::NotDeposit))?;
        BaseTimeUpdateTx::validate_deposit(deposit, header.number() + 1)
            .map_err(invalid_metadata)?;

        // The parent header does not contain its millisecond component, so only whole-second
        // ordering can be checked here.
        (timestamp >= header.timestamp())
            .then_some(())
            .ok_or(InvalidPayloadAttributesError::InvalidTimestamp)
    }
}

impl<Types, Tx, ChainSpec> EngineApiValidator<Types> for BaseEngineValidator<Tx, ChainSpec>
where
    Types: PayloadTypes<
            PayloadAttributes = BasePayloadBuilderAttributes<Tx>,
            ExecutionData = ExecutionData,
            BuiltPayload: BuiltPayload<Primitives: NodePrimitives<SignedTx = Tx>>,
        >,
    Tx: BaseTransaction + SignedTransaction + Unpin + 'static,
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
    use alloy_primitives::{Address, B64, B256, U256, b64};
    use alloy_rpc_types_engine::PayloadAttributes;
    use base_common_chains::{BaseUpgrade, ChainConfig};
    use base_common_consensus::{BasePrimitives, BaseTxEnvelope, TxDeposit};
    use base_common_rpc_types_engine::BasePayloadAttributes;
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use base_execution_consensus::BaseConsensusError;
    use reth_ethereum_forks::ForkCondition;
    use reth_primitives_traits::WithEncoded;
    use reth_provider::{
        noop::NoopProvider,
        test_utils::{ExtendedAccount, MockEthProvider},
    };
    use reth_trie_common::KeccakKeyHasher;

    use super::*;
    use crate::engine;

    const COBALT_TIMESTAMP: u64 = 1_800_000_001;

    fn validator_with_chain_spec(
        chain_spec: BaseChainSpec,
    ) -> BaseEngineValidator<BaseTxEnvelope, BaseChainSpec> {
        BaseEngineValidator::<BaseTxEnvelope, BaseChainSpec>::new::<KeccakKeyHasher>(Arc::new(
            chain_spec,
        ))
    }

    fn validator() -> BaseEngineValidator<BaseTxEnvelope, BaseChainSpec> {
        validator_with_chain_spec(BaseChainSpec::sepolia())
    }

    fn cobalt_validator() -> BaseEngineValidator<BaseTxEnvelope, BaseChainSpec> {
        validator_with_chain_spec(
            BaseChainSpecBuilder::base_mainnet()
                .with_fork(BaseUpgrade::Cobalt, ForkCondition::Timestamp(COBALT_TIMESTAMP))
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
                payload_attributes: PayloadAttributes {
                    timestamp,
                    prev_randao: B256::ZERO,
                    suggested_fee_recipient: Address::ZERO,
                    withdrawals: Some(vec![]),
                    parent_beacon_block_root: Some(B256::ZERO),
                    slot_number: None,
                    target_gas_limit: None,
                },
            },
            3,
        )
        .expect("valid test payload attributes")
    }

    fn cobalt_attributes(timestamp: u64) -> BasePayloadBuilderAttributes<BaseTxEnvelope> {
        get_attributes(Some(b64!("0000000000000000")), Some(1), timestamp)
    }

    fn add_base_time_transaction(
        attributes: &mut BasePayloadBuilderAttributes<BaseTxEnvelope>,
        millis_part: u16,
    ) {
        let metadata = BaseTimeUpdateTx::new(millis_part).unwrap().into_deposit_tx(9);
        attributes.transactions = vec![
            WithEncoded::from_2718_encodable(TxDeposit::default().seal_slow().into()),
            WithEncoded::from_2718_encodable(metadata.into()),
        ];
    }

    #[test]
    fn test_well_formed_attributes_pre_holocene() {
        let validator = validator();
        let attributes = get_attributes(None, None, 1732633199);

        let result = <engine::BaseEngineValidator<_, _> as EngineApiValidator<
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

        let result = <engine::BaseEngineValidator<_, _> as EngineApiValidator<
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

        let result = <engine::BaseEngineValidator<_, _> as EngineApiValidator<
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

        let result = <engine::BaseEngineValidator<_, _> as EngineApiValidator<
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

        let result = <engine::BaseEngineValidator<_, _> as EngineApiValidator<
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

        let result = <engine::BaseEngineValidator<_, _> as EngineApiValidator<
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

        let result = <engine::BaseEngineValidator<_, _> as EngineApiValidator<
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

        let result = <engine::BaseEngineValidator<_, _> as EngineApiValidator<
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

        let result = <engine::BaseEngineValidator<_, _> as EngineApiValidator<
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

        let result = <engine::BaseEngineValidator<_, _> as EngineApiValidator<
            BaseEngineTypes,
        >>::ensure_well_formed_attributes(
            &validator, EngineApiMessageVersion::V3, &attributes
        );
        assert_invalid_params_error!(result, "MissingMinBaseFeeInPayloadAttributes");
    }

    fn validate_against_parent(
        validator: &BaseEngineValidator<BaseTxEnvelope, BaseChainSpec>,
        timestamp: u64,
        timestamp_millis_part: u16,
        parent_timestamp: u64,
    ) -> Result<(), InvalidPayloadAttributesError> {
        let mut attributes = cobalt_attributes(timestamp);
        add_base_time_transaction(&mut attributes, timestamp_millis_part);
        let header = Header { number: 8, timestamp: parent_timestamp, ..Default::default() };

        <engine::BaseEngineValidator<_, _> as PayloadValidator<BaseEngineTypes>>::
            validate_payload_attributes_against_header(validator, &attributes, &header)
    }

    #[test]
    fn test_payload_attributes_post_cobalt_accept_same_second() {
        let validator = cobalt_validator();

        assert!(
            validate_against_parent(&validator, COBALT_TIMESTAMP, 200, COBALT_TIMESTAMP).is_ok()
        );
    }

    #[test]
    fn test_payload_attributes_post_cobalt_accept_next_second() {
        let validator = cobalt_validator();

        assert!(
            validate_against_parent(&validator, COBALT_TIMESTAMP + 1, 0, COBALT_TIMESTAMP).is_ok()
        );
    }

    #[test]
    fn test_payload_attributes_post_cobalt_reject_backwards_seconds() {
        let validator = cobalt_validator();

        assert!(matches!(
            validate_against_parent(&validator, COBALT_TIMESTAMP, 800, COBALT_TIMESTAMP + 1),
            Err(InvalidPayloadAttributesError::InvalidTimestamp)
        ));
    }

    #[test]
    fn test_payload_attributes_post_cobalt_require_base_time_transaction() {
        let validator = cobalt_validator();
        let attributes = cobalt_attributes(COBALT_TIMESTAMP);
        let header = Header { number: 8, timestamp: COBALT_TIMESTAMP, ..Default::default() };

        let result = <engine::BaseEngineValidator<_, _> as PayloadValidator<BaseEngineTypes>>::
            validate_payload_attributes_against_header(&validator, &attributes, &header);

        assert_eq!(
            result.unwrap_err().to_string(),
            "Invalid params: missing BaseTime metadata deposit at tx[1]"
        );
    }

    #[test]
    fn test_payload_attributes_post_cobalt_reject_invalid_base_time_transaction() {
        let validator = cobalt_validator();
        let mut attributes = cobalt_attributes(COBALT_TIMESTAMP);
        attributes.transactions = vec![
            WithEncoded::from_2718_encodable(TxDeposit::default().seal_slow().into()),
            WithEncoded::from_2718_encodable(TxDeposit::default().seal_slow().into()),
        ];
        let header = Header { number: 8, timestamp: COBALT_TIMESTAMP, ..Default::default() };

        let result = <engine::BaseEngineValidator<_, _> as PayloadValidator<BaseEngineTypes>>::
            validate_payload_attributes_against_header(&validator, &attributes, &header);

        assert_eq!(
            result.unwrap_err().to_string(),
            "Invalid params: invalid BaseTime metadata source hash"
        );
    }

    fn post_execution_block(withdrawals_root: B256) -> RecoveredBlock<BaseBlock> {
        let block = BaseBlock {
            header: Header {
                timestamp: COBALT_TIMESTAMP,
                withdrawals_root: Some(withdrawals_root),
                ..Default::default()
            },
            body: Default::default(),
        };
        RecoveredBlock::new_sealed(SealedBlock::seal_slow(block), vec![])
    }

    fn base_time_block(
        timestamp: u64,
        millis_part: u16,
        withdrawals_root: B256,
    ) -> RecoveredBlock<BaseBlock> {
        let number = 9;
        let metadata = BaseTimeUpdateTx::new(millis_part).unwrap().into_deposit_tx(number);
        base_time_block_with_transactions(
            timestamp,
            withdrawals_root,
            vec![TxDeposit::default().seal_slow().into(), metadata.into()],
        )
    }

    fn base_time_block_with_transactions(
        timestamp: u64,
        withdrawals_root: B256,
        transactions: Vec<BaseTxEnvelope>,
    ) -> RecoveredBlock<BaseBlock> {
        let block = BaseBlock {
            header: Header {
                number: 9,
                timestamp,
                withdrawals_root: Some(withdrawals_root),
                ..Default::default()
            },
            body: BlockBody { transactions, ..Default::default() },
        };
        let signers = vec![Address::ZERO; block.body.transactions.len()];
        RecoveredBlock::new_sealed(SealedBlock::seal_slow(block), signers)
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

    fn validate_base_time_progression(
        parent_timestamp: u64,
        parent_millis_part: u16,
        child_timestamp: u64,
        child_millis_part: u16,
        withdrawals_root: B256,
    ) -> Result<(), InsertBlockErrorKind> {
        let validator = validator_with_chain_spec(
            BaseChainSpecBuilder::base_mainnet()
                .with_fork(BaseUpgrade::Cobalt, ForkCondition::Timestamp(COBALT_TIMESTAMP))
                .build(),
        );
        let block = base_time_block(child_timestamp, child_millis_part, withdrawals_root);
        let parent =
            SealedHeader::seal_slow(Header { timestamp: parent_timestamp, ..Default::default() });
        let state_updates = HashedPostState::default();
        let parent_state = parent_state(parent_millis_part);

        PayloadValidator::<BaseEngineTypes>::validate_block_post_execution_with_hashed_state(
            &validator,
            || &state_updates,
            &block,
            &parent,
            || Ok(Box::new(parent_state)),
        )
    }

    fn base_consensus_error(error: &InsertBlockErrorKind) -> Option<&BaseConsensusError> {
        let InsertBlockErrorKind::Consensus(ConsensusError::Other(error)) = error else {
            return None;
        };
        error.downcast_ref()
    }

    fn base_time_metadata_error(error: &InsertBlockErrorKind) -> Option<&BaseTimeMetadataError> {
        let InsertBlockErrorKind::Consensus(ConsensusError::Other(error)) = error else {
            return None;
        };
        error.downcast_ref()
    }

    #[test]
    fn post_execution_skips_base_time_at_activation_but_checks_isthmus() {
        let block = post_execution_block(EMPTY_ROOT_HASH);
        let parent = SealedHeader::seal_slow(Header {
            timestamp: COBALT_TIMESTAMP - 1,
            ..Default::default()
        });
        let state_updates = HashedPostState::default();
        let error =
            PayloadValidator::<BaseEngineTypes>::validate_block_post_execution_with_hashed_state(
                &cobalt_validator(),
                || &state_updates,
                &block,
                &parent,
                || Ok(Box::new(NoopProvider::default())),
            )
            .unwrap_err();

        assert!(matches!(
            base_consensus_error(&error),
            Some(BaseConsensusError::L2WithdrawalsRootMismatch { .. })
        ));
    }

    #[test]
    fn post_execution_validates_exact_200ms_progression() {
        for (parent_seconds, parent_millis, child_seconds, child_millis) in [
            (COBALT_TIMESTAMP, 0, COBALT_TIMESTAMP, 200),
            (COBALT_TIMESTAMP, 200, COBALT_TIMESTAMP, 400),
            (COBALT_TIMESTAMP, 400, COBALT_TIMESTAMP, 600),
            (COBALT_TIMESTAMP, 600, COBALT_TIMESTAMP, 800),
            (COBALT_TIMESTAMP, 800, COBALT_TIMESTAMP + 1, 0),
        ] {
            validate_base_time_progression(
                parent_seconds,
                parent_millis,
                child_seconds,
                child_millis,
                EMPTY_ROOT_HASH,
            )
            .unwrap();
        }

        for (parent_seconds, parent_millis, child_seconds, child_millis) in [
            (COBALT_TIMESTAMP, 200, COBALT_TIMESTAMP, 200),
            (COBALT_TIMESTAMP, 400, COBALT_TIMESTAMP, 200),
            (COBALT_TIMESTAMP, 200, COBALT_TIMESTAMP, 600),
            (COBALT_TIMESTAMP, 800, COBALT_TIMESTAMP + 1, 200),
            (COBALT_TIMESTAMP, 800, COBALT_TIMESTAMP, 800),
            (COBALT_TIMESTAMP, 200, COBALT_TIMESTAMP + 1, 400),
        ] {
            let error = validate_base_time_progression(
                parent_seconds,
                parent_millis,
                child_seconds,
                child_millis,
                EMPTY_ROOT_HASH,
            )
            .unwrap_err();
            assert!(matches!(
                base_consensus_error(&error),
                Some(BaseConsensusError::BaseTimeProgressionInvalid { .. })
            ));
        }
    }

    #[test]
    fn post_execution_progression_error_contains_full_timestamps() {
        for (parent_seconds, parent_millis, child_seconds, child_millis) in [
            (COBALT_TIMESTAMP, 200, COBALT_TIMESTAMP, 600),
            (COBALT_TIMESTAMP, 800, COBALT_TIMESTAMP + 1, 200),
            (COBALT_TIMESTAMP, 400, COBALT_TIMESTAMP, 200),
        ] {
            let error = validate_base_time_progression(
                parent_seconds,
                parent_millis,
                child_seconds,
                child_millis,
                EMPTY_ROOT_HASH,
            )
            .unwrap_err();
            assert!(matches!(
                base_consensus_error(&error),
                Some(BaseConsensusError::BaseTimeProgressionInvalid {
                    parent_timestamp_ms,
                    child_timestamp_ms,
                }) if *parent_timestamp_ms == u128::from(parent_seconds) * 1_000
                    + u128::from(parent_millis)
                    && *child_timestamp_ms == u128::from(child_seconds) * 1_000
                        + u128::from(child_millis)
            ));
        }
    }

    #[test]
    fn post_execution_rejects_invalid_claim() {
        for (transactions, expected) in [
            (vec![], BaseTimeMetadataError::Missing),
            (
                vec![
                    TxDeposit::default().seal_slow().into(),
                    TxDeposit::default().seal_slow().into(),
                ],
                BaseTimeMetadataError::InvalidSourceHash,
            ),
        ] {
            let block =
                base_time_block_with_transactions(COBALT_TIMESTAMP, EMPTY_ROOT_HASH, transactions);
            let parent = SealedHeader::seal_slow(Header {
                timestamp: COBALT_TIMESTAMP,
                ..Default::default()
            });
            let state_updates = HashedPostState::default();
            let error = PayloadValidator::<BaseEngineTypes>::
                validate_block_post_execution_with_hashed_state(
                    &cobalt_validator(),
                    || &state_updates,
                    &block,
                    &parent,
                    || Ok(Box::new(parent_state(200))),
                )
                .unwrap_err();

            assert_eq!(base_time_metadata_error(&error), Some(&expected));
        }
    }

    #[test]
    fn post_execution_accepts_zero_claim_on_first_active_block() {
        let block = base_time_block(COBALT_TIMESTAMP, 0, EMPTY_ROOT_HASH);
        let parent = SealedHeader::seal_slow(Header {
            timestamp: COBALT_TIMESTAMP - 1,
            ..Default::default()
        });
        let state_updates = HashedPostState::default();

        PayloadValidator::<BaseEngineTypes>::validate_block_post_execution_with_hashed_state(
            &cobalt_validator(),
            || &state_updates,
            &block,
            &parent,
            || Ok(Box::new(parent_state(0))),
        )
        .unwrap();
    }

    #[test]
    fn post_execution_rejects_nonzero_claim_on_first_active_block() {
        for millis_part in [200, 400, 600, 800] {
            let block = base_time_block(COBALT_TIMESTAMP, millis_part, EMPTY_ROOT_HASH);
            let parent = SealedHeader::seal_slow(Header {
                timestamp: COBALT_TIMESTAMP - 1,
                ..Default::default()
            });
            let state_updates = HashedPostState::default();
            let error = PayloadValidator::<BaseEngineTypes>::
                validate_block_post_execution_with_hashed_state(
                    &cobalt_validator(),
                    || &state_updates,
                    &block,
                    &parent,
                    || Ok(Box::new(parent_state(0))),
                )
                .unwrap_err();

            assert!(matches!(
                base_consensus_error(&error),
                Some(BaseConsensusError::BaseTimeActivationMillisNonZero {
                    timestamp_millis_part,
                }) if *timestamp_millis_part == millis_part
            ));
        }
    }

    #[test]
    fn post_execution_requires_claim_on_first_active_block() {
        let block = base_time_block_with_transactions(
            COBALT_TIMESTAMP,
            EMPTY_ROOT_HASH,
            vec![TxDeposit::default().seal_slow().into()],
        );
        let parent = SealedHeader::seal_slow(Header {
            timestamp: COBALT_TIMESTAMP - 1,
            ..Default::default()
        });
        let state_updates = HashedPostState::default();
        let error =
            PayloadValidator::<BaseEngineTypes>::validate_block_post_execution_with_hashed_state(
                &cobalt_validator(),
                || &state_updates,
                &block,
                &parent,
                || Ok(Box::new(parent_state(0))),
            )
            .unwrap_err();

        assert_eq!(base_time_metadata_error(&error), Some(&BaseTimeMetadataError::Missing));
    }

    #[test]
    fn post_execution_validates_isthmus_before_base_time() {
        let error = validate_base_time_progression(
            COBALT_TIMESTAMP,
            200,
            COBALT_TIMESTAMP + 1,
            400,
            B256::ZERO,
        )
        .unwrap_err();

        assert!(matches!(
            base_consensus_error(&error),
            Some(BaseConsensusError::L2WithdrawalsRootMismatch { .. })
        ));
    }
}
