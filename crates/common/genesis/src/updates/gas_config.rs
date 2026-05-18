//! The gas config update type.

use alloy_primitives::{LogData, U256};
use alloy_sol_types::{SolType, sol};

use crate::{GasConfigUpdateError, RollupConfig, SystemConfig, SystemConfigLog};

/// The gas config update type.
#[derive(Debug, Default, Clone, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GasConfigUpdate {
    /// The scalar.
    pub scalar: Option<U256>,
    /// The overhead.
    pub overhead: Option<U256>,
}

impl GasConfigUpdate {
    /// Applies the update to the [`SystemConfig`].
    pub const fn apply(&self, config: &mut SystemConfig) {
        if let Some(scalar) = self.scalar {
            config.scalar = scalar;
        }
        if let Some(overhead) = self.overhead {
            config.overhead = overhead;
        }
    }
}

impl TryFrom<&SystemConfigLog> for GasConfigUpdate {
    type Error = GasConfigUpdateError;

    fn try_from(sys_log: &SystemConfigLog) -> Result<Self, Self::Error> {
        let LogData { data, .. } = &sys_log.log.data;
        if data.len() != 128 {
            return Err(GasConfigUpdateError::InvalidDataLen(data.len()));
        }

        let Ok(pointer) = <sol!(uint64)>::abi_decode_validate(&data[0..32]) else {
            return Err(GasConfigUpdateError::PointerDecodingError);
        };
        if pointer != 32 {
            return Err(GasConfigUpdateError::InvalidDataPointer(pointer));
        }

        let Ok(length) = <sol!(uint64)>::abi_decode_validate(&data[32..64]) else {
            return Err(GasConfigUpdateError::LengthDecodingError);
        };
        if length != 64 {
            return Err(GasConfigUpdateError::InvalidDataLength(length));
        }

        let Ok(overhead) = <sol!(uint256)>::abi_decode_validate(&data[64..96]) else {
            return Err(GasConfigUpdateError::OverheadDecodingError);
        };
        let Ok(scalar) = <sol!(uint256)>::abi_decode_validate(&data[96..]) else {
            return Err(GasConfigUpdateError::ScalarDecodingError);
        };

        if sys_log.ecotone_active
            && RollupConfig::check_ecotone_l1_system_config_scalar(scalar.to_be_bytes()).is_err()
        {
            // ignore invalid scalars, retain the old system-config scalar
            return Ok(Self::default());
        }

        // If ecotone is active, set the overhead to zero, otherwise set to the decoded value.
        let overhead = if sys_log.ecotone_active { U256::ZERO } else { overhead };

        Ok(Self { scalar: Some(scalar), overhead: Some(overhead) })
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_primitives::{Address, B256, Bytes, Log, LogData, hex, uint};
    use rstest::rstest;

    use super::*;
    use crate::SystemConfigUpdate;

    #[test]
    fn test_gas_config_update_try_from() {
        let log = Log {
            address: Address::ZERO,
            data: LogData::new_unchecked(
                vec![SystemConfigUpdate::TOPIC, SystemConfigUpdate::EVENT_VERSION_0, B256::ZERO],
                hex!("00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000babe000000000000000000000000000000000000000000000000000000000000beef").into()
            )
        };
        let system_log = SystemConfigLog::new(log, false);
        let update = GasConfigUpdate::try_from(&system_log).unwrap();
        assert_eq!(update.overhead, Some(uint!(0xbabe_U256)));
        assert_eq!(update.scalar, Some(uint!(0xbeef_U256)));
    }

    #[test]
    fn test_gas_config_update_invalid_data_len() {
        let log =
            Log { address: Address::ZERO, data: LogData::new_unchecked(vec![], Bytes::default()) };
        let system_log = SystemConfigLog::new(log, false);
        assert_eq!(
            GasConfigUpdate::try_from(&system_log).unwrap_err(),
            GasConfigUpdateError::InvalidDataLen(0)
        );
    }

    #[rstest]
    #[case(hex!("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF0000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000babe000000000000000000000000000000000000000000000000000000000000beef"), GasConfigUpdateError::PointerDecodingError)]
    #[case(hex!("00000000000000000000000000000000000000000000000000000000000000210000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000babe000000000000000000000000000000000000000000000000000000000000beef"), GasConfigUpdateError::InvalidDataPointer(33))]
    #[case(hex!("0000000000000000000000000000000000000000000000000000000000000020FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000000000000000000000000000000000000000000000000000000000babe000000000000000000000000000000000000000000000000000000000000beef"), GasConfigUpdateError::LengthDecodingError)]
    #[case(hex!("00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000041000000000000000000000000000000000000000000000000000000000000babe000000000000000000000000000000000000000000000000000000000000beef"), GasConfigUpdateError::InvalidDataLength(65))]
    fn test_gas_config_update_errors(
        #[case] data: [u8; 128],
        #[case] expected: GasConfigUpdateError,
    ) {
        let log = Log {
            address: Address::ZERO,
            data: LogData::new_unchecked(
                vec![SystemConfigUpdate::TOPIC, SystemConfigUpdate::EVENT_VERSION_0, B256::ZERO],
                data.into(),
            ),
        };
        let system_log = SystemConfigLog::new(log, false);
        assert_eq!(GasConfigUpdate::try_from(&system_log).unwrap_err(), expected);
    }

    #[rstest]
    #[case(hex!("00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000040FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000000000000000000000000000000000000000000000000000000000beef"))]
    #[case(hex!("00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000babeFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"))]
    fn test_gas_config_update_max_u256_succeeds(#[case] data: [u8; 128]) {
        let log = Log {
            address: Address::ZERO,
            data: LogData::new_unchecked(
                vec![SystemConfigUpdate::TOPIC, SystemConfigUpdate::EVENT_VERSION_0, B256::ZERO],
                data.into(),
            ),
        };
        let system_log = SystemConfigLog::new(log, false);
        assert!(GasConfigUpdate::try_from(&system_log).is_ok());
    }
}
