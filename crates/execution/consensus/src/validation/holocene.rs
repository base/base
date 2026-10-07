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

    #[test]
    fn pre_holocene_extra_data_is_unconstrained() {
        let spec = builder().granite_activated().build();
        assert!(check(&spec, &[]));
        assert!(check(&spec, &[0xff; 32]));
    }

    #[test]
    fn holocene_extra_data_rules() {
        let spec = builder().holocene_activated().build();
        assert!(check(&spec, &HOLOCENE_VALID));
        assert!(!check(&spec, &[]));
        assert!(!check(&spec, &HOLOCENE_VALID[..8]));
        assert!(!check(&spec, &[1, 0, 0, 0, 250, 0, 0, 0, 6]));
        assert!(!check(&spec, &JOVIAN_VALID));
        assert!(!check(&spec, &[0; 9]));
        assert!(!check(&spec, &[0, 0, 0, 0, 0, 0, 0, 0, 6]));
        assert!(!check(&spec, &[0, 0, 0, 0, 250, 0, 0, 0, 0]));
    }

    #[test]
    fn jovian_extra_data_rules() {
        let spec = builder().jovian_activated().build();
        assert!(check(&spec, &JOVIAN_VALID));
        assert!(!check(&spec, &HOLOCENE_VALID));
        assert!(!check(&spec, &[1; 9]));
        assert!(!check(&spec, &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]));
        assert!(!check(&spec, &[1, 0, 0, 0, 0, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0, 1]));
        assert!(!check(&spec, &[1, 0, 0, 0, 250, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]));
    }

    #[test]
    fn validate_header_rejects_zero_holocene_params() {
        let consensus = BaseBeaconConsensus::new(Arc::new(builder().holocene_activated().build()));
        let header = |extra_data: &[u8]| {
            SealedHeader::seal_slow(Header {
                base_fee_per_gas: Some(1),
                extra_data: Bytes::copy_from_slice(extra_data),
                ..Default::default()
            })
        };
        assert!(consensus.validate_header(&header(&HOLOCENE_VALID)).is_ok());
        assert!(consensus.validate_header(&header(&[0; 9])).is_err());
    }
}
