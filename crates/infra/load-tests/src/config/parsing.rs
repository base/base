use alloy_primitives::{Address, U256};

use crate::utils::{BaselineError, Result};

pub(super) fn parse_address(s: &str, field: &str) -> Result<Address> {
    s.parse::<Address>()
        .map_err(|e| BaselineError::Config(format!("invalid {field} address '{s}': {e}")))
}

pub(super) fn parse_amount(s: &str, field: &str) -> Result<U256> {
    s.parse::<U256>().map_err(|e| BaselineError::Config(format!("invalid {field} '{s}': {e}")))
}

/// Parses a [`U256`] from a hex string (`0x`-prefixed) or a decimal string.
///
/// Storage slots, masks, and predicate values are commonly written in hex, so
/// this accepts both forms rather than the decimal-only [`parse_amount`].
pub(super) fn parse_u256_hex_or_dec(s: &str, field: &str) -> Result<U256> {
    let trimmed = s.trim();
    let parsed = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .map_or_else(|| U256::from_str_radix(trimmed, 10), |hex| U256::from_str_radix(hex, 16));
    parsed.map_err(|e| BaselineError::Config(format!("invalid {field} '{s}': {e}")))
}

pub(super) fn validate_swap_amounts(min: U256, max: U256, tx_type: &str) -> Result<()> {
    if min > max {
        return Err(BaselineError::Config(format!(
            "{tx_type} min_amount ({min}) exceeds max_amount ({max})"
        )));
    }
    let u128_max = U256::from(u128::MAX);
    if min > u128_max {
        return Err(BaselineError::Config(format!(
            "{tx_type} min_amount ({min}) exceeds u128::MAX — swap calls require u128 amounts"
        )));
    }
    if max > u128_max {
        return Err(BaselineError::Config(format!(
            "{tx_type} max_amount ({max}) exceeds u128::MAX — swap calls require u128 amounts"
        )));
    }
    Ok(())
}
