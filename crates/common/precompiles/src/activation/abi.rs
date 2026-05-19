//! ABI definitions for the activation registry precompile.

use alloy_sol_types::sol;

sol! {
    /// Activation registry ABI.
    interface IActivationRegistry {
        /// Emitted when a feature is enabled.
        event FeatureEnabled(bytes32 indexed feature, address indexed caller);

        /// Emitted when a feature is disabled.
        event FeatureDisabled(bytes32 indexed feature, address indexed caller);

        /// Caller is not authorized to enable features.
        error Unauthorized(address caller);

        /// Feature is already enabled.
        error AlreadyEnabled(bytes32 feature);

        /// Feature is already disabled.
        error AlreadyDisabled(bytes32 feature);

        /// Feature is not enabled.
        error FeatureNotEnabled(bytes32 feature);

        /// Precompile cannot be executed via delegatecall or callcode.
        error DelegateCallNotAllowed();

        /// State-mutating call was attempted in a static context.
        error StaticCallNotAllowed();

        /// Returns true when `feature` is enabled.
        function isEnabled(bytes32 feature) external view returns (bool);

        /// Enables `feature`.
        function enable(bytes32 feature) external;

        /// Disables `feature`.
        function disable(bytes32 feature) external;

        /// Returns the activation admin.
        function activationAdmin() external view returns (address);
    }
}
