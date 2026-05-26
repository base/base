//! `BaseTime` system contract ABI.

use alloy_primitives::Address;
use alloy_sol_types::sol;
use base_common_consensus::Predeploys;
use revm::primitives::{U256, uint};

/// `BaseTime` system contract metadata.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BaseTime;

impl BaseTime {
    /// `BaseTime` predeploy address.
    pub const ADDRESS: Address = Predeploys::BASE_TIME;

    /// Storage slot containing the latest block timestamp in milliseconds.
    pub const LATEST_TIMESTAMP_MILLIS_SLOT: U256 = uint!(0_U256);

    /// Storage slot containing the latest written L2 block number.
    pub const LATEST_BLOCK_NUMBER_SLOT: U256 = uint!(1_U256);

    /// Number of block-number-keyed entries retained by the history ring buffer.
    pub const HISTORY_BUFFER_LENGTH: u64 = 8191;

    /// First storage slot for historical millisecond timestamps.
    pub const HISTORY_TIMESTAMP_BASE_SLOT: U256 = uint!(2_U256);

    /// First storage slot for historical block-number freshness markers.
    pub const HISTORY_BLOCK_NUMBER_BASE_SLOT: U256 = uint!(8193_U256);

    /// Returns the block-number-keyed ring buffer index for `block_number`.
    pub const fn history_index(block_number: u64) -> u64 {
        block_number % Self::HISTORY_BUFFER_LENGTH
    }

    /// Returns the timestamp history storage slot for `block_number`.
    pub fn timestamp_history_slot(block_number: u64) -> U256 {
        Self::HISTORY_TIMESTAMP_BASE_SLOT + U256::from(Self::history_index(block_number))
    }

    /// Returns the block-number marker history storage slot for `block_number`.
    pub fn block_number_history_slot(block_number: u64) -> U256 {
        Self::HISTORY_BLOCK_NUMBER_BASE_SLOT + U256::from(Self::history_index(block_number))
    }
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

    #[test]
    fn base_time_current_storage_slots_match_layout() {
        assert_eq!(BaseTime::LATEST_TIMESTAMP_MILLIS_SLOT, U256::ZERO);
        assert_eq!(BaseTime::LATEST_BLOCK_NUMBER_SLOT, U256::from(1));
    }

    #[test]
    fn base_time_history_slots_are_block_number_keyed() {
        assert_eq!(BaseTime::HISTORY_BUFFER_LENGTH, 8191);
        assert_eq!(BaseTime::timestamp_history_slot(0), U256::from(2));
        assert_eq!(BaseTime::block_number_history_slot(0), U256::from(8193));

        let wrapped_block = BaseTime::HISTORY_BUFFER_LENGTH;
        assert_eq!(BaseTime::timestamp_history_slot(wrapped_block), U256::from(2));
        assert_eq!(BaseTime::block_number_history_slot(wrapped_block), U256::from(8193));

        assert_eq!(BaseTime::timestamp_history_slot(1), U256::from(3));
        assert_eq!(BaseTime::block_number_history_slot(1), U256::from(8194));
    }
}
