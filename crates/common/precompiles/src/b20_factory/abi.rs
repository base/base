//! ABI definition for the `IB20Factory` interface.

use alloy_sol_types::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IB20Factory {
        // ── Structs ─────────────────────────────────────────────────────────

        enum B20Variant {
            /// Asset B-20 token variant.
            ASSET,
            /// Stablecoin B-20 token variant.
            STABLECOIN
        }

        struct B20StablecoinCreateParams {
            uint8 version;
            string name;
            string symbol;
            address initialAdmin;
            string currency;
        }

        struct B20AssetCreateParams {
            uint8 version;
            string name;
            string symbol;
            address initialAdmin;
            uint8 decimals;
        }

        // ── Errors ───────────────────────────────────────────────────────────

        /// A token already exists at the address derived from `(variant, msg.sender, salt)`.
        error TokenAlreadyExists(address token);

        /// `variant` is not recognized or is `NONE`.
        error InvalidVariant();

        /// `version` is not supported for the requested variant.
        error UnsupportedVersion(uint8 version, B20Variant variant);

        /// A required string argument was empty.
        /// @param field  Name of the missing field (e.g. `"currency"`).
        error MissingRequiredField(string field);

        /// The stablecoin `currency` field was not on the ISO 4217 fiat allowlist.
        error InvalidCurrency(string code);

        /// The asset `decimals` field was outside the allowed range.
        error InvalidDecimals(uint8 decimals);

        /// One of the post-creation init calls failed.
        error InitCallFailed(uint256 index);

        // ── Events ───────────────────────────────────────────────────────────

        event B20Created(
            address indexed token,
            B20Variant indexed variant,
            string name,
            string symbol,
            uint8 decimals,
            bytes variantParams
        );

        /// ABI-encoded payload for the `variantParams` field of `B20Created`
        /// when variant is `STABLECOIN`.
        struct B20StablecoinEventParams {
            uint8 version;
            string currency;
        }

        // ── Functions ────────────────────────────────────────────────────────

        /// Creates a B-20 token of the requested variant at a deterministic address.
        ///
        /// Default tokens start with an unbounded supply cap and the pausable plus mutable-cap
        /// capability bits enabled. Callers configure optional launch state atomically through
        /// `initCalls`, such as minting initial supply, lowering the supply cap, pausing, or setting
        /// metadata.
        function createB20(
            B20Variant variant,
            bytes32 salt,
            bytes calldata params,
            bytes[] calldata initCalls
        ) external returns (address token);

        /// Returns the address a `createB20` call would produce.
        function getB20Address(B20Variant variant, address sender, bytes32 salt) external view returns (address);

        /// Returns `true` if `token` has the B-20 address prefix.
        function isB20(address token) external view returns (bool);

        /// Returns `true` if `token` has been initialized by this factory.
        function isB20Initialized(address token) external view returns (bool);
    }
}

impl IB20Factory::IB20FactoryCalls {
    /// Returns the stable metric label for this decoded factory call.
    pub const fn as_label(&self) -> &'static str {
        match self {
            Self::createB20(_) => "factory.createB20",
            Self::getB20Address(_) => "factory.getB20Address",
            Self::isB20(_) => "factory.isB20",
            Self::isB20Initialized(_) => "factory.isB20Initialized",
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256};

    use crate::IB20Factory;

    #[test]
    fn factory_call_labels_are_stable() {
        assert_eq!(
            IB20Factory::IB20FactoryCalls::getB20Address(IB20Factory::getB20AddressCall {
                variant: IB20Factory::B20Variant::ASSET,
                sender: Address::ZERO,
                salt: B256::ZERO,
            })
            .as_label(),
            "factory.getB20Address"
        );
    }
}
