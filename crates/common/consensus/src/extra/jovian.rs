use alloy_eips::eip1559::BaseFeeParams;
use alloy_primitives::{B64, Bytes};

use super::{EIP1559ParamError, encode_eip_1559_params};

const VERSION_BYTE: u8 = 1;

/// EIP-1559 extra data encoding for the Jovian hardfork.
///
/// Jovian extra data is 17 bytes:
/// - 1 byte version (always `1`)
/// - 4 bytes `max_change_denominator` (big-endian u32)
/// - 4 bytes `elasticity_multiplier` (big-endian u32)
/// - 8 bytes `min_base_fee` (big-endian u64)
///
/// See: <https://specs.base.org/upgrades/jovian/exec-engine>
#[derive(Debug)]
pub struct JovianExtraData;

impl JovianExtraData {
    /// Decodes the EIP-1559 parameters from Jovian `extra_data` bytes.
    ///
    /// Expects exactly 17 bytes with a leading version byte of `1`.
    ///
    /// Returns (`elasticity`, `denominator`, `min_base_fee`).
    pub fn decode(extra_data: &[u8]) -> Result<(u32, u32, u64), EIP1559ParamError> {
        if extra_data.len() != 17 {
            return Err(EIP1559ParamError::InvalidExtraDataLength);
        }
        if extra_data[0] != VERSION_BYTE {
            return Err(EIP1559ParamError::InvalidVersion(extra_data[0]));
        }
        let denominator: [u8; 4] = extra_data[1..5].try_into().expect("sufficient length");
        let elasticity: [u8; 4] = extra_data[5..9].try_into().expect("sufficient length");
        let min_base_fee: [u8; 8] = extra_data[9..17].try_into().expect("sufficient length");
        Ok((
            u32::from_be_bytes(elasticity),
            u32::from_be_bytes(denominator),
            u64::from_be_bytes(min_base_fee),
        ))
    }

    /// Encodes the EIP-1559 parameters into Jovian `extra_data` bytes.
    ///
    /// Produces a 17-byte encoding with a leading version byte of `1`.
    pub fn encode(
        eip_1559_params: B64,
        default_base_fee_params: BaseFeeParams,
        min_base_fee: u64,
    ) -> Result<Bytes, EIP1559ParamError> {
        let mut extra_data = [0u8; 17];
        extra_data[0] = VERSION_BYTE;
        encode_eip_1559_params(eip_1559_params, default_base_fee_params, &mut extra_data)?;
        extra_data[9..17].copy_from_slice(&min_base_fee.to_be_bytes());
        Ok(Bytes::copy_from_slice(&extra_data))
    }
}

#[cfg(test)]
mod tests {
    use core::str::FromStr;

    use alloy_eips::eip1559::BaseFeeParams;
    use alloy_primitives::{B64, Bytes};

    use super::JovianExtraData;
    use crate::extra::EIP1559ParamError;

    #[test]
    fn test_encode_with_explicit_params() {
        let eip_1559_params = B64::from_str("0x0000000800000008").unwrap();
        let extra_data = JovianExtraData::encode(eip_1559_params, BaseFeeParams::new(80, 60), 257);
        assert_eq!(
            extra_data.unwrap(),
            Bytes::copy_from_slice(&[1, 0, 0, 0, 8, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 1, 1])
        );
    }

    #[test]
    fn test_encode_with_defaults() {
        let extra_data = JovianExtraData::encode(B64::ZERO, BaseFeeParams::new(80, 60), 0);
        assert_eq!(
            extra_data.unwrap(),
            Bytes::copy_from_slice(&[1, 0, 0, 0, 80, 0, 0, 0, 60, 0, 0, 0, 0, 0, 0, 0, 0])
        );
    }

    #[test]
    fn test_decode_invalid_length() {
        let extra_data = [0u8; 8];
        let res = JovianExtraData::decode(&extra_data);
        assert_eq!(res.unwrap_err(), EIP1559ParamError::InvalidExtraDataLength);
    }
}
