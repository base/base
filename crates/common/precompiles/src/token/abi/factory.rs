//! ABI definition for the `ITokenFactory` interface.

use alloy_sol_types::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface ITokenFactory {
        // ── Structs ─────────────────────────────────────────────────────────

        struct CreateDefaultTokenParams {
            string name;
            string symbol;
            uint8 decimals;
            address admin;
            uint256 capabilities;
            uint256 initialSupply;
            address initialSupplyRecipient;
            uint64 transferPolicyId;
            uint256 supplyCap;
            uint256 minimumRedeemable;
            string contractURI;
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

        // ── Events ───────────────────────────────────────────────────────────

        event DefaultTokenCreated(
            address indexed token,
            address indexed creator,
            address indexed admin,
            string name,
            string symbol,
            uint8 decimals,
            uint256 capabilities,
            uint256 initialSupply,
            bytes32 salt
        );

        // ── Functions ────────────────────────────────────────────────────────

        /// Creates a Default-variant token at a deterministic address.
        function createDefault(CreateDefaultTokenParams calldata params) external returns (address token);

        /// Returns the address a `createDefault` call would produce for `(creator, salt)`.
        function predictDefaultAddress(address creator, bytes32 salt) external view returns (address);

        /// Returns the address a `createStablecoin` call would produce for `(creator, salt)`.
        function predictStablecoinAddress(address creator, bytes32 salt) external view returns (address);

        /// Returns the address a `createSecurity` call would produce for `(creator, salt)`.
        function predictSecurityAddress(address creator, bytes32 salt) external view returns (address);

        /// Returns `true` if `token` is a deployed B-20 token (correct prefix + code at address).
        function isB20(address token) external view returns (bool);

        /// Returns the variant of `token` (0=NONE, 1=DEFAULT, 2=STABLECOIN, 3=SECURITY).
        /// Decoded from the address prefix with no storage read.
        function variantOf(address token) external view returns (uint8);
    }
}
