use alloy_primitives::{Address, B256, Bytes};
use alloy_sol_types::SolCall;
use base_common_precompiles::{ActivationRegistryStorage, IActivationRegistry};

use crate::{CalldataAction, FeatureName};

/// Encoded transaction target and calldata for an activation registry mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalldataOutput {
    /// Activation registry method that was encoded.
    pub action: CalldataAction,
    /// Feature targeted by the encoded method.
    pub feature: FeatureName,
    /// Transaction recipient.
    pub to: Address,
    /// Encoded calldata.
    pub data: Bytes,
    /// Activation registry feature ID.
    pub feature_id: B256,
}

/// Encodes activation registry calldata.
#[derive(Debug, Clone, Copy)]
pub struct CalldataEncoder;

impl CalldataEncoder {
    /// Encodes calldata for `action(feature)`.
    pub fn encode(action: CalldataAction, feature: FeatureName) -> CalldataOutput {
        let feature_id = feature.activation_feature().id();
        let data = match action {
            CalldataAction::Activate => {
                IActivationRegistry::activateCall { feature: feature_id }.abi_encode()
            }
            CalldataAction::Deactivate => {
                IActivationRegistry::deactivateCall { feature: feature_id }.abi_encode()
            }
        };

        CalldataOutput {
            action,
            feature,
            to: ActivationRegistryStorage::ADDRESS,
            data: Bytes::from(data),
            feature_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_sol_types::SolCall;
    use base_common_precompiles::{ActivationFeature, IActivationRegistry};

    use super::*;

    #[test]
    fn encode_activate_uses_generated_activation_registry_abi() {
        let output = CalldataEncoder::encode(CalldataAction::Activate, FeatureName::B20Asset);
        let feature = ActivationFeature::B20Asset.id();

        assert_eq!(output.to, ActivationRegistryStorage::ADDRESS);
        assert_eq!(output.feature_id, feature);
        assert_eq!(
            output.data,
            Bytes::from(IActivationRegistry::activateCall { feature }.abi_encode())
        );
    }

    #[test]
    fn encode_deactivate_uses_generated_activation_registry_abi() {
        let output =
            CalldataEncoder::encode(CalldataAction::Deactivate, FeatureName::PolicyRegistry);
        let feature = ActivationFeature::PolicyRegistry.id();

        assert_eq!(output.to, ActivationRegistryStorage::ADDRESS);
        assert_eq!(output.feature_id, feature);
        assert_eq!(
            output.data,
            Bytes::from(IActivationRegistry::deactivateCall { feature }.abi_encode())
        );
    }
}
