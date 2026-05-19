//! ABI definition for the `ITokenFactory` interface.

use alloy_sol_types::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface ITokenFactory {
        // ── Structs ─────────────────────────────────────────────────────────

        struct B20TokenParams {
            string name;
            string symbol;
            uint8 decimals;
            address admin;
            uint256 capabilities;
            uint256 initialSupply;
            address initialSupplyRecipient;
            uint256 supplyCap;
            uint256 minimumRedeemable;
            string contractURI;
        }

        struct CreateTokenParams {
            uint8 version;
            uint8 variant;
            bytes requiredParams;
            bytes optionalParams;
            bytes[] postCreateCalls;
            bytes32 salt;
        }

        // ── Errors ───────────────────────────────────────────────────────────

        /// A token is already deployed at the address derived from `(variant, caller, salt)`.
        error TokenAlreadyExists(address token);

        /// The derived address falls in the reserved range (lower 8 bytes < 1024).
        error AddressReserved(address token);

        /// `supplyCap` is below `initialSupply`.
        error InvalidSupplyCap();

        /// A required address argument was `address(0)`.
        error ZeroAddress();

        /// `version` is not supported by this factory.
        error UnsupportedTokenVersion(uint8 version);

        /// `variant` is not supported by this factory.
        error UnsupportedTokenVariant(uint8 variant);

        /// Optional parameter bytes are reserved for future versions.
        error UnsupportedOptionalParams();

        /// `requiredParams` could not be decoded for the requested token shape.
        error InvalidTokenParams();

        // ── Events ───────────────────────────────────────────────────────────

        event TokenCreated(
            address indexed token,
            address indexed creator,
            address indexed admin,
            uint8 variant,
            uint8 decimals,
            string name,
            string symbol,
            uint256 capabilities,
            uint256 initialSupply,
            bytes32 salt
        );

        // ── Functions ────────────────────────────────────────────────────────

        /// Creates a token at a deterministic address.
        function createToken(CreateTokenParams calldata params) external returns (address token);

        /// Returns the address a `createToken` call would produce.
        function predictTokenAddress(address creator, uint8 variant, uint8 decimals, bytes32 salt) external view returns (address);

        /// Returns `true` if `token` is a deployed B-20 token (correct prefix + code at address).
        function isB20(address token) external view returns (bool);

        /// Returns the variant of `token` (0=NONE, 1=DEFAULT).
        /// Decoded from the address prefix with no storage read.
        function variantOf(address token) external view returns (uint8);

        /// Returns the decimals encoded in `token`.
        /// Decoded from the address prefix with no storage read.
        function decimalsOf(address token) external view returns (uint8);
    }
}
