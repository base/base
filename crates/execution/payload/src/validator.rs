//! Validates execution payloads against Base consensus rules.

use alloc::sync::Arc;

use alloy_hardforks::EthereumHardforks;
use base_common_chain_config::BaseChainSpec;
use base_common_types_chain::{BlockHeader, SealedBlock};
use base_common_types_payload::{BasePayloadError, ExecutionData, PayloadError};
use derive_more::Constructor;

/// Execution payload validator.
#[derive(Clone, Debug, Constructor)]
pub struct BaseExecutionPayloadValidator {
    /// The Base chain configuration used for payload validation.
    pub chain_spec: Arc<BaseChainSpec>,
}

impl BaseExecutionPayloadValidator {
    /// Decodes a payload and checks its hash, withdrawals, and upgrade-dependent sidecar fields.
    /// Checks retain Engine API ordering so malformed payloads report the expected first error.
    pub fn ensure_well_formed_payload(
        &self,
        payload: ExecutionData,
    ) -> Result<SealedBlock, BasePayloadError> {
        // BAL bytes are carried through `ExecutionData` for Amsterdam-aware paths, but payload
        // well-formedness here is defined by the encoded execution payload + sidecar alone.
        let ExecutionData { payload, sidecar, .. } = payload;

        let expected_hash = payload.block_hash();

        // First parse the block
        let sealed_block = payload.try_into_block_with_sidecar(&sidecar)?.seal_slow();

        // Ensure the hash included in the payload matches the block hash
        if expected_hash != sealed_block.hash() {
            Err(PayloadError::BlockHash {
                execution: sealed_block.hash(),
                consensus: expected_hash,
            })?;
        }

        let chain_spec = &self.chain_spec;
        if chain_spec.is_shanghai_active_at_timestamp(sealed_block.timestamp) {
            if sealed_block.body().withdrawals.is_none() {
                return Err(PayloadError::PostShanghaiBlockWithoutWithdrawals.into());
            }
        } else if sealed_block.body().withdrawals.is_some() {
            return Err(PayloadError::PreShanghaiBlockWithWithdrawals.into());
        }

        let is_ecotone_active = chain_spec.is_cancun_active_at_timestamp(sealed_block.timestamp);
        let cancun_sidecar_fields = sidecar.ecotone();
        if is_ecotone_active {
            if sealed_block.blob_gas_used().is_none() {
                // cancun active but blob gas used not present
                return Err(PayloadError::PostCancunBlockWithoutBlobGasUsed.into());
            }
            if sealed_block.excess_blob_gas().is_none() {
                // cancun active but excess blob gas not present
                return Err(PayloadError::PostCancunBlockWithoutExcessBlobGas.into());
            }
            if cancun_sidecar_fields.is_none() {
                // cancun active but cancun fields not present
                return Err(PayloadError::PostCancunWithoutCancunFields.into());
            }
        } else {
            if sealed_block.blob_gas_used().is_some() {
                // cancun not active but blob gas used present
                return Err(PayloadError::PreCancunBlockWithBlobGasUsed.into());
            }
            if sealed_block.excess_blob_gas().is_some() {
                // cancun not active but excess blob gas present
                return Err(PayloadError::PreCancunBlockWithExcessBlobGas.into());
            }
            if cancun_sidecar_fields.is_some() {
                // cancun not active but cancun fields present
                return Err(PayloadError::PreCancunWithCancunFields.into());
            }
        }

        if !chain_spec.is_prague_active_at_timestamp(sealed_block.timestamp) {
            if sidecar.isthmus().is_some() {
                return Err(PayloadError::PrePragueBlockRequests.into());
            }
            if sealed_block.body().has_eip7702_transactions() {
                return Err(PayloadError::PrePragueBlockWithEip7702Transactions.into());
            }
        }

        Ok(sealed_block)
    }
}
