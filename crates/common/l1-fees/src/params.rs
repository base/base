//! Engine-neutral OP-stack L1 fee parameters and cost math.

use alloy_primitives::U256;
use base_common_flz::{NON_ZERO_BYTE_COST, tx_estimated_size_fjord};
use base_common_genesis::BaseUpgrade;

/// Gas per calldata token (EIP-2028 standard token cost).
pub const STANDARD_TOKEN_COST: u64 = 4;
/// Istanbul non-zero-byte token multiplier: a non-zero byte counts as this many tokens.
pub const NON_ZERO_BYTE_MULTIPLIER_ISTANBUL: u64 = 4;
/// Fixed-point decimal scaling factor for the operator fee scalar (6 decimal points).
pub const OPERATOR_FEE_SCALAR_DECIMAL: u64 = 1_000_000;
/// Jovian multiplier applied to the operator fee scalar component.
pub const OPERATOR_FEE_JOVIAN_MULTIPLIER: u64 = 100;

/// Deposit-transaction EIP-2718 type byte; deposits are exempt from L1 fees.
const DEPOSIT_TX_TYPE: u8 = 0x7E;
/// Extra signature bytes folded into pre-Regolith rollup data cost.
const PRE_REGOLITH_SIGNATURE_BYTES: u64 = 68;

/// Engine-neutral L1 fee parameters for a block, as posted by the L1 attributes
/// deposit.
///
/// Carries the values needed to price the L1 data fee and the Isthmus operator
/// fee. The math ([`L1FeeParams::calculate_tx_l1_cost`],
/// [`L1FeeParams::operator_fee_charge`]) is a faithful, engine-independent port
/// of the OP-stack schedule and is shared by the revm and EVM2 integrations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct L1FeeParams {
    /// The base fee of the L1 origin block.
    pub l1_base_fee: U256,
    /// The current L1 fee overhead. `None` once Ecotone is activated.
    pub l1_fee_overhead: Option<U256>,
    /// The current L1 base fee scalar.
    pub l1_base_fee_scalar: U256,
    /// The current L1 blob base fee. `None` before Ecotone.
    pub l1_blob_base_fee: Option<U256>,
    /// The current L1 blob base fee scalar. `None` before Ecotone.
    pub l1_blob_base_fee_scalar: Option<U256>,
    /// The operator fee scalar. `None` before Isthmus.
    pub operator_fee_scalar: Option<U256>,
    /// The operator fee constant. `None` before Isthmus.
    pub operator_fee_constant: Option<U256>,
    /// True if Ecotone is activated but the L1 fee scalars have not yet been set.
    pub empty_ecotone_scalars: bool,
    /// The Jovian DA-footprint gas scalar. `None` before Jovian (or when unset).
    pub da_footprint_gas_scalar: Option<U256>,
}

impl L1FeeParams {
    /// Returns whether `target` is active at `upgrade`.
    ///
    /// Mirrors `BaseSpecId::is_enabled_in`: upgrades are ordered by discriminant.
    const fn is_enabled(upgrade: BaseUpgrade, target: BaseUpgrade) -> bool {
        (target as u8) <= (upgrade as u8)
    }

    /// Returns whether `input` is fee-exempt (empty or a deposit transaction).
    fn is_fee_exempt(input: &[u8]) -> bool {
        input.is_empty() || input.first() == Some(&DEPOSIT_TX_TYPE)
    }

    /// Calculates the L1 calldata gas for posting `input`, per the schedule at `upgrade`.
    ///
    /// Post-Fjord uses the FastLZ-estimated compressed size; earlier forks count
    /// EIP-2028 calldata tokens, with pre-Regolith adding 68 signature bytes.
    pub fn data_gas(input: &[u8], upgrade: BaseUpgrade) -> U256 {
        if Self::is_enabled(upgrade, BaseUpgrade::Fjord) {
            let estimated_size = U256::from(tx_estimated_size_fjord(input));
            return estimated_size
                .saturating_mul(U256::from(NON_ZERO_BYTE_COST))
                .wrapping_div(U256::from(1_000_000u64));
        }

        let mut tokens = input.iter().fold(0u64, |acc, &byte| {
            acc + if byte == 0 { 1 } else { NON_ZERO_BYTE_MULTIPLIER_ISTANBUL }
        });
        if !Self::is_enabled(upgrade, BaseUpgrade::Regolith) {
            tokens += PRE_REGOLITH_SIGNATURE_BYTES * NON_ZERO_BYTE_MULTIPLIER_ISTANBUL;
        }
        U256::from(tokens.saturating_mul(STANDARD_TOKEN_COST))
    }

    /// Calculates the Jovian DA-footprint gas for posting the enveloped transaction bytes `input`.
    ///
    /// The footprint is the FastLZ-estimated compressed size (scaled by `1e6`, as
    /// [`tx_estimated_size_fjord`] returns) divided back down by `1e6` and multiplied by the
    /// DA-footprint gas scalar. Returns zero when no scalar is set (pre-Jovian or unconfigured).
    /// Mirrors the reference `jovian_da_footprint_estimation`.
    pub fn jovian_da_footprint(&self, input: &[u8]) -> u64 {
        let Some(scalar) = self.da_footprint_gas_scalar else {
            return 0;
        };
        U256::from(tx_estimated_size_fjord(input))
            .wrapping_div(U256::from(1_000_000u64))
            .saturating_mul(scalar)
            .saturating_to::<u64>()
    }

    /// Calculates the L1 data-posting cost for `input` at `upgrade`.
    ///
    /// Deposit and empty transactions are fee-exempt. This is the pure fee math;
    /// callers own any caching.
    pub fn calculate_tx_l1_cost(&self, input: &[u8], upgrade: BaseUpgrade) -> U256 {
        if Self::is_fee_exempt(input) {
            return U256::ZERO;
        }
        if Self::is_enabled(upgrade, BaseUpgrade::Fjord) {
            self.calculate_tx_l1_cost_fjord(input)
        } else if Self::is_enabled(upgrade, BaseUpgrade::Ecotone) {
            self.calculate_tx_l1_cost_ecotone(input, upgrade)
        } else {
            self.calculate_tx_l1_cost_bedrock(input, upgrade)
        }
    }

    /// Pre-Ecotone (Bedrock) L1 cost. Deposit and empty transactions are fee-exempt.
    pub fn calculate_tx_l1_cost_bedrock(&self, input: &[u8], upgrade: BaseUpgrade) -> U256 {
        if Self::is_fee_exempt(input) {
            return U256::ZERO;
        }
        Self::data_gas(input, upgrade)
            .saturating_add(self.l1_fee_overhead.unwrap_or_default())
            .saturating_mul(self.l1_base_fee)
            .saturating_mul(self.l1_base_fee_scalar)
            .wrapping_div(U256::from(1_000_000u64))
    }

    /// Post-Ecotone L1 cost:
    /// `calldataGas * (l1BaseFee*16*l1BaseFeeScalar + l1BlobBaseFee*l1BlobBaseFeeScalar) / 16e6`.
    pub fn calculate_tx_l1_cost_ecotone(&self, input: &[u8], upgrade: BaseUpgrade) -> U256 {
        if Self::is_fee_exempt(input) {
            return U256::ZERO;
        }
        // The very first Ecotone block (unless activated at genesis) still prices
        // using the Bedrock function, detected via unset Ecotone scalars.
        if self.empty_ecotone_scalars {
            return self.calculate_tx_l1_cost_bedrock(input, upgrade);
        }
        let rollup_data_gas_cost = Self::data_gas(input, upgrade);
        self.calculate_l1_fee_scaled_ecotone()
            .saturating_mul(rollup_data_gas_cost)
            .wrapping_div(U256::from(1_000_000u64 * NON_ZERO_BYTE_COST))
    }

    /// Post-Fjord L1 cost:
    /// `estimatedSize * (baseFeeScalar*l1BaseFee*16 + blobFeeScalar*l1BlobBaseFee) / 1e12`.
    pub fn calculate_tx_l1_cost_fjord(&self, input: &[u8]) -> U256 {
        if Self::is_fee_exempt(input) {
            return U256::ZERO;
        }
        let l1_fee_scaled = self.calculate_l1_fee_scaled_ecotone();
        if l1_fee_scaled.is_zero() {
            return U256::ZERO;
        }
        U256::from(tx_estimated_size_fjord(input))
            .saturating_mul(l1_fee_scaled)
            .wrapping_div(U256::from(1_000_000_000_000u64))
    }

    /// `l1BaseFee*16*l1BaseFeeScalar + l1BlobBaseFee*l1BlobBaseFeeScalar`.
    fn calculate_l1_fee_scaled_ecotone(&self) -> U256 {
        let calldata_cost_per_byte = self
            .l1_base_fee
            .saturating_mul(U256::from(NON_ZERO_BYTE_COST))
            .saturating_mul(self.l1_base_fee_scalar);
        let blob_cost_per_byte = self
            .l1_blob_base_fee
            .unwrap_or_default()
            .saturating_mul(self.l1_blob_base_fee_scalar.unwrap_or_default());
        calldata_cost_per_byte.saturating_add(blob_cost_per_byte)
    }

    /// Calculates the Isthmus operator fee for `input` at gas budget `gas_limit`.
    ///
    /// Deposit and empty transactions are exempt; pre-Isthmus this is zero.
    pub fn operator_fee_charge(&self, input: &[u8], gas_limit: U256, upgrade: BaseUpgrade) -> U256 {
        if Self::is_fee_exempt(input) {
            return U256::ZERO;
        }
        self.operator_fee_charge_inner(gas_limit, upgrade)
    }

    /// Calculates the operator fee for a given `gas` amount.
    ///
    /// Missing scalars fall back to zero to match the execution path during the
    /// txpool bootstrap window before the first L1 attributes deposit.
    pub fn operator_fee_charge_inner(&self, gas: U256, upgrade: BaseUpgrade) -> U256 {
        let operator_fee_scalar = self.operator_fee_scalar.unwrap_or_default();
        let operator_fee_constant = self.operator_fee_constant.unwrap_or_default();

        let product = if Self::is_enabled(upgrade, BaseUpgrade::Jovian) {
            gas.saturating_mul(operator_fee_scalar)
                .saturating_mul(U256::from(OPERATOR_FEE_JOVIAN_MULTIPLIER))
        } else {
            gas.saturating_mul(operator_fee_scalar) / U256::from(OPERATOR_FEE_SCALAR_DECIMAL)
        };
        product.saturating_add(operator_fee_constant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> L1FeeParams {
        L1FeeParams {
            l1_base_fee: U256::from(1_000_000),
            l1_fee_overhead: Some(U256::from(1_000_000)),
            l1_base_fee_scalar: U256::from(1_000_000),
            ..Default::default()
        }
    }

    #[test]
    fn data_gas_non_zero_bytes_matches_reference() {
        let input = alloy_primitives::bytes!("FACADE");
        assert_eq!(L1FeeParams::data_gas(&input, BaseUpgrade::Bedrock), U256::from(1136));
        assert_eq!(L1FeeParams::data_gas(&input, BaseUpgrade::Regolith), U256::from(48));
        assert_eq!(L1FeeParams::data_gas(&input, BaseUpgrade::Fjord), U256::from(1600));
    }

    #[test]
    fn data_gas_zero_bytes_matches_reference() {
        let input = alloy_primitives::bytes!("FA00CA00DE");
        assert_eq!(L1FeeParams::data_gas(&input, BaseUpgrade::Bedrock), U256::from(1144));
    }

    #[test]
    fn jovian_da_footprint_zero_without_scalar() {
        // No DA scalar (pre-Jovian / unconfigured) yields no footprint.
        assert_eq!(params().jovian_da_footprint(&[0x02, 0xAB, 0xCD]), 0);
    }

    #[test]
    fn jovian_da_footprint_uses_min_size_and_scalar() {
        // A small input floors at the minimum FastLZ size (100 * 1e6); divided by 1e6 that is 100
        // compressed bytes, times the scalar.
        let p = L1FeeParams { da_footprint_gas_scalar: Some(U256::from(7)), ..Default::default() };
        assert_eq!(p.jovian_da_footprint(&[0x02, 0xAB, 0xCD]), 100 * 7);
    }

    #[test]
    fn deposit_and_empty_are_fee_exempt() {
        let p = params();
        assert_eq!(p.calculate_tx_l1_cost(&[], BaseUpgrade::Ecotone), U256::ZERO);
        assert_eq!(p.calculate_tx_l1_cost(&[0x7E, 1, 2], BaseUpgrade::Ecotone), U256::ZERO);
    }

    #[test]
    fn operator_fee_zero_before_isthmus_scalars() {
        // Without scalars the operator fee reduces to the (zero) constant.
        let p = params();
        assert_eq!(
            p.operator_fee_charge_inner(U256::from(21_000), BaseUpgrade::Isthmus),
            U256::ZERO
        );
    }
}
