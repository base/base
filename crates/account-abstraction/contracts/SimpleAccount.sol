// SPDX-License-Identifier: GPL-3.0
pragma solidity ^0.8.23;

import "./IEntryPoint.sol";

/**
 * Minimal ERC-4337 compliant smart contract wallet
 * Based on SimpleAccount from eth-infinitism/account-abstraction
 */
contract SimpleAccount {
    IEntryPoint private immutable _entryPoint;
    address public owner;

    event SimpleAccountInitialized(IEntryPoint indexed entryPoint, address indexed owner);

    modifier onlyOwner() {
        _onlyOwner();
        _;
    }

    function _onlyOwner() internal view {
        require(msg.sender == owner || msg.sender == address(this), "only owner");
    }

    modifier onlyEntryPoint() {
        require(msg.sender == address(_entryPoint), "only EntryPoint");
        _;
    }

    constructor(IEntryPoint anEntryPoint) {
        _entryPoint = anEntryPoint;
        // Don't disable initializers - let the factory call initialize()
    }

    /**
     * Initialize the account with an owner
     */
    function initialize(address anOwner) public {
        require(owner == address(0), "already initialized");
        owner = anOwner;
        emit SimpleAccountInitialized(_entryPoint, owner);
    }

    /**
     * Execute a transaction (called by entryPoint)
     */
    function execute(address dest, uint256 value, bytes calldata func) external onlyEntryPoint {
        _call(dest, value, func);
    }

    /**
     * Execute a sequence of transactions
     */
    function executeBatch(address[] calldata dest, uint256[] calldata value, bytes[] calldata func) external onlyEntryPoint {
        require(dest.length == func.length && dest.length == value.length, "wrong array lengths");
        for (uint256 i = 0; i < dest.length; i++) {
            _call(dest[i], value[i], func[i]);
        }
    }

    /**
     * Validate user's signature and nonce
     * The entryPoint will call this during validation
     */
    function validateUserOp(
        UserOperation calldata userOp,
        bytes32 userOpHash,
        uint256 missingAccountFunds
    ) external onlyEntryPoint returns (uint256 validationData) {
        validationData = _validateSignature(userOp, userOpHash);
        _payPrefund(missingAccountFunds);
    }

    /**
     * Validate signature
     * @param userOp the operation that is about to be executed
     * @param userOpHash hash of the user operation
     * @return validationData signature validation data (0 = success, 1 = failure, sigTimeRange)
     */
    function _validateSignature(UserOperation calldata userOp, bytes32 userOpHash)
        internal virtual view returns (uint256 validationData) {
        bytes32 hash = ECDSA.toEthSignedMessageHash(userOpHash);
        if (owner != ECDSA.recover(hash, userOp.signature))
            return SIG_VALIDATION_FAILED;
        return 0;
    }

    /**
     * Send to the entrypoint (msg.sender) the missing funds for this transaction
     */
    function _payPrefund(uint256 missingAccountFunds) internal {
        if (missingAccountFunds != 0) {
            (bool success,) = payable(msg.sender).call{value: missingAccountFunds, gas: type(uint256).max}("");
            (success);
            //ignore failure (its EntryPoint's job to verify, not account.)
        }
    }

    /**
     * Internal call implementation
     */
    function _call(address target, uint256 value, bytes memory data) internal {
        (bool success, bytes memory result) = target.call{value: value}(data);
        if (!success) {
            assembly {
                revert(add(result, 32), mload(result))
            }
        }
    }

    /**
     * Check current account deposit in the entryPoint
     */
    function getDeposit() public view returns (uint256) {
        return _entryPoint.balanceOf(address(this));
    }

    /**
     * Deposit more funds for this account in the entryPoint
     */
    function addDeposit() public payable {
        _entryPoint.depositTo{value: msg.value}(address(this));
    }

    /**
     * Withdraw value from the account's deposit
     * @param withdrawAddress target to send to
     * @param amount to withdraw
     */
    function withdrawDepositTo(address payable withdrawAddress, uint256 amount) public onlyOwner {
        _entryPoint.withdrawTo(withdrawAddress, amount);
    }

    /**
     * Get the EntryPoint
     */
    function entryPoint() public view returns (IEntryPoint) {
        return _entryPoint;
    }

    // Allow receiving ETH
    receive() external payable {}

    uint256 constant SIG_VALIDATION_FAILED = 1;
}

// Helper library for ECDSA signature recovery
library ECDSA {
    function recover(bytes32 hash, bytes memory signature) internal pure returns (address) {
        if (signature.length != 65) {
            return address(0);
        }

        bytes32 r;
        bytes32 s;
        uint8 v;

        assembly {
            r := mload(add(signature, 0x20))
            s := mload(add(signature, 0x40))
            v := byte(0, mload(add(signature, 0x60)))
        }

        if (uint256(s) > 0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF5D576E7357A4501DDFE92F46681B20A0) {
            return address(0);
        }

        return ecrecover(hash, v, r, s);
    }

    function toEthSignedMessageHash(bytes32 hash) internal pure returns (bytes32) {
        return keccak256(abi.encodePacked("\x19Ethereum Signed Message:\n32", hash));
    }
}

