// SPDX-License-Identifier: GPL-3.0
pragma solidity ^0.8.23;

/**
 * User Operation struct for EIP-4337 v0.6
 */
struct UserOperation {
    address sender;
    uint256 nonce;
    bytes initCode;
    bytes callData;
    uint256 callGasLimit;
    uint256 verificationGasLimit;
    uint256 preVerificationGas;
    uint256 maxFeePerGas;
    uint256 maxPriorityFeePerGas;
    bytes paymasterAndData;
    bytes signature;
}

/**
 * Minimal IEntryPoint interface for v0.6
 */
interface IEntryPoint {
    /**
     * Get deposit info for an account
     */
    function balanceOf(address account) external view returns (uint256);

    /**
     * Add deposit for an account
     */
    function depositTo(address account) external payable;

    /**
     * Withdraw deposit
     */
    function withdrawTo(address payable withdrawAddress, uint256 withdrawAmount) external;

    /**
     * Get the current account nonce
     */
    function getNonce(address sender, uint192 key) external view returns (uint256 nonce);

    /**
     * Handle a user operation
     */
    function handleOps(UserOperation[] calldata ops, address payable beneficiary) external;
}



