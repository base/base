//! The batcher update type.

use alloy_primitives::{Address, LogData};
use alloy_sol_types::{SolType, sol};

use crate::{
    BatcherUpdateError, SystemConfig, SystemConfigLog, UpdateDataValidator, ValidationError,
};

/// The batcher update type.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BatcherUpdate {
    /// The batcher address.
    pub batcher_address: Address,
}

impl BatcherUpdate {
    /// Applies the update to the [`SystemConfig`].
    pub const fn apply(&self, config: &mut SystemConfig) {
        config.batcher_address = self.batcher_address;
    }
}

impl TryFrom<&SystemConfigLog> for BatcherUpdate {
    type Error = BatcherUpdateError;

    fn try_from(log: &SystemConfigLog) -> Result<Self, Self::Error> {
        let LogData { data, .. } = &log.log.data;

        let validated = UpdateDataValidator::validate(data).map_err(|e| match e {
            ValidationError::InvalidDataLen(_expected, actual) => {
                BatcherUpdateError::InvalidDataLen(actual)
            }
            ValidationError::PointerDecodingError => BatcherUpdateError::PointerDecodingError,
            ValidationError::InvalidDataPointer(pointer) => {
                BatcherUpdateError::InvalidDataPointer(pointer)
            }
            ValidationError::LengthDecodingError => BatcherUpdateError::LengthDecodingError,
            ValidationError::InvalidDataLength(length) => {
                BatcherUpdateError::InvalidDataLength(length)
            }
        })?;

        let Ok(batcher_address) = <sol!(address)>::abi_decode_validate(validated.payload()) else {
            return Err(BatcherUpdateError::BatcherAddressDecodingError);
        };

        Ok(Self { batcher_address })
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_primitives::{B256, Bytes, Log, LogData, address, hex};
    use rstest::rstest;

    use super::*;
    use crate::SystemConfigUpdate;

    #[test]
    fn test_batcher_update_try_from() {
        let log = Log {
            address: Address::ZERO,
            data: LogData::new_unchecked(
                vec![SystemConfigUpdate::TOPIC, SystemConfigUpdate::EVENT_VERSION_0, B256::ZERO],
                hex!("00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000beef").into()
            )
        };
        let system_log = SystemConfigLog::new(log, false);
        let update = BatcherUpdate::try_from(&system_log).unwrap();
        assert_eq!(update.batcher_address, address!("000000000000000000000000000000000000bEEF"));
    }

    #[test]
    fn test_batcher_update_invalid_data_len() {
        let log =
            Log { address: Address::ZERO, data: LogData::new_unchecked(vec![], Bytes::default()) };
        let system_log = SystemConfigLog::new(log, false);
        assert_eq!(
            BatcherUpdate::try_from(&system_log).unwrap_err(),
            BatcherUpdateError::InvalidDataLen(0)
        );
    }

    #[rstest]
    #[case(hex!("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000babe0000beef"), BatcherUpdateError::PointerDecodingError)]
    #[case(hex!("000000000000000000000000000000000000000000000000000000000000002100000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000babe0000beef"), BatcherUpdateError::InvalidDataPointer(33))]
    #[case(hex!("0000000000000000000000000000000000000000000000000000000000000020FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0000000000000000000000000000000000000000000000000000babe0000beef"), BatcherUpdateError::LengthDecodingError)]
    #[case(hex!("000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000210000000000000000000000000000000000000000000000000000babe0000beef"), BatcherUpdateError::InvalidDataLength(33))]
    #[case(hex!("00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000020FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"), BatcherUpdateError::BatcherAddressDecodingError)]
    fn test_batcher_update_errors(#[case] data: [u8; 96], #[case] expected: BatcherUpdateError) {
        let log = Log {
            address: Address::ZERO,
            data: LogData::new_unchecked(
                vec![SystemConfigUpdate::TOPIC, SystemConfigUpdate::EVENT_VERSION_0, B256::ZERO],
                data.into(),
            ),
        };
        let system_log = SystemConfigLog::new(log, false);
        assert_eq!(BatcherUpdate::try_from(&system_log).unwrap_err(), expected);
    }
}
