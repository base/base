//! Validates execution payload wrt Base consensus rules

use alloc::sync::Arc;

use alloy_consensus::{Block, BlockHeader};
use alloy_eips::eip7928::bal::DecodedBal;
use alloy_rpc_types_engine::PayloadError;
use base_common_chains::Upgrades;
use base_common_rpc_types_engine::{BasePayloadError, ExecutionData};
use derive_more::{Constructor, Deref};
use reth_payload_validator::{amsterdam, cancun, prague, shanghai};
use reth_primitives_traits::{Block as _, SealedBlock, SignedTransaction};

/// Execution payload validator.
#[derive(Clone, Debug, Deref, Constructor)]
pub struct BaseExecutionPayloadValidator<ChainSpec> {
    /// Chain spec to validate against.
    #[deref]
    inner: Arc<ChainSpec>,
}

impl<ChainSpec> BaseExecutionPayloadValidator<ChainSpec>
where
    ChainSpec: Upgrades,
{
    /// Returns reference to chain spec.
    pub fn chain_spec(&self) -> &ChainSpec {
        &self.inner
    }

    /// Ensures that the given payload does not violate any consensus rules that concern the block's
    /// layout.
    ///
    /// See also [`ensure_well_formed_payload`].
    pub fn ensure_well_formed_payload<T: SignedTransaction>(
        &self,
        payload: ExecutionData,
    ) -> Result<SealedBlock<Block<T>>, BasePayloadError> {
        ensure_well_formed_payload(self.chain_spec(), payload)
    }
}

/// Ensures that the given payload does not violate any consensus rules that concern the block's
/// layout, like:
///    - missing or invalid base fee
///    - invalid transactions
///    - incorrect hash
///    - block contains blob transactions or blob versioned hashes
///    - block contains l1 withdrawals
///
/// The checks are done in the order that conforms with the engine-API specification.
///
/// This is intended to be invoked after receiving the payload from the CLI.
/// The additional fields, starting with [`MaybeCancunPayloadFields`](alloy_rpc_types_engine::MaybeCancunPayloadFields), are not part of the payload, but are additional fields starting in the `engine_newPayloadV3` RPC call, See also <https://specs.base.org/protocol/execution#engine_newpayloadv3>
///
/// If the cancun fields are provided this also validates that the versioned hashes in the block
/// are empty as well as those passed in the sidecar. If the payload fields are not provided.
///
/// Validation according to specs <https://specs.base.org/protocol/execution#engine-api>.
pub fn ensure_well_formed_payload<ChainSpec, T>(
    chain_spec: ChainSpec,
    payload: ExecutionData,
) -> Result<SealedBlock<Block<T>>, BasePayloadError>
where
    ChainSpec: Upgrades,
    T: SignedTransaction,
{
    // BAL bytes are carried through `ExecutionData` for Amsterdam-aware paths, but payload
    // well-formedness here is defined by the encoded execution payload + sidecar alone.
    let ExecutionData { payload, sidecar, block_access_list } = payload;

    let expected_hash = payload.block_hash();

    // First parse the block
    let sealed_block = payload.try_into_block_with_sidecar(&sidecar)?.seal_slow();

    // Ensure the hash included in the payload matches the block hash
    if expected_hash != sealed_block.hash() {
        Err(PayloadError::BlockHash { execution: sealed_block.hash(), consensus: expected_hash })?;
    }

    shanghai::ensure_well_formed_fields(
        sealed_block.body(),
        chain_spec.is_shanghai_active_at_timestamp(sealed_block.timestamp),
    )?;

    cancun::ensure_well_formed_header_and_sidecar_fields(
        &sealed_block,
        sidecar.ecotone(),
        chain_spec.is_cancun_active_at_timestamp(sealed_block.timestamp),
    )?;

    prague::ensure_well_formed_fields(
        sealed_block.body(),
        sidecar.isthmus(),
        chain_spec.is_prague_active_at_timestamp(sealed_block.timestamp),
    )?;

    let is_amsterdam = chain_spec.is_amsterdam_active_at_timestamp(sealed_block.timestamp);
    amsterdam::ensure_well_formed_fields(&sealed_block, is_amsterdam)?;

    // EIP-7928: an Amsterdam block must ship the access list its header commits to, and the
    // shipped list must hash to that commitment. Without this a peer could advertise a
    // commitment it never supplied a matching list for.
    match (is_amsterdam, block_access_list) {
        (true, None) => {
            return Err(PayloadError::PostAmsterdamBlockWithoutBlockAccessList.into());
        }
        (true, Some(bytes)) => {
            let header_hash = sealed_block
                .block_access_list_hash()
                .ok_or(PayloadError::PostAmsterdamBlockWithoutBlockAccessList)?;
            let decoded = DecodedBal::from_rlp_bytes(bytes)
                .map_err(BasePayloadError::InvalidBlockAccessList)?;
            let payload_hash = decoded.hash();
            if payload_hash != header_hash {
                return Err(BasePayloadError::BlockAccessListHashMismatch {
                    header: header_hash,
                    payload: payload_hash,
                });
            }
        }
        (false, _) => {}
    }

    Ok(sealed_block)
}
