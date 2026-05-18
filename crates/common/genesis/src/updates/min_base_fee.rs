//! The min base fee update type.

use alloy_primitives::LogData;
use alloy_sol_types::{SolType, sol};

use crate::{SystemConfig, SystemConfigLog, system::MinBaseFeeUpdateError};

/// The min base fee update type.
#[derive(Debug, Default, Clone, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MinBaseFeeUpdate {
    /// The min base fee.
    pub min_base_fee: u64,
}

impl MinBaseFeeUpdate {
    /// Applies the update to the [`SystemConfig`].
    pub const fn apply(&self, config: &mut SystemConfig) {
        config.min_base_fee = Some(self.min_base_fee);
    }
}

impl TryFrom<&SystemConfigLog> for MinBaseFeeUpdate {
    type Error = MinBaseFeeUpdateError;

    fn try_from(log: &SystemConfigLog) -> Result<Self, Self::Error> {
        let LogData { data, .. } = &log.log.data;
        if data.len() != 96 {
            return Err(MinBaseFeeUpdateError::InvalidDataLen(data.len()));
        }

        let Ok(pointer) = <sol!(uint64)>::abi_decode_validate(&data[0..32]) else {
            return Err(MinBaseFeeUpdateError::PointerDecodingError);
        };
        if pointer != 32 {
            return Err(MinBaseFeeUpdateError::InvalidDataPointer(pointer));
        }

        let Ok(length) = <sol!(uint64)>::abi_decode_validate(&data[32..64]) else {
            return Err(MinBaseFeeUpdateError::LengthDecodingError);
        };
        if length != 32 {
            return Err(MinBaseFeeUpdateError::InvalidDataLength(length));
        }

        let Ok(min_base_fee) = <sol!(uint64)>::abi_decode_validate(&data[64..96]) else {
            return Err(MinBaseFeeUpdateError::MinBaseFeeDecodingError);
        };

        Ok(Self { min_base_fee })
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
    fn test_min_base_fee_update_try_from() {
        let log = Log {
            address: Address::ZERO,
            data: LogData::new_unchecked(
                vec![SystemConfigUpdate::TOPIC, SystemConfigUpdate::EVENT_VERSION_0, B256::ZERO],
                hex!("00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000beef").into()
            )
        };
        let system_log = SystemConfigLog::new(log, false);
        assert_eq!(MinBaseFeeUpdate::try_from(&system_log).unwrap().min_base_fee, 0xbeef_u64);
    }

    #[test]
    fn test_min_base_fee_update_invalid_data_len() {
        let log =
            Log { address: Address::ZERO, data: LogData::new_unchecked(vec![], Bytes::default()) };
        let system_log = SystemConfigLog::new(log, false);
        assert_eq!(
            MinBaseFeeUpdate::try_from(&system_log).unwrap_err(),
            MinBaseFeeUpdateError::InvalidDataLen(0)
        );
    }

    #[rstest]
    #[case(hex!("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000babe0000beef"), MinBaseFeeUpdateError::PointerDecodingError)]
    #[case(hex!("000000000000000000000000000000000000000000000000000000000000002100000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000babe0000beef"), MinBaseFeeUpdateError::InvalidDataPointer(33))]
    #[case(hex!("0000000000000000000000000000000000000000000000000000000000000020FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0000000000000000000000000000000000000000000000000000babe0000beef"), MinBaseFeeUpdateError::LengthDecodingError)]
    #[case(hex!("000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000210000000000000000000000000000000000000000000000000000babe0000beef"), MinBaseFeeUpdateError::InvalidDataLength(33))]
    #[case(hex!("00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000020FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"), MinBaseFeeUpdateError::MinBaseFeeDecodingError)]
    fn test_min_base_fee_update_errors(
        #[case] data: [u8; 96],
        #[case] expected: MinBaseFeeUpdateError,
    ) {
        let log = Log {
            address: Address::ZERO,
            data: LogData::new_unchecked(
                vec![SystemConfigUpdate::TOPIC, SystemConfigUpdate::EVENT_VERSION_0, B256::ZERO],
                data.into(),
            ),
        };
        let system_log = SystemConfigLog::new(log, false);
        assert_eq!(MinBaseFeeUpdate::try_from(&system_log).unwrap_err(), expected);
    }
}
