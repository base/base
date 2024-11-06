//! Support for EIP-1559 parameters after holocene.

use alloy_eips::eip1559::BaseFeeParams;
use alloy_primitives::{Bytes, B64};

/// Extracts the Holocene 1599 parameters from the encoded form:
/// <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/holocene/exec-engine.md#eip1559params-encoding>
///
/// Returns (`elasticity`, `denominator`)
pub fn decode_eip_1559_params(eip_1559_params: B64) -> (u32, u32) {
    let denominator: [u8; 4] = eip_1559_params.0[..4].try_into().expect("sufficient length");
    let elasticity: [u8; 4] = eip_1559_params.0[4..8].try_into().expect("sufficient length");

    (u32::from_be_bytes(elasticity), u32::from_be_bytes(denominator))
}

/// Extracts the `eip1559` parameters for the payload.
pub fn decode_holocene_extra_data(
    eip_1559_params: B64,
    default_base_fee_params: BaseFeeParams,
) -> Result<Bytes, EIP1559ParamError> {
    let mut extra_data = [0u8; 9];
    // If eip 1559 params aren't set, use the canyon base fee param constants
    // otherwise use them
    if eip_1559_params.is_zero() {
        // Try casting max_change_denominator to u32
        let max_change_denominator: u32 = (default_base_fee_params.max_change_denominator)
            .try_into()
            .map_err(|_| EIP1559ParamError::DenominatorOverflow)?;

        // Try casting elasticity_multiplier to u32
        let elasticity_multiplier: u32 = (default_base_fee_params.elasticity_multiplier)
            .try_into()
            .map_err(|_| EIP1559ParamError::ElasticityOverflow)?;

        // Copy the values safely
        extra_data[1..5].copy_from_slice(&max_change_denominator.to_be_bytes());
        extra_data[5..9].copy_from_slice(&elasticity_multiplier.to_be_bytes());
    } else {
        let (elasticity, denominator) = decode_eip_1559_params(eip_1559_params);
        extra_data[1..5].copy_from_slice(&denominator.to_be_bytes());
        extra_data[5..9].copy_from_slice(&elasticity.to_be_bytes());
    }
    Ok(Bytes::copy_from_slice(&extra_data))
}

/// Error type for EIP-1559 parameters
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EIP1559ParamError {
    /// No EIP-1559 parameters provided
    NoEIP1559Params,
    /// Denominator overflow
    DenominatorOverflow,
    /// Elasticity overflow
    ElasticityOverflow,
}

impl core::fmt::Display for EIP1559ParamError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoEIP1559Params => {
                write!(f, "No EIP1559 parameters provided")
            }
            Self::DenominatorOverflow => write!(f, "Denominator overflow"),
            Self::ElasticityOverflow => {
                write!(f, "Elasticity overflow")
            }
        }
    }
}

impl core::error::Error for EIP1559ParamError {}

#[cfg(test)]
mod tests {
    use super::*;
    use core::str::FromStr;

    #[test]
    fn test_get_extra_data_post_holocene() {
        let eip_1559_params = B64::from_str("0x0000000800000008").unwrap();
        let extra_data = decode_holocene_extra_data(eip_1559_params, BaseFeeParams::new(80, 60));
        assert_eq!(extra_data.unwrap(), Bytes::copy_from_slice(&[0, 0, 0, 0, 8, 0, 0, 0, 8]));
    }

    #[test]
    fn test_get_extra_data_post_holocene_default() {
        let eip_1559_params = B64::ZERO;
        let extra_data = decode_holocene_extra_data(eip_1559_params, BaseFeeParams::new(80, 60));
        assert_eq!(extra_data.unwrap(), Bytes::copy_from_slice(&[0, 0, 0, 0, 80, 0, 0, 0, 60]));
    }
}
