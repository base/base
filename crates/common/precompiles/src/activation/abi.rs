//! ABI definitions for the activation registry precompile.

use alloy_sol_types::sol;

sol! {
    /// Activation registry ABI.
    interface IActivationRegistry {
        /// Emitted when a feature is activated.
        event FeatureActivated(bytes32 indexed feature, address indexed caller);

        /// Emitted when a feature is deactivated.
        event FeatureDeactivated(bytes32 indexed feature, address indexed caller);

        /// Emitted when the activation admin changes.
        event AdminChanged(
            address indexed previousAdmin,
            address indexed newAdmin,
            address indexed caller
        );

        /// Caller is not authorized to activate features.
        error Unauthorized(address caller);

        /// Feature is already activated.
        error AlreadyActivated(bytes32 feature);

        /// Feature is not activated.
        error FeatureNotActivated(bytes32 feature);

        /// Precompile cannot be executed via delegatecall or callcode.
        error DelegateCallNotAllowed();

        /// State-mutating call was attempted in a static context.
        error StaticCallNotAllowed();

        /// State-backed admin storage is not active for this fork.
        error AdminStorageNotEnabled();

        /// The new admin address is zero.
        error ZeroAdminAddress();

        /// Returns true when `feature` is activated.
        function isActivated(bytes32 feature) external view returns (bool);

        /// Reverts with `FeatureNotActivated` if `feature` is not activated.
        function checkActivated(bytes32 feature) external view;

        /// Returns the activation admin.
        function admin() external view returns (address);

        /// Sets the activation admin.
        function setAdmin(address newAdmin) external;

        /// Activates `feature`.
        function activate(bytes32 feature) external;

        /// Deactivates `feature`.
        function deactivate(bytes32 feature) external;
    }
}

impl IActivationRegistry::IActivationRegistryCalls {
    /// Returns the stable metric label for this decoded activation-registry call.
    pub const fn as_label(&self) -> &'static str {
        match self {
            Self::isActivated(_) => "activation.isActivated",
            Self::checkActivated(_) => "activation.checkActivated",
            Self::admin(_) => "activation.admin",
            Self::setAdmin(_) => "activation.setAdmin",
            Self::activate(_) => "activation.activate",
            Self::deactivate(_) => "activation.deactivate",
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::IActivationRegistry;

    #[test]
    fn activation_call_labels_are_stable() {
        assert_eq!(
            IActivationRegistry::IActivationRegistryCalls::admin(IActivationRegistry::adminCall {})
                .as_label(),
            "activation.admin"
        );
    }
}
