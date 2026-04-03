use alloy_eips::eip1559::BaseFeeParams;
use alloy_primitives::{B64, Bytes};

use super::{EIP1559ParamError, encode_eip_1559_params};

const VERSION_BYTE: u8 = 0;

/// EIP-1559 extra data encoding for the Holocene hardfork.
///
/// Holocene extra data is 9 bytes:
/// - 1 byte version (always `0`)
/// - 4 bytes `max_change_denominator` (big-endian u32)
/// - 4 bytes `elasticity_multiplier` (big-endian u32)
///
/// See: <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/holocene/exec-engine.md#eip1559params-encoding>
#[derive(Debug)]
pub struct HoloceneExtraData;

impl HoloceneExtraData {
    /// Extracts the Holocene EIP-1559 parameters from the encoded [`B64`] form.
    ///
    /// Returns (`elasticity`, `denominator`).
    pub fn decode_params(eip_1559_params: B64) -> (u32, u32) {
        let denominator: [u8; 4] = eip_1559_params.0[..4].try_into().expect("sufficient length");
        let elasticity: [u8; 4] = eip_1559_params.0[4..8].try_into().expect("sufficient length");
        (u32::from_be_bytes(elasticity), u32::from_be_bytes(denominator))
    }

    /// Decodes the EIP-1559 parameters from Holocene `extra_data` bytes.
    ///
    /// Expects exactly 9 bytes with a leading version byte of `0`.
    ///
    /// Returns (`elasticity`, `denominator`).
    pub fn decode(extra_data: &[u8]) -> Result<(u32, u32), EIP1559ParamError> {
        if extra_data.len() != 9 {
            return Err(EIP1559ParamError::InvalidExtraDataLength);
        }
        if extra_data[0] != VERSION_BYTE {
            return Err(EIP1559ParamError::InvalidVersion(extra_data[0]));
        }
        Ok(Self::decode_params(B64::from_slice(&extra_data[1..9])))
    }

    /// Encodes the EIP-1559 parameters into Holocene `extra_data` bytes.
    ///
    /// Produces a 9-byte encoding with a leading version byte of `0`.
    pub fn encode(
        eip_1559_params: B64,
        default_base_fee_params: BaseFeeParams,
    ) -> Result<Bytes, EIP1559ParamError> {
        let mut extra_data = [0u8; 9];
        encode_eip_1559_params(eip_1559_params, default_base_fee_params, &mut extra_data)?;
        Ok(Bytes::copy_from_slice(&extra_data))
    }
}

#[cfg(test)]
mod tests {
    use core::str::FromStr;

    use alloy_eips::eip1559::BaseFeeParams;
    use alloy_primitives::{B64, Bytes};

    use super::HoloceneExtraData;
    use crate::extra::{EIP1559ParamError, JovianExtraData};

    #[test]
    fn test_encode_with_explicit_params() {
        let eip_1559_params = B64::from_str("0x0000000800000008").unwrap();
        let extra_data = HoloceneExtraData::encode(eip_1559_params, BaseFeeParams::new(80, 60));
        assert_eq!(extra_data.unwrap(), Bytes::copy_from_slice(&[0, 0, 0, 0, 8, 0, 0, 0, 8]));
    }

    #[test]
    fn test_encode_with_defaults() {
        let extra_data = HoloceneExtraData::encode(B64::ZERO, BaseFeeParams::new(80, 60));
        assert_eq!(extra_data.unwrap(), Bytes::copy_from_slice(&[0, 0, 0, 0, 80, 0, 0, 0, 60]));
    }

    #[test]
    fn test_decode_invalid_length_short() {
        let extra_data = HoloceneExtraData::encode(B64::ZERO, BaseFeeParams::new(80, 60)).unwrap();
        let res = HoloceneExtraData::decode(&extra_data[..8]).unwrap_err();
        assert_eq!(res, EIP1559ParamError::InvalidExtraDataLength);
    }

    #[test]
    fn test_decode_rejects_jovian_extra_data() {
        let extra_data = JovianExtraData::encode(B64::ZERO, BaseFeeParams::new(80, 60), 0).unwrap();
        let res = HoloceneExtraData::decode(&extra_data).unwrap_err();
        assert_eq!(res, EIP1559ParamError::InvalidExtraDataLength);
    }
}
