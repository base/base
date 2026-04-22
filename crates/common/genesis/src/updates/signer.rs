//! The unsafe block signer update.

use alloy_primitives::{Address, LogData};
use alloy_sol_types::{SolType, sol};

use crate::{SystemConfigLog, UnsafeBlockSignerUpdateError, UpdateDataValidator, ValidationError};

/// The unsafe block signer update type.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct UnsafeBlockSignerUpdate {
    /// The new unsafe block signer address.
    pub unsafe_block_signer: Address,
}

impl TryFrom<&SystemConfigLog> for UnsafeBlockSignerUpdate {
    type Error = UnsafeBlockSignerUpdateError;

    fn try_from(log: &SystemConfigLog) -> Result<Self, Self::Error> {
        let LogData { data, .. } = &log.log.data;

        let validated = UpdateDataValidator::validate(data).map_err(|e| match e {
            ValidationError::InvalidDataLen(_expected, actual) => {
                UnsafeBlockSignerUpdateError::InvalidDataLen(actual)
            }
            ValidationError::PointerDecodingError => {
                UnsafeBlockSignerUpdateError::PointerDecodingError
            }
            ValidationError::InvalidDataPointer(pointer) => {
                UnsafeBlockSignerUpdateError::InvalidDataPointer(pointer)
            }
            ValidationError::LengthDecodingError => {
                UnsafeBlockSignerUpdateError::LengthDecodingError
            }
            ValidationError::InvalidDataLength(length) => {
                UnsafeBlockSignerUpdateError::InvalidDataLength(length)
            }
        })?;

        let Ok(unsafe_block_signer) = <sol!(address)>::abi_decode_validate(validated.payload())
        else {
            return Err(UnsafeBlockSignerUpdateError::UnsafeBlockSignerAddressDecodingError);
        };

        Ok(Self { unsafe_block_signer })
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
    fn test_signer_update_try_from() {
        let log = Log {
            address: Address::ZERO,
            data: LogData::new_unchecked(
                vec![SystemConfigUpdate::TOPIC, SystemConfigUpdate::EVENT_VERSION_0, B256::ZERO],
                hex!("00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000beef").into()
            )
        };
        let system_log = SystemConfigLog::new(log, false);
        let update = UnsafeBlockSignerUpdate::try_from(&system_log).unwrap();
        assert_eq!(
            update.unsafe_block_signer,
            address!("000000000000000000000000000000000000bEEF")
        );
    }

    #[test]
    fn test_signer_update_invalid_data_len() {
        let log =
            Log { address: Address::ZERO, data: LogData::new_unchecked(vec![], Bytes::default()) };
        let system_log = SystemConfigLog::new(log, false);
        assert_eq!(
            UnsafeBlockSignerUpdate::try_from(&system_log).unwrap_err(),
            UnsafeBlockSignerUpdateError::InvalidDataLen(0)
        );
    }

    #[rstest]
    #[case(hex!("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000babe0000beef"), UnsafeBlockSignerUpdateError::PointerDecodingError)]
    #[case(hex!("000000000000000000000000000000000000000000000000000000000000002100000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000babe0000beef"), UnsafeBlockSignerUpdateError::InvalidDataPointer(33))]
    #[case(hex!("0000000000000000000000000000000000000000000000000000000000000020FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0000000000000000000000000000000000000000000000000000babe0000beef"), UnsafeBlockSignerUpdateError::LengthDecodingError)]
    #[case(hex!("000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000210000000000000000000000000000000000000000000000000000babe0000beef"), UnsafeBlockSignerUpdateError::InvalidDataLength(33))]
    #[case(hex!("00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000020FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"), UnsafeBlockSignerUpdateError::UnsafeBlockSignerAddressDecodingError)]
    fn test_signer_update_errors(
        #[case] data: [u8; 96],
        #[case] expected: UnsafeBlockSignerUpdateError,
    ) {
        let log = Log {
            address: Address::ZERO,
            data: LogData::new_unchecked(
                vec![SystemConfigUpdate::TOPIC, SystemConfigUpdate::EVENT_VERSION_0, B256::ZERO],
                data.into(),
            ),
        };
        let system_log = SystemConfigLog::new(log, false);
        assert_eq!(UnsafeBlockSignerUpdate::try_from(&system_log).unwrap_err(), expected);
    }
}
