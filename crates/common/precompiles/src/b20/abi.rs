//! ABI definition for the `IB20` interface.

use alloy_sol_types::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IB20 {
        // Errors
        error ContractPaused(uint256 pausedVector);
        error InsufficientAllowance(address spender, uint256 allowance, uint256 needed);
        error InsufficientBalance(address sender, uint256 balance, uint256 needed);
        error InvalidSender(address sender);
        error InvalidReceiver(address receiver);
        error InvalidApprover(address approver);
        error InvalidSpender(address spender);
        error InvalidAmount();
        error InvalidSupplyCap(uint256 currentSupply, uint256 proposedCap);
        error SupplyCapExceeded(uint256 cap, uint256 attempted);
        error ExpiredSignature(uint256 deadline);
        error InvalidSigner(address signer, address owner);
        error FeatureDisabled(uint256 capability);
        error MinimumRedeemableNotMet(uint256 amount, uint256 minimum);
        error Unauthorized();
        error Uninitialized();

        // Events
        event Transfer(address indexed from, address indexed to, uint256 amount);
        event Approval(address indexed owner, address indexed spender, uint256 amount);
        event Memo(bytes32 indexed memo);
        event Paused(address indexed updater, uint256 vectors);
        event Unpaused(address indexed updater);
        event SupplyCapUpdated(address indexed updater, uint256 oldSupplyCap, uint256 newSupplyCap);
        event ContractURIUpdated();
        event NameUpdated(address indexed updater, string newName);
        event SymbolUpdated(address indexed updater, string newSymbol);
        event Redeemed(address indexed holder, uint256 amount);
        event MinimumRedeemableUpdated(address indexed updater, uint256 oldMinimum, uint256 newMinimum);

        // Capabilities
        function capabilities() external view returns (uint256);
        function isPausable() external view returns (bool);
        function isCapMutable() external view returns (bool);

        // ERC-20
        function name() external view returns (string);
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
        function totalSupply() external view returns (uint256);
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function transfer(address to, uint256 amount) external returns (bool);
        function transferFrom(address from, address to, uint256 amount) external returns (bool);
        function approve(address spender, uint256 amount) external returns (bool);

        // Metadata updates
        function setName(string calldata newName) external;
        function setSymbol(string calldata newSymbol) external;

        // Memo transfer variants
        function transferWithMemo(address to, uint256 amount, bytes32 memo) external returns (bool);
        function transferFromWithMemo(address from, address to, uint256 amount, bytes32 memo) external returns (bool);

        // Mint / burn
        function mint(address to, uint256 amount) external;
        function mintWithMemo(address to, uint256 amount, bytes32 memo) external;
        function burn(uint256 amount) external;
        function burnWithMemo(uint256 amount, bytes32 memo) external;

        // Redeem
        function redeem(uint256 amount) external;
        function redeemWithMemo(uint256 amount, bytes32 memo) external;
        function minimumRedeemable() external view returns (uint256);
        function setMinimumRedeemable(uint256 newMinimum) external;

        // Pause
        function paused() external view returns (uint256);
        function isPaused(uint256 vector) external view returns (bool);
        function pause(uint256 vectors) external;
        function unpause() external;

        // Supply cap
        function supplyCap() external view returns (uint256);
        function setSupplyCap(uint256 newSupplyCap) external;

        // Permit (EIP-2612 + ERC-5267)
        function DOMAIN_SEPARATOR() external view returns (bytes32);
        function nonces(address owner) external view returns (uint256);
        function permit(address owner, address spender, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s) external;
        function eip712Domain() external view returns (bytes1 fields, string memory name, string memory version, uint256 chainId, address verifyingContract, bytes32 salt, uint256[] memory extensions);

        // Contract URI (ERC-7572)
        function contractURI() external view returns (string);
        function setContractURI(string calldata newURI) external;
    }
}
