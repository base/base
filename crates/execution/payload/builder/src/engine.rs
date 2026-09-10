use std::sync::Arc;

use alloy_primitives::{B256, keccak256};
use base_common_chain_config::BaseChainSpec;
use base_common_chain_config::Upgrades;
use base_common_types_chain::{BlockHeader, Predeploys};
use base_common_types_chain::{RecoveredBlock, SealedBlock, SealedHeader};
use base_common_types_payload::ExecutionData;
use base_consensus_batch_types::{BaseTimeMetadataError, BaseTimeUpdateTx};
use base_execution_evm_blocks::InsertBlockErrorKind;
use base_execution_evm_blocks::{
    BaseConsensusError, ConsensusError, verify_withdrawals_root_prehashed,
};
use base_execution_evm_runtime::BaseTime;
use base_execution_payload_types::{
    BasePayloadBuilderAttributes, InvalidPayloadAttributesError, NewPayloadError,
};
use base_execution_state_api::{ProviderResult, StateProvider, StateProviderBox};
use base_execution_state_types::HashedPostState;

use crate::BaseExecutionPayloadValidator;

/// Validates Base execution inputs.
#[derive(Debug)]
pub struct BaseEngineValidator {
    inner: BaseExecutionPayloadValidator,
    hashed_addr_l2tol1_msg_passer: B256,
}

impl BaseEngineValidator {
    /// Instantiates a new validator.
    pub fn new(chain_spec: Arc<BaseChainSpec>) -> Self {
        let hashed_addr_l2tol1_msg_passer = keccak256(Predeploys::L2_TO_L1_MESSAGE_PASSER);
        Self {
            inner: BaseExecutionPayloadValidator::new(chain_spec),
            hashed_addr_l2tol1_msg_passer,
        }
    }
}

impl Clone for BaseEngineValidator {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            hashed_addr_l2tol1_msg_passer: self.hashed_addr_l2tol1_msg_passer,
        }
    }
}

impl BaseEngineValidator {
    /// Returns the chain spec used by the validator.
    #[inline]
    pub fn chain_spec(&self) -> &BaseChainSpec {
        &self.inner.chain_spec
    }

    /// Validates native build attributes using the active Base upgrade schedule.
    pub fn validate_attributes(
        &self,
        attributes: &BasePayloadBuilderAttributes,
    ) -> Result<(), InvalidPayloadAttributesError> {
        let timestamp = attributes.payload_attributes.timestamp;
        let fields = &attributes.payload_attributes;
        if fields.has_withdrawals != self.chain_spec().is_canyon_active_at_timestamp(timestamp) {
            return Err(InvalidPayloadAttributesError::InvalidParams(
                "withdrawals presence does not match Canyon activation".into(),
            ));
        }
        if fields.parent_beacon_block_root.is_some()
            != self.chain_spec().is_ecotone_active_at_timestamp(timestamp)
        {
            return Err(InvalidPayloadAttributesError::InvalidParams(
                "parent beacon block root presence does not match Ecotone activation".into(),
            ));
        }
        self.validate_base_attributes(attributes)
    }

    /// Checks Base gas and fee parameters for native builds.
    pub fn validate_base_attributes(
        &self,
        attributes: &BasePayloadBuilderAttributes,
    ) -> Result<(), InvalidPayloadAttributesError> {
        if attributes.gas_limit.is_none() {
            return Err(InvalidPayloadAttributesError::InvalidParams(
                "MissingGasLimitInPayloadAttributes".to_string().into(),
            ));
        }

        if self
            .chain_spec()
            .is_holocene_active_at_timestamp(attributes.payload_attributes.timestamp)
        {
            let (elasticity, denominator) =
                attributes.decode_eip_1559_params().ok_or_else(|| {
                    InvalidPayloadAttributesError::InvalidParams(
                        "MissingEip1559ParamsInPayloadAttributes".to_string().into(),
                    )
                })?;

            if elasticity != 0 && denominator == 0 {
                return Err(InvalidPayloadAttributesError::InvalidParams(
                    "Eip1559ParamsDenominatorZero".to_string().into(),
                ));
            } else if denominator != 0 && elasticity == 0 {
                return Err(InvalidPayloadAttributesError::InvalidParams(
                    "Eip1559ParamsElasticityZero".to_string().into(),
                ));
            }
        }

        if self.chain_spec().is_jovian_active_at_timestamp(attributes.payload_attributes.timestamp)
        {
            if attributes.min_base_fee.is_none() {
                return Err(InvalidPayloadAttributesError::InvalidParams(
                    "MissingMinBaseFeeInPayloadAttributes".to_string().into(),
                ));
            }
        } else if attributes.min_base_fee.is_some() {
            return Err(InvalidPayloadAttributesError::InvalidParams(
                "MinBaseFeeNotAllowedBeforeJovian".to_string().into(),
            ));
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
        verify_withdrawals_root_prehashed(predeploy_storage_updates, parent_state, header)
            .map_err(ConsensusError::other)
    }
}

impl BaseEngineValidator {
    /// Converts and validates a payload, recovering its transaction senders.
    pub fn ensure_well_formed_payload(
        &self,
        payload: ExecutionData,
    ) -> Result<RecoveredBlock, NewPayloadError> {
        self.convert_payload_to_block(payload)?
            .try_recover()
            .map_err(|e| NewPayloadError::Other(e.into()))
    }

    /// Checks Base post-execution rules against the parent state and hashed updates.
    pub fn validate_block_post_execution_with_hashed_state<'a>(
        &self,
        state_updates: impl FnOnce() -> &'a HashedPostState,
        block: &RecoveredBlock,
        parent_header: &SealedHeader,
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

    /// Converts and validates a payload without recovering transaction senders.
    pub fn convert_payload_to_block(
        &self,
        payload: ExecutionData,
    ) -> Result<SealedBlock, NewPayloadError> {
        self.inner.ensure_well_formed_payload(payload).map_err(NewPayloadError::other)
    }

    /// Checks build attributes against the parent header and active Base upgrades.
    pub fn validate_payload_attributes_against_header(
        &self,
        attributes: &BasePayloadBuilderAttributes,
        header: &base_common_types_chain::Header,
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
            .transactions
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

#[cfg(test)]
mod tests {
    use alloy_hardforks::ForkCondition;
    use alloy_primitives::{Address, B64, B256, U256, b64};
    use base_common_chain_config::{BaseChainSpec, BaseChainSpecBuilder};
    use base_common_chain_config::{BaseUpgrade, ChainConfig};
    use base_common_types_chain::WithEncoded;
    use base_common_types_chain::{
        BaseBlock, BaseTxEnvelope, BlockBody, EMPTY_ROOT_HASH, Header, Sealable, TxDeposit,
    };
    use base_common_types_payload::{BasePayloadAttributes, PayloadAttributes};
    use base_execution_evm_blocks::BaseConsensusError;
    use base_execution_payload_types::BasePayloadBuilderAttributes;
    use base_execution_state_provider::{
        NoopProvider, test_utils::ExtendedAccount, test_utils::MockEthProvider,
    };

    use super::*;

    const COBALT_TIMESTAMP: u64 = 1_800_000_001;

    fn validator_with_chain_spec(chain_spec: BaseChainSpec) -> BaseEngineValidator {
        BaseEngineValidator::new(Arc::new(chain_spec))
    }

    fn validator() -> BaseEngineValidator {
        validator_with_chain_spec(BaseChainSpec::sepolia())
    }

    fn cobalt_validator() -> BaseEngineValidator {
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
                InvalidPayloadAttributesError::InvalidParams(inner) => {
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
    ) -> BasePayloadBuilderAttributes {
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

    fn cobalt_attributes(timestamp: u64) -> BasePayloadBuilderAttributes {
        get_attributes(Some(b64!("0000000000000000")), Some(1), timestamp)
    }

    fn add_base_time_transaction(attributes: &mut BasePayloadBuilderAttributes, millis_part: u16) {
        let metadata = BaseTimeUpdateTx::new(millis_part).unwrap().into_deposit_tx(9);
        attributes.transactions = vec![
            WithEncoded::from_2718_encodable(TxDeposit::default().seal_slow().into()),
            WithEncoded::from_2718_encodable(metadata.into()),
        ];
    }

    #[test]
    fn native_attributes_enforce_fork_fields_and_base_parameters() {
        let validator = validator();
        let mut attributes = get_attributes(Some(B64::ZERO), None, 1732633200);
        assert!(validator.validate_attributes(&attributes).is_ok());
        attributes.payload_attributes.has_withdrawals = false;
        assert!(validator.validate_attributes(&attributes).is_err());
        attributes.payload_attributes.has_withdrawals = true;
        attributes.payload_attributes.parent_beacon_block_root = None;
        assert!(validator.validate_attributes(&attributes).is_err());
        attributes.payload_attributes.parent_beacon_block_root = Some(B256::ZERO);
        attributes.gas_limit = None;
        assert_invalid_params_error!(
            validator.validate_attributes(&attributes),
            "MissingGasLimitInPayloadAttributes"
        );
        attributes.gas_limit = Some(1000);
        attributes.eip_1559_params = None;
        assert_invalid_params_error!(
            validator.validate_attributes(&attributes),
            "MissingEip1559ParamsInPayloadAttributes"
        );
    }

    #[test]
    fn test_well_formed_attributes_pre_holocene() {
        let validator = validator();
        let attributes = get_attributes(None, None, 1732633199);

        let result = validator.validate_attributes(&attributes);
        assert!(result.is_ok());
    }

    #[test]
    fn test_well_formed_attributes_holocene_no_eip1559_params() {
        let validator = validator();
        let attributes = get_attributes(None, None, 1732633200);

        let result = validator.validate_attributes(&attributes);
        assert_invalid_params_error!(result, "MissingEip1559ParamsInPayloadAttributes");
    }

    #[test]
    fn test_well_formed_attributes_holocene_eip1559_params_zero_denominator() {
        let validator = validator();
        let attributes = get_attributes(Some(b64!("0000000000000008")), None, 1732633200);

        let result = validator.validate_attributes(&attributes);
        assert_invalid_params_error!(result, "Eip1559ParamsDenominatorZero");
    }

    #[test]
    fn test_well_formed_attributes_holocene_eip1559_params_zero_elasticity() {
        let validator = validator();
        let attributes = get_attributes(Some(b64!("0000000800000000")), None, 1732633200);

        let result = validator.validate_attributes(&attributes);
        assert_invalid_params_error!(result, "Eip1559ParamsElasticityZero");
    }

    #[test]
    fn test_well_formed_attributes_holocene_valid() {
        let validator = validator();
        let attributes = get_attributes(Some(b64!("0000000800000008")), None, 1732633200);

        let result = validator.validate_attributes(&attributes);
        assert!(result.is_ok());
    }

    #[test]
    fn test_well_formed_attributes_holocene_valid_all_zero() {
        let validator = validator();
        let attributes = get_attributes(Some(b64!("0000000000000000")), None, 1732633200);

        let result = validator.validate_attributes(&attributes);
        assert!(result.is_ok());
    }

    #[test]
    fn test_well_formed_attributes_jovian_valid() {
        let validator = validator();
        let attributes = get_attributes(
            Some(b64!("0000000000000000")),
            Some(1),
            ChainConfig::sepolia().upgrades[base_common_chain_config::BaseUpgrade::Jovian]
                .as_timestamp()
                .unwrap_or_default(),
        );

        let result = validator.validate_attributes(&attributes);
        assert!(result.is_ok());
    }

    /// After Jovian (and holocene), eip1559 params must be Some
    #[test]
    fn test_malformed_attributes_jovian_with_eip_1559_params_none() {
        let validator = validator();
        let attributes = get_attributes(
            None,
            Some(1),
            ChainConfig::sepolia().upgrades[base_common_chain_config::BaseUpgrade::Jovian]
                .as_timestamp()
                .unwrap_or_default(),
        );

        let result = validator.validate_attributes(&attributes);
        assert_invalid_params_error!(result, "MissingEip1559ParamsInPayloadAttributes");
    }

    /// Before Jovian, min base fee must be None
    #[test]
    fn test_malformed_attributes_pre_jovian_with_min_base_fee() {
        let validator = validator();
        let attributes = get_attributes(Some(b64!("0000000000000000")), Some(1), 1732633200);

        let result = validator.validate_attributes(&attributes);
        assert_invalid_params_error!(result, "MinBaseFeeNotAllowedBeforeJovian");
    }

    /// After Jovian, min base fee must be Some
    #[test]
    fn test_malformed_attributes_post_jovian_with_min_base_fee_none() {
        let validator = validator();
        let attributes = get_attributes(
            Some(b64!("0000000000000000")),
            None,
            ChainConfig::sepolia().upgrades[base_common_chain_config::BaseUpgrade::Jovian]
                .as_timestamp()
                .unwrap_or_default(),
        );

        let result = validator.validate_attributes(&attributes);
        assert_invalid_params_error!(result, "MissingMinBaseFeeInPayloadAttributes");
    }

    fn validate_against_parent(
        validator: &BaseEngineValidator,
        timestamp: u64,
        timestamp_millis_part: u16,
        parent_timestamp: u64,
    ) -> Result<(), InvalidPayloadAttributesError> {
        let mut attributes = cobalt_attributes(timestamp);
        add_base_time_transaction(&mut attributes, timestamp_millis_part);
        let header = Header { number: 8, timestamp: parent_timestamp, ..Default::default() };

        BaseEngineValidator::validate_payload_attributes_against_header(
            validator,
            &attributes,
            &header,
        )
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

        let result = BaseEngineValidator::validate_payload_attributes_against_header(
            &validator,
            &attributes,
            &header,
        );

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

        let result = BaseEngineValidator::validate_payload_attributes_against_header(
            &validator,
            &attributes,
            &header,
        );

        assert_eq!(
            result.unwrap_err().to_string(),
            "Invalid params: invalid BaseTime metadata source hash"
        );
    }

    fn post_execution_block(withdrawals_root: B256) -> RecoveredBlock {
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

    fn base_time_block(timestamp: u64, millis_part: u16, withdrawals_root: B256) -> RecoveredBlock {
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
    ) -> RecoveredBlock {
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

    fn parent_state(millis_part: u16) -> MockEthProvider {
        let provider = MockEthProvider::new();
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

        BaseEngineValidator::validate_block_post_execution_with_hashed_state(
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
        let error = BaseEngineValidator::validate_block_post_execution_with_hashed_state(
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
            let error = BaseEngineValidator::validate_block_post_execution_with_hashed_state(
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

        BaseEngineValidator::validate_block_post_execution_with_hashed_state(
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
            let error = BaseEngineValidator::validate_block_post_execution_with_hashed_state(
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
        let error = BaseEngineValidator::validate_block_post_execution_with_hashed_state(
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
