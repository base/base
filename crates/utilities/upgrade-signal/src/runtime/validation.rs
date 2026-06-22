//! Runtime upgrade signal validation.

use alloy_primitives::Address;
use base_common_genesis::BaseUpgrade;

use crate::{UpgradeSignalError, UpgradeSignalSchedule};

/// Runtime schedule validation context shared by execution and consensus refresh paths.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct UpgradeSignalRuntimeValidation {
    /// Whether positive Beryl signals require an execution activation admin address.
    pub require_activation_admin_for_beryl: bool,
    /// Execution activation admin address for the L2 chain, when known.
    pub activation_admin_address: Option<Address>,
}

impl UpgradeSignalRuntimeValidation {
    /// Creates a validation context with execution-specific checks disabled.
    pub const fn disabled() -> Self {
        Self { require_activation_admin_for_beryl: false, activation_admin_address: None }
    }

    /// Creates a validation context that enforces execution activation admin invariants.
    pub const fn with_activation_admin_address(activation_admin_address: Option<Address>) -> Self {
        Self { require_activation_admin_for_beryl: true, activation_admin_address }
    }

    /// Creates the fail-closed validation context used when no activation admin source is known.
    ///
    /// This requires an activation admin address for positive Beryl signals but has none, so a
    /// positive Beryl signal is rejected rather than applied unguarded.
    pub const fn fail_closed() -> Self {
        Self::with_activation_admin_address(None)
    }

    /// Validates a schedule before it mutates the process-local runtime registry.
    pub fn validate_schedule(
        &self,
        chain_id: u64,
        schedule: &UpgradeSignalSchedule,
    ) -> Result<(), UpgradeSignalError> {
        if self.require_activation_admin_for_beryl
            && !matches!(self.activation_admin_address, Some(address) if address != Address::ZERO)
            && schedule.signals.iter().any(|signal| {
                signal.positive_activation_timestamp().is_some()
                    && signal.hardfork_id == BaseUpgrade::Beryl
            })
        {
            return Err(UpgradeSignalError::missing_activation_admin_address(chain_id));
        }

        Ok(())
    }
}

impl Default for UpgradeSignalRuntimeValidation {
    fn default() -> Self {
        Self::disabled()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, address};

    use super::*;
    use crate::{UpgradeSignal, UpgradeSignalSchedule};

    fn beryl_schedule(timestamp: u64) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(vec![UpgradeSignal {
            hardfork_id: BaseUpgrade::Beryl,
            activation_timestamp: timestamp,
            protocol_version: U256::from(7),
            l1_block_number: 1,
        }])
    }

    #[test]
    fn rejects_positive_beryl_schedule_without_activation_admin() {
        let validation = UpgradeSignalRuntimeValidation::with_activation_admin_address(None);

        assert!(matches!(
            validation.validate_schedule(1, &beryl_schedule(42)),
            Err(UpgradeSignalError::MissingActivationAdminAddress { chain_id: 1 })
        ));
    }

    #[test]
    fn rejects_positive_beryl_schedule_with_zero_activation_admin() {
        let validation =
            UpgradeSignalRuntimeValidation::with_activation_admin_address(Some(Address::ZERO));

        assert!(matches!(
            validation.validate_schedule(1, &beryl_schedule(42)),
            Err(UpgradeSignalError::MissingActivationAdminAddress { chain_id: 1 })
        ));
    }

    #[test]
    fn accepts_positive_beryl_schedule_with_nonzero_activation_admin() {
        let validation = UpgradeSignalRuntimeValidation::with_activation_admin_address(Some(
            address!("0000000000000000000000000000000000000001"),
        ));

        assert!(validation.validate_schedule(1, &beryl_schedule(42)).is_ok());
    }

    #[test]
    fn accepts_zero_beryl_schedule_without_activation_admin() {
        let validation = UpgradeSignalRuntimeValidation::with_activation_admin_address(None);

        assert!(validation.validate_schedule(1, &beryl_schedule(0)).is_ok());
    }
}
