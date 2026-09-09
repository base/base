use alloc::{boxed::Box, format, sync::Arc};

use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
use alloy_primitives::{B64, B256};
use base_common_chain_config::BaseChainSpec;
use base_common_chain_config::Upgrades;
use base_common_types_chain::{
    BaseReceipt, BlockHeader as _, EMPTY_OMMER_ROOT_HASH, constants::MAXIMUM_EXTRA_DATA_SIZE,
};
use base_execution_state_types::BlockExecutionResult;
use reth_primitives_traits::{GotExpected, RecoveredBlock, SealedBlock, SealedHeader};

use crate::{
    ConsensusError, HeaderConsensusError, ReceiptRootBloom,
    common_validation::{
        validate_against_parent_eip1559_base_fee, validate_against_parent_hash_number,
        validate_cancun_gas, validate_header_base_fee, validate_header_extra_data,
        validate_header_gas,
    },
    ensure_empty_shanghai_withdrawals, ensure_empty_withdrawals_root,
    ensure_withdrawals_storage_root_is_some, validate_block_post_execution, validation,
};

/// Base consensus implementation.
///
/// Provides basic checks as outlined in the execution specs.
#[derive(Debug, Clone)]
pub struct BaseBeaconConsensus {
    /// Configuration
    chain_spec: Arc<BaseChainSpec>,
    validation: crate::ValidationMode,
    /// Maximum allowed extra data size in bytes
    max_extra_data_size: usize,
}

impl BaseBeaconConsensus {
    /// Create a new instance of [`BaseBeaconConsensus`]
    pub const fn new(chain_spec: Arc<BaseChainSpec>) -> Self {
        Self {
            chain_spec,
            max_extra_data_size: MAXIMUM_EXTRA_DATA_SIZE,
            validation: crate::ValidationMode::Base,
        }
    }

    /// Returns the maximum allowed extra data size.
    pub const fn max_extra_data_size(&self) -> usize {
        self.max_extra_data_size
    }

    /// Sets the maximum allowed extra data size and returns the updated instance.
    pub const fn with_max_extra_data_size(mut self, size: usize) -> Self {
        self.max_extra_data_size = size;
        self
    }
}

impl BaseBeaconConsensus {
    pub fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock,
        result: &BlockExecutionResult<BaseReceipt>,
        receipt_root_bloom: Option<ReceiptRootBloom>,
        _block_access_list_hash: Option<B256>,
    ) -> Result<(), ConsensusError> {
        match &self.validation {
            crate::ValidationMode::Base => {}
            crate::ValidationMode::Skip => return Ok(()),
            #[cfg(any(test, feature = "test-utils"))]
            crate::ValidationMode::Test(consensus) => {
                return consensus.validate_block_post_execution(
                    block,
                    result,
                    receipt_root_bloom,
                    _block_access_list_hash,
                );
            }
            #[cfg(any(test, feature = "test-utils"))]
            crate::ValidationMode::EthereumTest(consensus) => {
                return consensus.validate_block_post_execution(
                    block,
                    result,
                    receipt_root_bloom,
                    _block_access_list_hash,
                );
            }
        }

        validate_block_post_execution(block.header(), &self.chain_spec, result, receipt_root_bloom)
    }
}

impl BaseBeaconConsensus {
    pub fn validate_body_against_header(
        &self,
        body: &base_common_types_chain::BaseBlockBody,
        header: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        match &self.validation {
            crate::ValidationMode::Base => {}
            crate::ValidationMode::Skip => return Ok(()),
            #[cfg(any(test, feature = "test-utils"))]
            crate::ValidationMode::Test(consensus) => {
                return consensus.validate_body_against_header(body, header);
            }
            #[cfg(any(test, feature = "test-utils"))]
            crate::ValidationMode::EthereumTest(consensus) => {
                return consensus.validate_body_against_header(body, header);
            }
        }

        validation::validate_body_against_header_base(&self.chain_spec, body, header.header())?;
        // This is also checked by `validate_block_pre_execution` because callers may invoke either
        // consensus entry point independently.
        validation::validate_base_time_metadata(
            &self.chain_spec,
            header.timestamp(),
            header.number(),
            &body.transactions,
        )
        .map_err(ConsensusError::other)
    }

    pub fn validate_block_pre_execution(&self, block: &SealedBlock) -> Result<(), ConsensusError> {
        match &self.validation {
            crate::ValidationMode::Base => {}
            crate::ValidationMode::Skip => return Ok(()),
            #[cfg(any(test, feature = "test-utils"))]
            crate::ValidationMode::Test(consensus) => {
                return consensus.validate_block_pre_execution(block);
            }
            #[cfg(any(test, feature = "test-utils"))]
            crate::ValidationMode::EthereumTest(consensus) => {
                return consensus.validate_block_pre_execution(block);
            }
        }

        // Check ommers hash
        let ommers_hash = block.body().calculate_ommers_root();
        if block.ommers_hash() != ommers_hash {
            return Err(ConsensusError::BodyOmmersHashDiff(
                GotExpected { got: ommers_hash, expected: block.ommers_hash() }.into(),
            ));
        }

        // Check transaction root
        if let Err(error) = block.ensure_transaction_root_valid() {
            return Err(ConsensusError::BodyTransactionRootDiff(error.into()));
        }

        validation::validate_base_time_metadata(
            &self.chain_spec,
            block.timestamp(),
            block.number(),
            &block.body().transactions,
        )
        .map_err(ConsensusError::other)?;

        // Check empty shanghai-withdrawals
        if self.chain_spec.is_canyon_active_at_timestamp(block.timestamp()) {
            ensure_empty_shanghai_withdrawals(block.body()).map_err(|err| {
                ConsensusError::Other(Arc::from(Box::<dyn core::error::Error + Send + Sync>::from(
                    format!("failed to verify block {}: {err}", block.number()),
                )))
            })?
        } else {
            return Ok(());
        }

        // Blob gas used validation
        // In Jovian, the blob gas used computation has changed. We are moving the blob base fee
        // validation to post-execution since the DA footprint calculation is stateful.
        // Pre-execution we only validate that the blob gas used is present in the header.
        if self.chain_spec.is_jovian_active_at_timestamp(block.timestamp()) {
            block.blob_gas_used().ok_or(ConsensusError::BlobGasUsedMissing)?;
        } else if self.chain_spec.is_ecotone_active_at_timestamp(block.timestamp()) {
            validate_cancun_gas(block)?;
        }

        // Check withdrawals root field in header
        if self.chain_spec.is_isthmus_active_at_timestamp(block.timestamp()) {
            // storage root of withdrawals pre-deploy is verified post-execution
            ensure_withdrawals_storage_root_is_some(block.header()).map_err(|err| {
                ConsensusError::Other(Arc::from(Box::<dyn core::error::Error + Send + Sync>::from(
                    format!("failed to verify block {}: {err}", block.number()),
                )))
            })?
        } else {
            // canyon is active, else would have returned already
            ensure_empty_withdrawals_root(block.header())?
        }

        Ok(())
    }
}

impl BaseBeaconConsensus {
    pub fn validate_header(&self, header: &SealedHeader) -> Result<(), ConsensusError> {
        match &self.validation {
            crate::ValidationMode::Base => {}
            crate::ValidationMode::Skip => return Ok(()),
            #[cfg(any(test, feature = "test-utils"))]
            crate::ValidationMode::Test(consensus) => return consensus.validate_header(header),
            #[cfg(any(test, feature = "test-utils"))]
            crate::ValidationMode::EthereumTest(consensus) => {
                return consensus.validate_header(header);
            }
        }

        let header = header.header();

        if header.nonce() != Some(B64::ZERO) {
            return Err(ConsensusError::TheMergeNonceIsNotZero);
        }

        if header.ommers_hash() != EMPTY_OMMER_ROOT_HASH {
            return Err(ConsensusError::TheMergeOmmerRootIsNotEmpty);
        }

        // Post-merge, the consensus layer is expected to perform checks such that the block
        // timestamp is a function of the slot. This is different from pre-merge, where blocks
        // are only allowed to be in the future (compared to the system's clock) by a certain
        // threshold.
        //
        // Block validation with respect to the parent should ensure that the block timestamp
        // is greater than its parent timestamp.

        // validate header extra data for all networks post merge
        validate_header_extra_data(header, self.max_extra_data_size)?;
        validate_header_gas(header)?;
        validate_header_base_fee(header, &self.chain_spec)?;

        // After Isthmus, every block header must carry `requests_hash = sha256("")`
        // (i.e. `EMPTY_REQUESTS_HASH`) because Base does not support EL-triggered execution
        // requests (EIP-7685). Before Isthmus, the field must be omitted entirely.
        //
        // The Engine API path (`BaseExecutionPayload::into_block_with_sidecar*`) already
        // enforces this rule, but raw header/block import paths (e.g. `import_blocks_from_file`,
        // historical/range sync) only flow through `BaseBeaconConsensus::validate_header`. Without
        // the check here, a malformed post-Isthmus header with an arbitrary `requests_hash`
        // would be accepted and persisted by the import path while being rejected by the
        // Engine path — a block-validity differential that could lead to a chain split.
        //
        // See `docs/specs/pages/upgrades/isthmus/exec-engine.md`.
        if self.chain_spec.is_isthmus_active_at_timestamp(header.timestamp()) {
            match header.requests_hash() {
                None => return Err(ConsensusError::RequestsHashMissing),
                Some(hash) if hash != EMPTY_REQUESTS_HASH => {
                    return Err(ConsensusError::BodyRequestsHashDiff(
                        GotExpected { got: hash, expected: EMPTY_REQUESTS_HASH }.into(),
                    ));
                }
                Some(_) => {}
            }
        } else if header.requests_hash().is_some() {
            return Err(ConsensusError::RequestsHashUnexpected);
        }

        Ok(())
    }

    pub fn validate_header_against_parent(
        &self,
        header: &SealedHeader,
        parent: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        match &self.validation {
            crate::ValidationMode::Base => {}
            crate::ValidationMode::Skip => return Ok(()),
            #[cfg(any(test, feature = "test-utils"))]
            crate::ValidationMode::Test(consensus) => {
                return consensus.validate_header_against_parent(header, parent);
            }
            #[cfg(any(test, feature = "test-utils"))]
            crate::ValidationMode::EthereumTest(consensus) => {
                return consensus.validate_header_against_parent(header, parent);
            }
        }

        validate_against_parent_hash_number(header.header(), parent)?;

        let allows_same_timestamp =
            self.chain_spec.is_cobalt_active_at_timestamp(header.timestamp())
                && self.chain_spec.is_cobalt_active_at_timestamp(parent.timestamp());
        let timestamp_is_invalid = header.timestamp() < parent.timestamp()
            || (header.timestamp() == parent.timestamp() && !allows_same_timestamp);
        if self.chain_spec.is_bedrock_active_at_block(header.number()) && timestamp_is_invalid {
            return Err(ConsensusError::TimestampIsInPast {
                parent_timestamp: parent.timestamp(),
                timestamp: header.timestamp(),
            });
        }

        validate_against_parent_eip1559_base_fee(
            header.header(),
            parent.header(),
            &self.chain_spec,
        )?;

        // Ensure that the blob gas fields for this block are correctly set.
        // On Base, the excess blob gas is always 0 for all blocks after ecotone.
        // The blob gas used and the excess blob gas should both be set after ecotone.
        // After Jovian, the blob gas used contains the current DA footprint.
        if self.chain_spec.is_ecotone_active_at_timestamp(header.timestamp()) {
            let blob_gas_used = header.blob_gas_used().ok_or(ConsensusError::BlobGasUsedMissing)?;

            // Before Jovian and after ecotone, the blob gas used should be 0.
            if !self.chain_spec.is_jovian_active_at_timestamp(header.timestamp())
                && blob_gas_used != 0
            {
                return Err(ConsensusError::BlobGasUsedDiff(GotExpected {
                    got: blob_gas_used,
                    expected: 0,
                }));
            }

            let excess_blob_gas =
                header.excess_blob_gas().ok_or(ConsensusError::ExcessBlobGasMissing)?;
            if excess_blob_gas != 0 {
                return Err(ConsensusError::ExcessBlobGasDiff {
                    diff: GotExpected { got: excess_blob_gas, expected: 0 },
                    parent_excess_blob_gas: parent.excess_blob_gas().unwrap_or(0),
                    parent_blob_gas_used: parent.blob_gas_used().unwrap_or(0),
                });
            }
        }

        Ok(())
    }
}

impl BaseBeaconConsensus {
    /// Disables validation for maintenance operations that replay existing chain data.
    pub fn noop() -> Self {
        Self {
            validation: crate::ValidationMode::Skip,
            ..Self::new(Arc::new(BaseChainSpec::default()))
        }
    }
    pub fn validate_header_range(
        &self,
        headers: &[SealedHeader],
    ) -> Result<(), HeaderConsensusError> {
        if let Some((initial_header, remaining_headers)) = headers.split_first() {
            self.validate_header(initial_header)
                .map_err(|e| HeaderConsensusError(e, initial_header.clone()))?;
            let mut parent = initial_header;
            for child in remaining_headers {
                self.validate_header(child).map_err(|e| HeaderConsensusError(e, child.clone()))?;
                self.validate_header_against_parent(child, parent)
                    .map_err(|e| HeaderConsensusError(e, child.clone()))?;
                parent = child;
            }
        }
        Ok(())
    }
    /// Creates a validator with controllable failures for network tests.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn test() -> Self {
        Self {
            validation: crate::ValidationMode::Test(Arc::new(crate::TestConsensus::default())),
            ..Self::noop()
        }
    }
    /// Creates the shared Ethereum-rule fixture for storage and engine tests.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn ethereum_test(chain_spec: Arc<BaseChainSpec>) -> Self {
        Self {
            validation: crate::ValidationMode::EthereumTest(crate::EthereumTestConsensus::new(
                chain_spec.clone(),
            )),
            ..Self::new(chain_spec)
        }
    }
    /// Controls validation failures in the network fixture.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn set_fail_validation(&self, value: bool) {
        if let crate::ValidationMode::Test(consensus) = &self.validation {
            consensus.set_fail_validation(value);
        }
    }
    /// Controls body validation failures separately in the network fixture.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn set_fail_body_against_header(&self, value: bool) {
        if let crate::ValidationMode::Test(consensus) = &self.validation {
            consensus.set_fail_body_against_header(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_eips::{
        eip1559::BaseFeeParams,
        eip4895::Withdrawals,
        eip7685::{EMPTY_REQUESTS_HASH, Requests},
    };
    use alloy_hardforks::ForkCondition;
    use alloy_primitives::{Address, B256, Bytes, Log, Signature, U256};
    use base_common_chain_config::BaseUpgrade;
    use base_common_chain_config::{BaseChainSpec, BaseChainSpecBuilder};
    use base_common_types_chain::{
        BaseReceipt, BaseTransactionSigned, BaseTypedTransaction, BlockBody, Eip658Value, Header,
        HoloceneExtraData, JovianExtraData, Receipt, TxEip7702, TxReceipt,
    };
    use reth_primitives_traits::{RecoveredBlock, SealedBlock, SealedHeader, proofs};
    use reth_provider::BlockExecutionResult;

    use crate::{BaseBeaconConsensus, ConsensusError};

    fn mock_tx(nonce: u64) -> BaseTransactionSigned {
        let tx = TxEip7702 {
            chain_id: 1u64,
            nonce,
            max_fee_per_gas: 0x28f000fff,
            max_priority_fee_per_gas: 0x28f000fff,
            gas_limit: 10,
            to: Address::default(),
            value: U256::from(3_u64),
            input: Bytes::from(vec![1, 2]),
            access_list: Default::default(),
            authorization_list: Default::default(),
        };

        let signature = Signature::new(U256::default(), U256::default(), true);

        BaseTransactionSigned::new_unhashed(BaseTypedTransaction::Eip7702(tx), signature)
    }

    fn base_mainnet_builder() -> BaseChainSpecBuilder {
        let base_mainnet = BaseChainSpec::mainnet();
        BaseChainSpecBuilder::default()
            .genesis(base_mainnet.genesis.clone())
            .chain(base_mainnet.chain())
    }

    #[test]
    fn activated_parent_validation_allows_same_second_headers() {
        let mut chain_spec = BaseChainSpec::mainnet();
        chain_spec.set_fork(BaseUpgrade::Cobalt, ForkCondition::Timestamp(10));
        let parent_header = Header {
            number: 8,
            timestamp: 10,
            gas_limit: 30_000_000,
            gas_used: 15_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };
        let child_base_fee = chain_spec.next_block_base_fee(&parent_header, 10).unwrap();
        let consensus = BaseBeaconConsensus::new(Arc::new(chain_spec));
        let parent = SealedHeader::seal_slow(parent_header);
        let child = SealedHeader::seal_slow(Header {
            number: 9,
            parent_hash: parent.hash(),
            timestamp: 10,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(child_base_fee),
            ..Default::default()
        });

        consensus.validate_header_against_parent(&child, &parent).unwrap();
    }

    #[test]
    fn activated_parent_validation_rejects_timestamp_in_past() {
        let mut chain_spec = BaseChainSpec::mainnet();
        chain_spec.set_fork(BaseUpgrade::Cobalt, ForkCondition::Timestamp(10));
        let parent_header = Header {
            number: 8,
            timestamp: 11,
            gas_limit: 30_000_000,
            gas_used: 15_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };
        let child_base_fee = chain_spec.next_block_base_fee(&parent_header, 10).unwrap();
        let consensus = BaseBeaconConsensus::new(Arc::new(chain_spec));
        let parent = SealedHeader::seal_slow(parent_header);
        let child = SealedHeader::seal_slow(Header {
            number: 9,
            parent_hash: parent.hash(),
            timestamp: 10,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(child_base_fee),
            ..Default::default()
        });

        assert!(matches!(
            consensus.validate_header_against_parent(&child, &parent),
            Err(ConsensusError::TimestampIsInPast { parent_timestamp: 11, timestamp: 10 })
        ));
    }

    #[test]
    fn test_block_blob_gas_used_validation_isthmus() {
        let chain_spec = base_mainnet_builder().isthmus_activated().build();

        // create a tx
        let transaction = mock_tx(0);

        let beacon_consensus = BaseBeaconConsensus::new(Arc::new(chain_spec));

        let header = Header {
            base_fee_per_gas: Some(1337),
            withdrawals_root: Some(proofs::calculate_withdrawals_root(&[])),
            blob_gas_used: Some(0),
            transactions_root: proofs::calculate_transaction_root(std::slice::from_ref(
                &transaction,
            )),
            timestamp: u64::MAX,
            ..Default::default()
        };
        let body = BlockBody {
            transactions: vec![transaction],
            ommers: vec![],
            withdrawals: Some(Withdrawals::default()),
        };

        let block = SealedBlock::seal_slow(base_common_types_chain::Block { header, body });

        // validate blob, it should pass blob gas used validation
        let pre_execution = beacon_consensus.validate_block_pre_execution(&block);

        assert!(pre_execution.is_ok());
    }

    #[test]
    fn test_block_blob_gas_used_validation_failure_isthmus() {
        let chain_spec = base_mainnet_builder().isthmus_activated().build();

        // create a tx
        let transaction = mock_tx(0);

        let beacon_consensus = BaseBeaconConsensus::new(Arc::new(chain_spec));

        let header = Header {
            base_fee_per_gas: Some(1337),
            withdrawals_root: Some(proofs::calculate_withdrawals_root(&[])),
            blob_gas_used: Some(10),
            transactions_root: proofs::calculate_transaction_root(std::slice::from_ref(
                &transaction,
            )),
            timestamp: u64::MAX,
            ..Default::default()
        };
        let body = BlockBody {
            transactions: vec![transaction],
            ommers: vec![],
            withdrawals: Some(Withdrawals::default()),
        };

        let block = SealedBlock::seal_slow(base_common_types_chain::Block { header, body });

        // validate blob, it should fail blob gas used validation
        let pre_execution = beacon_consensus.validate_block_pre_execution(&block);

        assert!(matches!(
            pre_execution.unwrap_err(),
            ConsensusError::BlobGasUsedDiff(diff) if diff.got == 10 && diff.expected == 0
        ));
    }

    #[test]
    fn test_block_blob_gas_used_validation_jovian() {
        const BLOB_GAS_USED: u64 = 1000;
        const GAS_USED: u64 = 10;

        let chain_spec = base_mainnet_builder().jovian_activated().build();

        // create a tx
        let transaction = mock_tx(0);

        let beacon_consensus = BaseBeaconConsensus::new(Arc::new(chain_spec));

        let receipt = BaseReceipt::Eip7702(Receipt::<Log> {
            status: Eip658Value::success(),
            cumulative_gas_used: GAS_USED,
            logs: vec![],
        });

        let header = Header {
            base_fee_per_gas: Some(1337),
            withdrawals_root: Some(proofs::calculate_withdrawals_root(&[])),
            blob_gas_used: Some(BLOB_GAS_USED),
            transactions_root: proofs::calculate_transaction_root(std::slice::from_ref(
                &transaction,
            )),
            timestamp: u64::MAX,
            gas_used: GAS_USED,
            receipts_root: proofs::calculate_receipt_root(std::slice::from_ref(
                &receipt.with_bloom_ref(),
            )),
            logs_bloom: receipt.bloom(),
            ..Default::default()
        };
        let body = BlockBody {
            transactions: vec![transaction],
            ommers: vec![],
            withdrawals: Some(Withdrawals::default()),
        };

        let block = SealedBlock::seal_slow(base_common_types_chain::Block { header, body });

        let result = BlockExecutionResult::<BaseReceipt> {
            blob_gas_used: BLOB_GAS_USED,
            receipts: vec![receipt],
            requests: Requests::default(),
            gas_used: GAS_USED,
        };

        // validate blob, it should pass blob gas used validation
        let pre_execution = beacon_consensus.validate_block_pre_execution(&block);

        assert!(pre_execution.is_ok());

        let block = RecoveredBlock::new_sealed(block, vec![Address::default()]);

        let post_execution = BaseBeaconConsensus::validate_block_post_execution(
            &beacon_consensus,
            &block,
            &result,
            None,
            None,
        );

        // validate blob, it should pass blob gas used validation
        assert!(post_execution.is_ok());
    }

    #[test]
    fn test_block_blob_gas_used_validation_failure_jovian() {
        const BLOB_GAS_USED: u64 = 1000;
        const GAS_USED: u64 = 10;

        let chain_spec = base_mainnet_builder().jovian_activated().build();

        // create a tx
        let transaction = mock_tx(0);

        let beacon_consensus = BaseBeaconConsensus::new(Arc::new(chain_spec));

        let receipt = BaseReceipt::Eip7702(Receipt::<Log> {
            status: Eip658Value::success(),
            cumulative_gas_used: GAS_USED,
            logs: vec![],
        });

        let header = Header {
            base_fee_per_gas: Some(1337),
            withdrawals_root: Some(proofs::calculate_withdrawals_root(&[])),
            blob_gas_used: Some(BLOB_GAS_USED),
            transactions_root: proofs::calculate_transaction_root(std::slice::from_ref(
                &transaction,
            )),
            gas_used: GAS_USED,
            timestamp: u64::MAX,
            receipts_root: proofs::calculate_receipt_root(std::slice::from_ref(
                &receipt.with_bloom_ref(),
            )),
            logs_bloom: receipt.bloom(),
            ..Default::default()
        };
        let body = BlockBody {
            transactions: vec![transaction],
            ommers: vec![],
            withdrawals: Some(Withdrawals::default()),
        };

        let block = SealedBlock::seal_slow(base_common_types_chain::Block { header, body });

        let result = BlockExecutionResult::<BaseReceipt> {
            blob_gas_used: BLOB_GAS_USED + 1,
            receipts: vec![receipt],
            requests: Requests::default(),
            gas_used: GAS_USED,
        };

        // validate blob, it should pass blob gas used validation
        let pre_execution = beacon_consensus.validate_block_pre_execution(&block);

        assert!(pre_execution.is_ok());

        let block = RecoveredBlock::new_sealed(block, vec![Address::default()]);

        let post_execution = BaseBeaconConsensus::validate_block_post_execution(
            &beacon_consensus,
            &block,
            &result,
            None,
            None,
        );

        // validate blob, it should fail blob gas used validation post execution.
        assert!(matches!(
            post_execution.unwrap_err(),
            ConsensusError::BlobGasUsedDiff(diff)
                if diff.got == BLOB_GAS_USED + 1 && diff.expected == BLOB_GAS_USED
        ));
    }

    #[test]
    fn test_header_min_base_fee_validation() {
        const MIN_BASE_FEE: u64 = 1000;

        let chain_spec = base_mainnet_builder().jovian_activated().build();

        // create a tx
        let transaction = mock_tx(0);

        let beacon_consensus = BaseBeaconConsensus::new(Arc::new(chain_spec));

        let receipt = BaseReceipt::Eip7702(Receipt::<Log> {
            status: Eip658Value::success(),
            cumulative_gas_used: 0,
            logs: vec![],
        });

        let parent = Header {
            number: 0,
            base_fee_per_gas: Some(MIN_BASE_FEE / 10),
            withdrawals_root: Some(proofs::calculate_withdrawals_root(&[])),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            transactions_root: proofs::calculate_transaction_root(std::slice::from_ref(
                &transaction,
            )),
            gas_used: 0,
            timestamp: u64::MAX - 1,
            receipts_root: proofs::calculate_receipt_root(std::slice::from_ref(
                &receipt.with_bloom_ref(),
            )),
            logs_bloom: receipt.bloom(),
            extra_data: JovianExtraData::encode(
                Default::default(),
                BaseFeeParams::optimism(),
                MIN_BASE_FEE,
            )
            .unwrap(),
            ..Default::default()
        };
        let parent = SealedHeader::seal_slow(parent);

        let header = Header {
            number: 1,
            base_fee_per_gas: Some(MIN_BASE_FEE),
            withdrawals_root: Some(proofs::calculate_withdrawals_root(&[])),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            transactions_root: proofs::calculate_transaction_root(std::slice::from_ref(
                &transaction,
            )),
            gas_used: 0,
            timestamp: u64::MAX,
            receipts_root: proofs::calculate_receipt_root(std::slice::from_ref(
                &receipt.with_bloom_ref(),
            )),
            logs_bloom: receipt.bloom(),
            parent_hash: parent.hash(),
            ..Default::default()
        };
        let header = SealedHeader::seal_slow(header);

        let result = beacon_consensus.validate_header_against_parent(&header, &parent);

        assert!(result.is_ok());
    }

    #[test]
    fn test_header_min_base_fee_validation_failure() {
        const MIN_BASE_FEE: u64 = 1000;

        let chain_spec = base_mainnet_builder().jovian_activated().build();

        // create a tx
        let transaction = mock_tx(0);

        let beacon_consensus = BaseBeaconConsensus::new(Arc::new(chain_spec));

        let receipt = BaseReceipt::Eip7702(Receipt::<Log> {
            status: Eip658Value::success(),
            cumulative_gas_used: 0,
            logs: vec![],
        });

        let parent = Header {
            number: 0,
            base_fee_per_gas: Some(MIN_BASE_FEE / 10),
            withdrawals_root: Some(proofs::calculate_withdrawals_root(&[])),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            transactions_root: proofs::calculate_transaction_root(std::slice::from_ref(
                &transaction,
            )),
            gas_used: 0,
            timestamp: u64::MAX - 1,
            receipts_root: proofs::calculate_receipt_root(std::slice::from_ref(
                &receipt.with_bloom_ref(),
            )),
            logs_bloom: receipt.bloom(),
            extra_data: JovianExtraData::encode(
                Default::default(),
                BaseFeeParams::optimism(),
                MIN_BASE_FEE,
            )
            .unwrap(),
            ..Default::default()
        };
        let parent = SealedHeader::seal_slow(parent);

        let header = Header {
            number: 1,
            base_fee_per_gas: Some(MIN_BASE_FEE - 1),
            withdrawals_root: Some(proofs::calculate_withdrawals_root(&[])),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            transactions_root: proofs::calculate_transaction_root(std::slice::from_ref(
                &transaction,
            )),
            gas_used: 0,
            timestamp: u64::MAX,
            receipts_root: proofs::calculate_receipt_root(std::slice::from_ref(
                &receipt.with_bloom_ref(),
            )),
            logs_bloom: receipt.bloom(),
            parent_hash: parent.hash(),
            ..Default::default()
        };
        let header = SealedHeader::seal_slow(header);

        let result = beacon_consensus.validate_header_against_parent(&header, &parent);

        assert!(matches!(
            result.unwrap_err(),
            ConsensusError::BaseFeeDiff(diff)
                if diff.got == MIN_BASE_FEE - 1 && diff.expected == MIN_BASE_FEE
        ));
    }

    #[test]
    fn test_header_da_footprint_validation() {
        const MIN_BASE_FEE: u64 = 100_000;
        const DA_FOOTPRINT: u64 = GAS_LIMIT - 1;
        const GAS_LIMIT: u64 = 100_000_000;

        let chain_spec = base_mainnet_builder().jovian_activated().build();

        // create a tx
        let transaction = mock_tx(0);

        let beacon_consensus = BaseBeaconConsensus::new(Arc::new(chain_spec));

        let receipt = BaseReceipt::Eip7702(Receipt::<Log> {
            status: Eip658Value::success(),
            cumulative_gas_used: 0,
            logs: vec![],
        });

        let parent = Header {
            number: 0,
            base_fee_per_gas: Some(MIN_BASE_FEE),
            withdrawals_root: Some(proofs::calculate_withdrawals_root(&[])),
            blob_gas_used: Some(DA_FOOTPRINT),
            excess_blob_gas: Some(0),
            transactions_root: proofs::calculate_transaction_root(std::slice::from_ref(
                &transaction,
            )),
            gas_used: 0,
            timestamp: u64::MAX - 1,
            receipts_root: proofs::calculate_receipt_root(std::slice::from_ref(
                &receipt.with_bloom_ref(),
            )),
            logs_bloom: receipt.bloom(),
            extra_data: JovianExtraData::encode(
                Default::default(),
                BaseFeeParams::optimism(),
                MIN_BASE_FEE,
            )
            .unwrap(),
            gas_limit: GAS_LIMIT,
            ..Default::default()
        };
        let parent = SealedHeader::seal_slow(parent);

        let header = Header {
            number: 1,
            base_fee_per_gas: Some(MIN_BASE_FEE + MIN_BASE_FEE / 10),
            withdrawals_root: Some(proofs::calculate_withdrawals_root(&[])),
            blob_gas_used: Some(DA_FOOTPRINT),
            excess_blob_gas: Some(0),
            transactions_root: proofs::calculate_transaction_root(std::slice::from_ref(
                &transaction,
            )),
            gas_used: 0,
            timestamp: u64::MAX,
            receipts_root: proofs::calculate_receipt_root(std::slice::from_ref(
                &receipt.with_bloom_ref(),
            )),
            logs_bloom: receipt.bloom(),
            parent_hash: parent.hash(),
            ..Default::default()
        };
        let header = SealedHeader::seal_slow(header);

        let result = beacon_consensus.validate_header_against_parent(&header, &parent);

        assert!(result.is_ok());
    }

    #[test]
    fn test_header_isthmus_validation() {
        const MIN_BASE_FEE: u64 = 100_000;
        const DA_FOOTPRINT: u64 = GAS_LIMIT - 1;
        const GAS_LIMIT: u64 = 100_000_000;

        let chain_spec = base_mainnet_builder().isthmus_activated().build();

        // create a tx
        let transaction = mock_tx(0);

        let beacon_consensus = BaseBeaconConsensus::new(Arc::new(chain_spec));

        let receipt = BaseReceipt::Eip7702(Receipt::<Log> {
            status: Eip658Value::success(),
            cumulative_gas_used: 0,
            logs: vec![],
        });

        let parent = Header {
            number: 0,
            base_fee_per_gas: Some(MIN_BASE_FEE),
            withdrawals_root: Some(proofs::calculate_withdrawals_root(&[])),
            blob_gas_used: Some(DA_FOOTPRINT),
            excess_blob_gas: Some(0),
            transactions_root: proofs::calculate_transaction_root(std::slice::from_ref(
                &transaction,
            )),
            gas_used: 0,
            timestamp: u64::MAX - 1,
            receipts_root: proofs::calculate_receipt_root(std::slice::from_ref(
                &receipt.with_bloom_ref(),
            )),
            logs_bloom: receipt.bloom(),
            extra_data: HoloceneExtraData::encode(Default::default(), BaseFeeParams::optimism())
                .unwrap(),
            gas_limit: GAS_LIMIT,
            ..Default::default()
        };
        let parent = SealedHeader::seal_slow(parent);

        let header = Header {
            number: 1,
            base_fee_per_gas: Some(MIN_BASE_FEE - 2 * MIN_BASE_FEE / 100),
            withdrawals_root: Some(proofs::calculate_withdrawals_root(&[])),
            blob_gas_used: Some(DA_FOOTPRINT),
            excess_blob_gas: Some(0),
            transactions_root: proofs::calculate_transaction_root(std::slice::from_ref(
                &transaction,
            )),
            gas_used: 0,
            timestamp: u64::MAX,
            receipts_root: proofs::calculate_receipt_root(std::slice::from_ref(
                &receipt.with_bloom_ref(),
            )),
            logs_bloom: receipt.bloom(),
            parent_hash: parent.hash(),
            ..Default::default()
        };
        let header = SealedHeader::seal_slow(header);

        let result = beacon_consensus.validate_header_against_parent(&header, &parent);

        assert!(matches!(
            result.unwrap_err(),
            ConsensusError::BlobGasUsedDiff(diff)
                if diff.got == DA_FOOTPRINT && diff.expected == 0
        ));
    }

    /// Builds a minimal post-Isthmus header that satisfies all of `validate_header`'s checks
    /// other than the `requests_hash` rule under test.
    fn isthmus_header_with_requests_hash(requests_hash: Option<B256>) -> SealedHeader {
        SealedHeader::seal_slow(Header {
            base_fee_per_gas: Some(1337),
            withdrawals_root: Some(B256::ZERO),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            timestamp: u64::MAX,
            requests_hash,
            ..Default::default()
        })
    }

    #[test]
    fn test_isthmus_validate_header_accepts_empty_requests_hash() {
        let chain_spec = base_mainnet_builder().isthmus_activated().build();
        let beacon_consensus = BaseBeaconConsensus::new(Arc::new(chain_spec));

        let header = isthmus_header_with_requests_hash(Some(EMPTY_REQUESTS_HASH));
        beacon_consensus
            .validate_header(&header)
            .expect("post-Isthmus header with EMPTY_REQUESTS_HASH must pass validate_header");
    }

    /// Regression: post-Isthmus headers carrying a non-empty `requests_hash` (Base does not
    /// support EL execution requests) must be rejected by the raw header import path, matching
    /// the Engine API's `BasePayloadError::NonEmptyELRequests` enforcement. Without the check,
    /// `import_blocks_from_file` and range-sync would persist a malformed header that the
    /// Engine path rejects, producing a block-validity differential inside the EL.
    #[test]
    fn test_isthmus_validate_header_rejects_non_empty_requests_hash() {
        let chain_spec = base_mainnet_builder().isthmus_activated().build();
        let beacon_consensus = BaseBeaconConsensus::new(Arc::new(chain_spec));

        let bogus = B256::repeat_byte(0x11);
        let header = isthmus_header_with_requests_hash(Some(bogus));

        assert!(matches!(
            beacon_consensus.validate_header(&header).unwrap_err(),
            ConsensusError::BodyRequestsHashDiff(diff)
                if diff.got == bogus && diff.expected == EMPTY_REQUESTS_HASH
        ));
    }

    #[test]
    fn test_isthmus_validate_header_rejects_missing_requests_hash() {
        let chain_spec = base_mainnet_builder().isthmus_activated().build();
        let beacon_consensus = BaseBeaconConsensus::new(Arc::new(chain_spec));

        let header = isthmus_header_with_requests_hash(None);

        assert!(matches!(
            beacon_consensus.validate_header(&header).unwrap_err(),
            ConsensusError::RequestsHashMissing,
        ));
    }

    #[test]
    fn test_pre_isthmus_validate_header_rejects_unexpected_requests_hash() {
        // Holocene activates strictly before Isthmus; this exercises the pre-Isthmus branch.
        let chain_spec = base_mainnet_builder().holocene_activated().build();
        let beacon_consensus = BaseBeaconConsensus::new(Arc::new(chain_spec));

        let header = SealedHeader::seal_slow(Header {
            base_fee_per_gas: Some(1337),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            timestamp: u64::MAX,
            requests_hash: Some(EMPTY_REQUESTS_HASH),
            ..Default::default()
        });

        assert!(matches!(
            beacon_consensus.validate_header(&header).unwrap_err(),
            ConsensusError::RequestsHashUnexpected,
        ));
    }
}
