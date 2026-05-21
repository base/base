//! ABI definition for the `ITokenFactory` interface.

use alloy_sol_types::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface ITokenFactory {
        // ── Structs ─────────────────────────────────────────────────────────

        enum TokenVariant {
            /// Address is not a factory-created B-20 token.
            NONE,
            /// Default B-20 token variant.
            DEFAULT,
            /// Stablecoin B-20 token variant.
            STABLECOIN,
            /// Security B-20 token variant.
            SECURITY
        }

        struct B20CreateParams {
            uint8 version;
            string name;
            string symbol;
            address initialAdmin;
        }

        struct B20StablecoinCreateParams {
            uint8 version;
            string name;
            string symbol;
            address initialAdmin;
            string currency;
        }

        struct B20SecurityCreateParams {
            uint8 version;
            string name;
            string symbol;
            address initialAdmin;
            string isin;
            uint256 minimumRedeemable;
        }

        // ── Errors ───────────────────────────────────────────────────────────

        /// A token already exists at the address derived from `(variant, msg.sender, salt)`.
        error TokenAlreadyExists(address token);

        /// `variant` is not recognized or is `NONE`.
        error InvalidVariant();

        /// `version` is not supported for the requested variant.
        error UnsupportedVersion(uint8 version);

        /// A required string argument was empty.
        error MissingRequiredField();

        /// One of the post-creation init calls failed.
        error InitCallFailed(uint256 index);

        // ── Events ───────────────────────────────────────────────────────────

        event TokenCreated(
            address indexed token,
            TokenVariant indexed variant,
            string name,
            string symbol,
            uint8 decimals
        );

        // ── Functions ────────────────────────────────────────────────────────

        /// Creates a B-20 token of the requested variant at a deterministic address.
        ///
        /// Default tokens start with an unbounded supply cap and the pausable plus mutable-cap
        /// capability bits enabled. Callers configure optional launch state atomically through
        /// `initCalls`, such as minting initial supply, lowering the supply cap, pausing, or setting
        /// metadata.
        function createToken(
            TokenVariant variant,
            bytes32 salt,
            bytes calldata params,
            bytes[] calldata initCalls
        ) external returns (address token);

        /// Returns the address a `createToken` call would produce.
        function getTokenAddress(TokenVariant variant, address sender, bytes32 salt) external view returns (address);

        /// Returns `true` if `token` has the B-20 address prefix.
        function isB20(address token) external view returns (bool);

        /// Returns the variant of `token` or `NONE` if it is not a B-20 token.
        /// Decoded from the address prefix with no storage read.
        function getTokenVariant(address token) external view returns (TokenVariant);
    }
}
