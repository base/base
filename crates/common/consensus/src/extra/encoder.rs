use alloy_eips::eip1559::BaseFeeParams;
use alloy_primitives::B64;

use super::{EIP1559ParamError, HoloceneExtraData};

/// Encoder for EIP-1559 extra-data parameters.
#[derive(Debug)]
pub struct EIP1559ParamEncoder;

impl EIP1559ParamEncoder {
    /// Encodes the EIP-1559 parameters into `extra_data`.
    ///
    /// If `eip_1559_params` is zero, uses `default_base_fee_params` instead.
    /// Requires `extra_data` to be at least 9 bytes.
    pub fn encode(
        eip_1559_params: B64,
        default_base_fee_params: BaseFeeParams,
        extra_data: &mut [u8],
    ) -> Result<(), EIP1559ParamError> {
        if extra_data.len() < 9 {
            return Err(EIP1559ParamError::InvalidExtraDataLength);
        }
        if eip_1559_params.is_zero() {
            let max_change_denominator: u32 = (default_base_fee_params.max_change_denominator)
                .try_into()
                .map_err(|_| EIP1559ParamError::DenominatorOverflow)?;
            let elasticity_multiplier: u32 = (default_base_fee_params.elasticity_multiplier)
                .try_into()
                .map_err(|_| EIP1559ParamError::ElasticityOverflow)?;
            extra_data[1..5].copy_from_slice(&max_change_denominator.to_be_bytes());
            extra_data[5..9].copy_from_slice(&elasticity_multiplier.to_be_bytes());
        } else {
            let (elasticity, denominator) = HoloceneExtraData::decode_params(eip_1559_params);
            extra_data[1..5].copy_from_slice(&denominator.to_be_bytes());
            extra_data[5..9].copy_from_slice(&elasticity.to_be_bytes());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip1559::BaseFeeParams;
    use alloy_primitives::B64;

    use super::EIP1559ParamEncoder;
    use crate::extra::EIP1559ParamError;

    #[test]
    fn test_encode_eip_1559_params_invalid_length() {
        let mut extra_data = [0u8; 8];
        let result =
            EIP1559ParamEncoder::encode(B64::ZERO, BaseFeeParams::new(80, 60), &mut extra_data);
        assert_eq!(result.unwrap_err(), EIP1559ParamError::InvalidExtraDataLength);
    }
}
