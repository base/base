//! Block header verification w.r.t. the EIP-1559 parameters in `extraData` introduced in Holocene.

use alloy_consensus::BlockHeader;
use base_common_chains::Upgrades;
use base_common_consensus::{EIP1559ParamError, HoloceneExtraData, JovianExtraData};
use reth_consensus::ConsensusError;

/// Verifies that the header's own `extraData` encodes valid EIP-1559 parameters.
///
/// From Holocene, `extraData` must be exactly 9 bytes with version `0`; from Jovian, exactly 17
/// bytes with version `1`. In both cases the denominator and elasticity must be non-zero. Before
/// Holocene, `extraData` is unconstrained beyond its size limit.
pub fn ensure_valid_extra_data<H: BlockHeader>(
    chain_spec: impl Upgrades,
    header: &H,
) -> Result<(), ConsensusError> {
    let timestamp = header.timestamp();
    let (elasticity, denominator) = if chain_spec.is_jovian_active_at_timestamp(timestamp) {
        let (elasticity, denominator, _) =
            JovianExtraData::decode(header.extra_data()).map_err(ConsensusError::other)?;
        (elasticity, denominator)
    } else if chain_spec.is_holocene_active_at_timestamp(timestamp) {
        HoloceneExtraData::decode(header.extra_data()).map_err(ConsensusError::other)?
    } else {
        return Ok(());
    };

    if elasticity == 0 || denominator == 0 {
        return Err(ConsensusError::other(EIP1559ParamError::ZeroParams));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::Header;
    use alloy_primitives::Bytes;
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use reth_consensus::HeaderValidator;
    use reth_primitives_traits::SealedHeader;
    use rstest::rstest;

    use super::ensure_valid_extra_data;
    use crate::BaseBeaconConsensus;

    const HOLOCENE_VALID: [u8; 9] = [0, 0, 0, 0, 250, 0, 0, 0, 6];
    const JOVIAN_VALID: [u8; 17] = [1, 0, 0, 0, 250, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0, 1];

    fn builder() -> BaseChainSpecBuilder {
        let mainnet = BaseChainSpec::mainnet();
        BaseChainSpecBuilder::default().genesis(mainnet.genesis.clone()).chain(mainnet.chain)
    }

    fn check(chain_spec: &BaseChainSpec, extra_data: &[u8]) -> bool {
        let header =
            Header { extra_data: Bytes::copy_from_slice(extra_data), ..Default::default() };
        ensure_valid_extra_data(chain_spec, &header).is_ok()
    }

    #[rstest]
    #[case::empty(&[])]
    #[case::arbitrary(&[0xff; 32])]
    fn pre_holocene_extra_data_is_unconstrained(#[case] extra_data: &[u8]) {
        assert!(check(&builder().granite_activated().build(), extra_data));
    }

    #[rstest]
    #[case::valid(&HOLOCENE_VALID, true)]
    #[case::empty(&[], false)]
    #[case::truncated(&HOLOCENE_VALID[..8], false)]
    #[case::wrong_version(&[1, 0, 0, 0, 250, 0, 0, 0, 6], false)]
    #[case::jovian_layout(&JOVIAN_VALID, false)]
    #[case::zero_both(&[0; 9], false)]
    #[case::zero_denominator(&[0, 0, 0, 0, 0, 0, 0, 0, 6], false)]
    #[case::zero_elasticity(&[0, 0, 0, 0, 250, 0, 0, 0, 0], false)]
    fn holocene_extra_data_rules(#[case] extra_data: &[u8], #[case] valid: bool) {
        assert_eq!(check(&builder().holocene_activated().build(), extra_data), valid);
    }

    #[rstest]
    #[case::valid(&JOVIAN_VALID, true)]
    #[case::holocene_layout(&HOLOCENE_VALID, false)]
    #[case::wrong_length(&[1; 9], false)]
    #[case::zero_both(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1], false)]
    #[case::zero_denominator(&[1, 0, 0, 0, 0, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0, 1], false)]
    #[case::zero_elasticity(&[1, 0, 0, 0, 250, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1], false)]
    fn jovian_extra_data_rules(#[case] extra_data: &[u8], #[case] valid: bool) {
        assert_eq!(check(&builder().jovian_activated().build(), extra_data), valid);
    }

    #[rstest]
    #[case::valid(&HOLOCENE_VALID, true)]
    #[case::zero_params(&[0; 9], false)]
    fn validate_header_enforces_holocene_params(#[case] extra_data: &[u8], #[case] valid: bool) {
        let consensus = BaseBeaconConsensus::new(Arc::new(builder().holocene_activated().build()));
        let header = SealedHeader::seal_slow(Header {
            base_fee_per_gas: Some(1),
            extra_data: Bytes::copy_from_slice(extra_data),
            ..Default::default()
        });
        assert_eq!(consensus.validate_header(&header).is_ok(), valid);
    }
}
