//! Activation registry precompile address, feature identifiers, and ABI.

use alloy_primitives::{Address, B256, address, b256};

/// Activation registry precompile address.
pub const ACTIVATION_REGISTRY_ADDRESS: Address =
    address!("8453000000000000000000000000000000000001");

/// Identifies a Base-native precompile feature in the activation registry.
///
/// Each variant maps to a stable `keccak256` hash of the feature's canonical name and is used as
/// the key when querying or mutating activation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationFeature {
    /// `keccak256("base.policy_registry")`
    PolicyRegistry,
    /// `keccak256("base.b20_stablecoin")`
    B20Stablecoin,
    /// `keccak256("base.b20_asset")`
    B20Asset,
}

impl ActivationFeature {
    /// Returns the `keccak256` hash that identifies this feature in storage.
    pub const fn id(self) -> B256 {
        match self {
            Self::PolicyRegistry => {
                b256!("0xb582ebae03f16fee49a6763f78df482fb11ae73f103ed0d330bbe556aa90a43f")
            }
            Self::B20Stablecoin => {
                b256!("0xecfa0def2c10020caaf65e6155aa69c84b24892aaef76eeac52e0e2b3a0b8601")
            }
            Self::B20Asset => {
                b256!("0xcdcc772fe4cbdb1029f822861176d09e646db96723d4c1e82ddfdeb8163ef54c")
            }
        }
    }
}

impl From<ActivationFeature> for B256 {
    fn from(feature: ActivationFeature) -> Self {
        feature.id()
    }
}

#[cfg(feature = "abi")]
mod abi {
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

            /// ETH was attached to a call targeting a nonpayable activation-registry selector.
            error NonPayable();

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
}

#[cfg(feature = "abi")]
pub use abi::IActivationRegistry;
