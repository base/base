//! ABI definitions for the activation registry precompile.

use alloy_sol_types::sol;

sol! {
    /// Activation registry ABI.
    interface IActivationRegistry {
        /// Emitted when a feature is activated.
        event FeatureActivated(bytes32 indexed feature, address indexed caller);

        /// Emitted when a feature is deactivated.
        event FeatureDeactivated(bytes32 indexed feature, address indexed caller);

        /// Caller is not authorized to activate features.
        error Unauthorized(address caller);

        /// Feature is already activated.
        error AlreadyActivated(bytes32 feature);

        /// Feature is already deactivated.
        error AlreadyDeactivated(bytes32 feature);

        /// Feature is not activated.
        error FeatureNotActivated(bytes32 feature);

        /// Precompile cannot be executed via delegatecall or callcode.
        error DelegateCallNotAllowed();

        /// State-mutating call was attempted in a static context.
        error StaticCallNotAllowed();

        /// Returns true when `feature` is activated.
        function isActivated(bytes32 feature) external view returns (bool);

        /// Reverts with `FeatureNotActivated` if `feature` is not activated.
        function checkActivated(bytes32 feature) external view;

        /// Returns the activation admin.
        function admin() external view returns (address);

        /// Activates `feature`.
        function activate(bytes32 feature) external;

        /// Deactivates `feature`.
        function deactivate(bytes32 feature) external;
    }
}
