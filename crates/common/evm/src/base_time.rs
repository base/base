//! `BaseTime` system contract ABI.

use alloy_primitives::Address;
use alloy_sol_types::sol;
use base_common_consensus::Predeploys;

/// `BaseTime` system contract metadata.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BaseTime;

impl BaseTime {
    /// `BaseTime` predeploy address.
    pub const ADDRESS: Address = Predeploys::BASE_TIME;
}

sol! {
    /// `BaseTime` predeploy ABI.
    interface IBaseTime {
        /// Returns the current block Unix timestamp in milliseconds.
        function timestampMs() external view returns (uint256);

        /// Returns the current block sub-second millisecond component.
        function timestampMillisPart() external view returns (uint256);

        /// Returns the stored Unix millisecond timestamp for `blockNumber`.
        function timestampMsAtBlock(uint256 blockNumber) external view returns (uint256);
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, address};
    use alloy_sol_types::SolCall;

    use super::{BaseTime, IBaseTime};

    #[test]
    fn base_time_address_matches_predeploy_reservation() {
        assert_eq!(BaseTime::ADDRESS, address!("0x420000000000000000000000000000000000001c"));
    }

    #[test]
    fn base_time_abi_selectors_are_distinct() {
        let selectors = [
            IBaseTime::timestampMsCall::SELECTOR,
            IBaseTime::timestampMillisPartCall::SELECTOR,
            IBaseTime::timestampMsAtBlockCall::SELECTOR,
        ];

        assert_ne!(selectors[0], selectors[1]);
        assert_ne!(selectors[0], selectors[2]);
        assert_ne!(selectors[1], selectors[2]);
    }

    #[test]
    fn timestamp_ms_return_encoding_roundtrips() {
        let timestamp_ms = U256::from(1_762_425_600_200u64);
        let encoded = IBaseTime::timestampMsCall::abi_encode_returns(&timestamp_ms);

        assert_eq!(IBaseTime::timestampMsCall::abi_decode_returns(&encoded).unwrap(), timestamp_ms);
    }
}
