//! The `IB20Factory` wire surface frozen at Beryl, the fork where the factory activates. Also the
//! canonical live surface, re-exported unqualified by [`super`].
//! A new wire surface goes in a new `abi/vN.rs`; see [`super`].

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

        /// ETH was sent to a nonpayable factory function.
        error NonPayable();

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

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use alloy_sol_types::{SolCall, SolEnum, SolInterface};

    use super::IB20Factory;

    /// The interface name reaches consensus data via `AbiDecodeFailed` on short calldata, so it is
    /// pinned here rather than left to a future rename. See the module docs.
    #[test]
    fn interface_name_is_frozen() {
        assert_eq!(IB20Factory::IB20FactoryCalls::NAME, "IB20FactoryCalls");
    }

    /// Beryl's `B20Variant` has exactly two variants. A third would make its discriminant decode
    /// at Beryl, minting a token at an address byte `[10]` no Beryl binary could produce.
    #[test]
    fn b20_variant_is_frozen() {
        assert_eq!(IB20Factory::B20Variant::COUNT, 2);
        assert!(IB20Factory::B20Variant::try_from(2u8).is_err());
        assert!(IB20Factory::B20Variant::try_from(0xffu8).is_err());
    }

    /// The exact selector set dialable at Beryl. Adding or removing one changes which calls
    /// historical blocks could make.
    #[test]
    fn selector_set_is_frozen() {
        let mut selectors: Vec<[u8; 4]> = IB20Factory::IB20FactoryCalls::selectors().collect();
        selectors.sort_unstable();

        let mut expected: Vec<[u8; 4]> = alloc::vec![
            IB20Factory::createB20Call::SELECTOR,
            IB20Factory::getB20AddressCall::SELECTOR,
            IB20Factory::isB20Call::SELECTOR,
            IB20Factory::isB20InitializedCall::SELECTOR,
        ];
        expected.sort_unstable();

        assert_eq!(selectors.len(), 4);
        assert_eq!(selectors, expected);
    }
}
