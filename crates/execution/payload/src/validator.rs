//! Validates execution payload wrt Base consensus rules

use alloc::sync::Arc;

use alloy_consensus::{Block, BlockHeader};
use alloy_eips::eip7928::bal::DecodedBal;
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_engine::PayloadError;
use base_common_chains::Upgrades;
use base_common_rpc_types_engine::{BasePayloadError, ExecutionData};
use derive_more::{Constructor, Deref};
use reth_payload_validator::{cancun, prague, shanghai};
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

    validate_block_access_list(
        block_access_list,
        sealed_block.block_access_list_hash(),
    )?;

    Ok(sealed_block)
}

/// Verifies a block access list carried alongside a payload against the block header.
///
/// EIP-7928 commits to the list from the block header, so a carried list must hash to that
/// commitment, and a list is only meaningful when the header commits to one. Base payloads do
/// not transport the list yet, so a commitment without a list is not an error: execution
/// regenerates the list and consensus validation compares it against the header.
pub fn validate_block_access_list(
    block_access_list: Option<Bytes>,
    header_block_access_list_hash: Option<B256>,
) -> Result<(), BasePayloadError> {
    match (block_access_list, header_block_access_list_hash) {
        (Some(bytes), Some(header_hash)) => {
            let decoded = DecodedBal::from_rlp_bytes(bytes)
                .map_err(BasePayloadError::InvalidBlockAccessList)?;
            let payload_hash = decoded.hash();
            if payload_hash != header_hash {
                return Err(BasePayloadError::BlockAccessListHashMismatch {
                    header: header_hash,
                    payload: payload_hash,
                });
            }
            Ok(())
        }
        (Some(_), None) => Err(BasePayloadError::UnexpectedBlockAccessList),
        (None, _) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip7928::bal::Bal;
    use alloy_primitives::b256;

    use super::*;

    /// An empty block access list and the commitment and RLP encoding that describe it.
    fn empty_block_access_list() -> (Bytes, B256) {
        let bal = Bal::default();
        (alloy_rlp::encode(&bal).into(), bal.compute_hash())
    }

    /// A list that hashes to the header commitment must be accepted.
    #[test]
    fn accepts_matching_block_access_list() {
        let (bytes, committed) = empty_block_access_list();

        validate_block_access_list(Some(bytes), Some(committed))
            .expect("a list matching the header commitment must be accepted");
    }

    /// A list that does not hash to the header commitment must be rejected.
    #[test]
    fn rejects_mismatched_block_access_list() {
        let (bytes, committed) = empty_block_access_list();
        let other = b256!("0x1111111111111111111111111111111111111111111111111111111111111111");
        assert_ne!(committed, other, "the fixture must commit to a different list");

        let error = validate_block_access_list(Some(bytes), Some(other))
            .expect_err("a list that does not match the commitment must be rejected");
        assert!(
            matches!(
                error,
                BasePayloadError::BlockAccessListHashMismatch { header, payload }
                    if header == other && payload == committed
            ),
            "the error must report the committed and carried hashes, got {error:?}"
        );
    }

    /// A carried list that is not valid RLP must be rejected.
    #[test]
    fn rejects_undecodable_block_access_list() {
        let error = validate_block_access_list(
            Some(Bytes::from_static(&[0xff, 0xff, 0xff])),
            Some(B256::ZERO),
        )
        .expect_err("an undecodable list must be rejected");
        assert!(
            matches!(error, BasePayloadError::InvalidBlockAccessList(_)),
            "expected a decode error, got {error:?}"
        );
    }

    /// A carried list without a header commitment must be rejected.
    #[test]
    fn rejects_block_access_list_without_commitment() {
        let (bytes, _) = empty_block_access_list();

        let error = validate_block_access_list(Some(bytes), None)
            .expect_err("a list without a commitment must be rejected");
        assert!(
            matches!(error, BasePayloadError::UnexpectedBlockAccessList),
            "expected an unexpected-list error, got {error:?}"
        );
    }

    /// A commitment without a carried list is accepted: execution regenerates the list.
    #[test]
    fn accepts_commitment_without_carried_block_access_list() {
        validate_block_access_list(None, Some(B256::repeat_byte(0x11)))
            .expect("a commitment without a carried list must be accepted");
    }

    /// Pre-Amsterdam payloads carry neither a list nor a commitment.
    #[test]
    fn accepts_absent_block_access_list() {
        validate_block_access_list(None, None)
            .expect("an absent list and commitment must be accepted");
    }

    /// An Amsterdam payload must rebuild the header it came from, commitment and slot number
    /// included, so every consumer that reconstructs a block from a payload derives the same
    /// block hash.
    #[test]
    fn amsterdam_payload_round_trips_the_header_commitment() {
        use alloy_consensus::Header;
        use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
        use base_common_chains::BaseUpgrade;
        use base_common_consensus::BaseTxEnvelope;
        use base_common_rpc_types_engine::BaseExecutionPayload;
        use base_execution_chainspec::BaseChainSpecBuilder;
        use reth_ethereum_forks::{EthereumHardfork, ForkCondition};

        // Base mainnet activates Isthmus before this, so every upgrade the payload shape depends
        // on is active as well.
        const TIMESTAMP: u64 = 1_800_000_000;

        let chain_spec = BaseChainSpecBuilder::base_mainnet()
            .with_fork(BaseUpgrade::Denim, ForkCondition::Timestamp(TIMESTAMP))
            .with_fork(EthereumHardfork::Amsterdam, ForkCondition::Timestamp(TIMESTAMP))
            .build();

        let (bytes, committed) = empty_block_access_list();
        let block = Block::<BaseTxEnvelope> {
            header: Header {
                number: 1,
                timestamp: TIMESTAMP,
                withdrawals_root: Some(B256::ZERO),
                requests_hash: Some(EMPTY_REQUESTS_HASH),
                blob_gas_used: Some(0),
                excess_blob_gas: Some(0),
                parent_beacon_block_root: Some(B256::repeat_byte(0x22)),
                block_access_list_hash: Some(committed),
                slot_number: Some(1),
                ..Default::default()
            },
            body: Default::default(),
        };

        // A payload carries the hash of the block it reconstructs, so derive it the way a
        // consumer would: rebuild the block from the payload, then commit to its hash.
        let (probe, sidecar) = BaseExecutionPayload::from_block_unchecked_with_extras(
            B256::ZERO,
            &block,
            Some(bytes.clone()),
        );
        let derived_hash = probe
            .try_into_block_with_sidecar::<BaseTxEnvelope>(&sidecar)
            .expect("the payload must reconstruct the block")
            .seal_slow()
            .hash();

        let (payload, _) = BaseExecutionPayload::from_block_unchecked_with_extras(
            derived_hash,
            &block,
            Some(bytes.clone()),
        );
        let sealed: SealedBlock<Block<BaseTxEnvelope>> = ensure_well_formed_payload(
            &chain_spec,
            ExecutionData::new(payload, sidecar, Some(bytes)),
        )
        .expect("an Amsterdam payload must round trip through payload validation");

        assert_eq!(
            sealed.hash(),
            derived_hash,
            "the rebuilt block must hash to the hash the payload commits to"
        );
        assert_eq!(
            sealed.block_access_list_hash(),
            Some(committed),
            "the rebuilt header must commit to the carried block access list"
        );
        assert_eq!(
            sealed.slot_number(),
            Some(1),
            "the rebuilt header must carry the slot number from the payload"
        );
    }
}
