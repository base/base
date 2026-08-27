//! L1 block information carried in the EVM2 block environment.

use alloy_primitives::U256;

/// Fixed-point denominator for the Ecotone L1 fee scalars (`10^6`).
const FEE_SCALAR_DENOMINATOR: u64 = 1_000_000;

/// Gas charged per zero byte of transaction calldata posted to L1.
const L1_GAS_PER_ZERO_BYTE: u64 = 4;

/// Gas charged per non-zero byte of transaction calldata posted to L1.
const L1_GAS_PER_NON_ZERO_BYTE: u64 = 16;

/// L1 fee parameters for the current block, carried as the Base
/// [`BlockEnvExt`](evm2::env::BlockEnvExt) extension.
///
/// These are populated from the L1 block attributes deposit and drive the L1
/// data fee charged to every non-deposit transaction. This models the Ecotone
/// schedule; later fork schedules (e.g. Fjord's FLZ-compressed estimate) are
/// layered on in follow-up work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct L1BlockInfo {
    /// Current L1 base fee.
    pub l1_base_fee: U256,
    /// Current L1 blob base fee.
    pub l1_blob_base_fee: U256,
    /// Ecotone base-fee scalar.
    pub base_fee_scalar: u64,
    /// Ecotone blob-base-fee scalar.
    pub blob_base_fee_scalar: u64,
}

impl L1BlockInfo {
    /// Returns the L1 calldata gas for `input` using the EIP-2028 byte costs.
    ///
    /// Every byte posted to L1 is charged: zero bytes at [`L1_GAS_PER_ZERO_BYTE`]
    /// and non-zero bytes at [`L1_GAS_PER_NON_ZERO_BYTE`].
    pub fn data_gas(input: &[u8]) -> u64 {
        input.iter().fold(0u64, |acc, &byte| {
            acc.saturating_add(if byte == 0 {
                L1_GAS_PER_ZERO_BYTE
            } else {
                L1_GAS_PER_NON_ZERO_BYTE
            })
        })
    }

    /// Calculates the Ecotone L1 data fee for a transaction whose posted
    /// calldata is `input`.
    ///
    /// `fee = data_gas * (16 * base_fee_scalar * l1_base_fee
    ///        + blob_base_fee_scalar * l1_blob_base_fee) / (16 * 10^6)`.
    pub fn calculate_tx_l1_cost(&self, input: &[u8]) -> U256 {
        let data_gas = U256::from(Self::data_gas(input));
        let weighted_base_fee = U256::from(L1_GAS_PER_NON_ZERO_BYTE)
            .saturating_mul(U256::from(self.base_fee_scalar))
            .saturating_mul(self.l1_base_fee);
        let weighted_blob_fee =
            U256::from(self.blob_base_fee_scalar).saturating_mul(self.l1_blob_base_fee);
        let scaled = data_gas.saturating_mul(weighted_base_fee.saturating_add(weighted_blob_fee));
        scaled / U256::from(L1_GAS_PER_NON_ZERO_BYTE * FEE_SCALAR_DENOMINATOR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_gas_charges_per_byte_class() {
        // two zero bytes (4 each) + three non-zero bytes (16 each) = 8 + 48 = 56.
        assert_eq!(L1BlockInfo::data_gas(&[0, 0, 1, 2, 3]), 56);
        assert_eq!(L1BlockInfo::data_gas(&[]), 0);
    }

    #[test]
    fn l1_cost_scales_with_base_fee_and_input() {
        let info = L1BlockInfo {
            l1_base_fee: U256::from(1_000_000_000u64),
            l1_blob_base_fee: U256::ZERO,
            base_fee_scalar: 1_000_000,
            blob_base_fee_scalar: 0,
        };
        // With blob terms zero and base_fee_scalar == denominator, the cost reduces
        // to data_gas * l1_base_fee.
        let input = [1u8; 10];
        let expected = U256::from(L1BlockInfo::data_gas(&input)) * U256::from(1_000_000_000u64);
        assert_eq!(info.calculate_tx_l1_cost(&input), expected);
    }

    #[test]
    fn l1_cost_is_zero_without_fees() {
        assert_eq!(L1BlockInfo::default().calculate_tx_l1_cost(&[1, 2, 3]), U256::ZERO);
    }
}
