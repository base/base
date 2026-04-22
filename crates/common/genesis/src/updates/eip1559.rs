//! The EIP-1559 update type.

use alloy_primitives::LogData;
use alloy_sol_types::{SolType, sol};

use crate::{
    EIP1559UpdateError, SystemConfig, SystemConfigLog, UpdateDataValidator, ValidationError,
};

/// The EIP-1559 update type.
#[derive(Debug, Default, Clone, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Eip1559Update {
    /// The EIP-1559 denominator.
    pub eip1559_denominator: u32,
    /// The EIP-1559 elasticity multiplier.
    pub eip1559_elasticity: u32,
}

impl Eip1559Update {
    /// Applies the update to the [`SystemConfig`].
    pub const fn apply(&self, config: &mut SystemConfig) {
        config.eip1559_denominator = Some(self.eip1559_denominator);
        config.eip1559_elasticity = Some(self.eip1559_elasticity);
    }
}

impl TryFrom<&SystemConfigLog> for Eip1559Update {
    type Error = EIP1559UpdateError;

    fn try_from(log: &SystemConfigLog) -> Result<Self, Self::Error> {
        let LogData { data, .. } = &log.log.data;

        let validated = UpdateDataValidator::validate(data).map_err(|e| match e {
            ValidationError::InvalidDataLen(_expected, actual) => {
                EIP1559UpdateError::InvalidDataLen(actual)
            }
            ValidationError::PointerDecodingError => EIP1559UpdateError::PointerDecodingError,
            ValidationError::InvalidDataPointer(pointer) => {
                EIP1559UpdateError::InvalidDataPointer(pointer)
            }
            ValidationError::LengthDecodingError => EIP1559UpdateError::LengthDecodingError,
            ValidationError::InvalidDataLength(length) => {
                EIP1559UpdateError::InvalidDataLength(length)
            }
        })?;

        let Ok(eip1559_params) = <sol!(uint64)>::abi_decode_validate(validated.payload()) else {
            return Err(EIP1559UpdateError::EIP1559DecodingError);
        };

        Ok(Self {
            eip1559_denominator: (eip1559_params >> 32) as u32,
            eip1559_elasticity: eip1559_params as u32,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_primitives::{Address, B256, Bytes, Log, LogData, hex};
    use rstest::rstest;

    use super::*;
    use crate::SystemConfigUpdate;

    #[test]
    fn test_eip1559_update_try_from() {
        let log = Log {
            address: Address::ZERO,
            data: LogData::new_unchecked(
                vec![SystemConfigUpdate::TOPIC, SystemConfigUpdate::EVENT_VERSION_0, B256::ZERO],
                hex!("000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000babe0000beef").into()
            )
        };
        let system_log = SystemConfigLog::new(log, false);
        let update = Eip1559Update::try_from(&system_log).unwrap();
        assert_eq!(update.eip1559_denominator, 0xbabe_u32);
        assert_eq!(update.eip1559_elasticity, 0xbeef_u32);
    }

    #[test]
    fn test_eip1559_update_invalid_data_len() {
        let log =
            Log { address: Address::ZERO, data: LogData::new_unchecked(vec![], Bytes::default()) };
        let system_log = SystemConfigLog::new(log, false);
        assert_eq!(
            Eip1559Update::try_from(&system_log).unwrap_err(),
            EIP1559UpdateError::InvalidDataLen(0)
        );
    }

    #[rstest]
    #[case(hex!("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000babe0000beef"), EIP1559UpdateError::PointerDecodingError)]
    #[case(hex!("000000000000000000000000000000000000000000000000000000000000002100000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000babe0000beef"), EIP1559UpdateError::InvalidDataPointer(33))]
    #[case(hex!("0000000000000000000000000000000000000000000000000000000000000020FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0000000000000000000000000000000000000000000000000000babe0000beef"), EIP1559UpdateError::LengthDecodingError)]
    #[case(hex!("000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000210000000000000000000000000000000000000000000000000000babe0000beef"), EIP1559UpdateError::InvalidDataLength(33))]
    #[case(hex!("00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000020FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"), EIP1559UpdateError::EIP1559DecodingError)]
    fn test_eip1559_update_errors(#[case] data: [u8; 96], #[case] expected: EIP1559UpdateError) {
        let log = Log {
            address: Address::ZERO,
            data: LogData::new_unchecked(
                vec![SystemConfigUpdate::TOPIC, SystemConfigUpdate::EVENT_VERSION_0, B256::ZERO],
                data.into(),
            ),
        };
        let system_log = SystemConfigLog::new(log, false);
        assert_eq!(Eip1559Update::try_from(&system_log).unwrap_err(), expected);
    }
}
