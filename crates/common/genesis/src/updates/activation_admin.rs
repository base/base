//! The activation admin update type.

use alloy_primitives::{Address, LogData};
use alloy_sol_types::{SolType, sol};

use crate::{
    ActivationAdminUpdateError, SystemConfig, SystemConfigLog, UpdateDataValidator, ValidationError,
};

/// The activation admin update type.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ActivationAdminUpdate {
    /// The activation registry admin address.
    pub activation_admin_address: Address,
}

impl ActivationAdminUpdate {
    /// Applies the update to the [`SystemConfig`].
    pub const fn apply(&self, config: &mut SystemConfig) {
        config.activation_admin_address = Some(self.activation_admin_address);
    }
}

impl TryFrom<&SystemConfigLog> for ActivationAdminUpdate {
    type Error = ActivationAdminUpdateError;

    fn try_from(log: &SystemConfigLog) -> Result<Self, Self::Error> {
        let LogData { data, .. } = &log.log.data;

        let validated = UpdateDataValidator::validate(data).map_err(|e| match e {
            ValidationError::InvalidDataLen(_expected, actual) => {
                ActivationAdminUpdateError::InvalidDataLen(actual)
            }
            ValidationError::PointerDecodingError => {
                ActivationAdminUpdateError::PointerDecodingError
            }
            ValidationError::InvalidDataPointer(pointer) => {
                ActivationAdminUpdateError::InvalidDataPointer(pointer)
            }
            ValidationError::LengthDecodingError => ActivationAdminUpdateError::LengthDecodingError,
            ValidationError::InvalidDataLength(length) => {
                ActivationAdminUpdateError::InvalidDataLength(length)
            }
        })?;

        let Ok(activation_admin_address) =
            <sol!(address)>::abi_decode_validate(validated.payload())
        else {
            return Err(ActivationAdminUpdateError::ActivationAdminAddressDecodingError);
        };

        Ok(Self { activation_admin_address })
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
    fn test_activation_admin_update_try_from() {
        let log = Log {
            address: Address::ZERO,
            data: LogData::new_unchecked(
                vec![
                    SystemConfigUpdate::TOPIC,
                    SystemConfigUpdate::EVENT_VERSION_0,
                    B256::ZERO,
                ],
                hex!("00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000beef").into(),
            ),
        };
        let system_log = SystemConfigLog::new(log, false);
        let update = ActivationAdminUpdate::try_from(&system_log).unwrap();
        assert_eq!(
            update.activation_admin_address,
            address!("000000000000000000000000000000000000bEEF")
        );
    }

    #[test]
    fn test_activation_admin_update_invalid_data_len() {
        let log =
            Log { address: Address::ZERO, data: LogData::new_unchecked(vec![], Bytes::default()) };
        let system_log = SystemConfigLog::new(log, false);
        assert_eq!(
            ActivationAdminUpdate::try_from(&system_log).unwrap_err(),
            ActivationAdminUpdateError::InvalidDataLen(0)
        );
    }

    #[rstest]
    #[case(hex!("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000babe0000beef"), ActivationAdminUpdateError::PointerDecodingError)]
    #[case(hex!("000000000000000000000000000000000000000000000000000000000000002100000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000babe0000beef"), ActivationAdminUpdateError::InvalidDataPointer(33))]
    #[case(hex!("0000000000000000000000000000000000000000000000000000000000000020FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0000000000000000000000000000000000000000000000000000babe0000beef"), ActivationAdminUpdateError::LengthDecodingError)]
    #[case(hex!("000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000210000000000000000000000000000000000000000000000000000babe0000beef"), ActivationAdminUpdateError::InvalidDataLength(33))]
    #[case(hex!("00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000020FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"), ActivationAdminUpdateError::ActivationAdminAddressDecodingError)]
    fn test_activation_admin_update_errors(
        #[case] data: [u8; 96],
        #[case] expected: ActivationAdminUpdateError,
    ) {
        let log = Log {
            address: Address::ZERO,
            data: LogData::new_unchecked(
                vec![SystemConfigUpdate::TOPIC, SystemConfigUpdate::EVENT_VERSION_0, B256::ZERO],
                data.into(),
            ),
        };
        let system_log = SystemConfigLog::new(log, false);
        assert_eq!(ActivationAdminUpdate::try_from(&system_log).unwrap_err(), expected);
    }
}
