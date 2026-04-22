//! The Operator Fee update type.

use alloy_primitives::LogData;

use crate::{
    OperatorFeeUpdateError, SystemConfig, SystemConfigLog, UpdateDataValidator, ValidationError,
};

/// The Operator Fee update type.
#[derive(Debug, Default, Clone, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OperatorFeeUpdate {
    /// The operator fee scalar.
    pub operator_fee_scalar: u32,
    /// The operator fee constant.
    pub operator_fee_constant: u64,
}

impl OperatorFeeUpdate {
    /// Applies the update to the [`SystemConfig`].
    pub const fn apply(&self, config: &mut SystemConfig) {
        config.operator_fee_scalar = Some(self.operator_fee_scalar);
        config.operator_fee_constant = Some(self.operator_fee_constant);
    }
}

impl TryFrom<&SystemConfigLog> for OperatorFeeUpdate {
    type Error = OperatorFeeUpdateError;

    fn try_from(log: &SystemConfigLog) -> Result<Self, Self::Error> {
        let LogData { data, .. } = &log.log.data;

        let validated = UpdateDataValidator::validate(data).map_err(|e| match e {
            ValidationError::InvalidDataLen(_expected, actual) => {
                OperatorFeeUpdateError::InvalidDataLen(actual)
            }
            ValidationError::PointerDecodingError => OperatorFeeUpdateError::PointerDecodingError,
            ValidationError::InvalidDataPointer(pointer) => {
                OperatorFeeUpdateError::InvalidDataPointer(pointer)
            }
            ValidationError::LengthDecodingError => OperatorFeeUpdateError::LengthDecodingError,
            ValidationError::InvalidDataLength(length) => {
                OperatorFeeUpdateError::InvalidDataLength(length)
            }
        })?;

        // The operator fee scalar and constant are
        // packed into a single u256 as follows:
        //
        // | Bytes    | Actual Size | Variable |
        // |----------|-------------|----------|
        // | 0 .. 24  | uint32      | scalar   |
        // | 24 .. 32 | uint64      | constant |
        // |----------|-------------|----------|

        let payload = validated.payload();
        let mut be_bytes = [0u8; 4];
        be_bytes[0..4].copy_from_slice(&payload[20..24]);
        let operator_fee_scalar = u32::from_be_bytes(be_bytes);

        let mut be_bytes = [0u8; 8];
        be_bytes[0..8].copy_from_slice(&payload[24..32]);
        let operator_fee_constant = u64::from_be_bytes(be_bytes);

        Ok(Self { operator_fee_scalar, operator_fee_constant })
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
    fn test_operator_fee_update_try_from() {
        let log = Log {
            address: Address::ZERO,
            data: LogData::new_unchecked(
                vec![], // Topics aren't checked
                hex!("0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000babe000000000000beef").into()
            )
        };
        let system_log = SystemConfigLog::new(log, false);
        let update = OperatorFeeUpdate::try_from(&system_log).unwrap();
        assert_eq!(update.operator_fee_scalar, 0xbabe_u32);
        assert_eq!(update.operator_fee_constant, 0xbeef_u64);
    }

    #[test]
    fn test_operator_fee_update_invalid_data_len() {
        let log =
            Log { address: Address::ZERO, data: LogData::new_unchecked(vec![], Bytes::default()) };
        let system_log = SystemConfigLog::new(log, false);
        assert_eq!(
            OperatorFeeUpdate::try_from(&system_log).unwrap_err(),
            OperatorFeeUpdateError::InvalidDataLen(0)
        );
    }

    #[rstest]
    #[case(hex!("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000babe0000beef"), OperatorFeeUpdateError::PointerDecodingError)]
    #[case(hex!("000000000000000000000000000000000000000000000000000000000000002100000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000babe0000beef"), OperatorFeeUpdateError::InvalidDataPointer(33))]
    #[case(hex!("0000000000000000000000000000000000000000000000000000000000000020FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0000000000000000000000000000000000000000000000000000babe0000beef"), OperatorFeeUpdateError::LengthDecodingError)]
    #[case(hex!("000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000210000000000000000000000000000000000000000000000000000babe0000beef"), OperatorFeeUpdateError::InvalidDataLength(33))]
    fn test_operator_fee_update_errors(
        #[case] data: [u8; 96],
        #[case] expected: OperatorFeeUpdateError,
    ) {
        let log = Log {
            address: Address::ZERO,
            data: LogData::new_unchecked(
                vec![SystemConfigUpdate::TOPIC, SystemConfigUpdate::EVENT_VERSION_0, B256::ZERO],
                data.into(),
            ),
        };
        let system_log = SystemConfigLog::new(log, false);
        assert_eq!(OperatorFeeUpdate::try_from(&system_log).unwrap_err(), expected);
    }
}
