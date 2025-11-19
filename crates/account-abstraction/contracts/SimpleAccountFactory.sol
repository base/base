// SPDX-License-Identifier: GPL-3.0
pragma solidity ^0.8.23;

import "./SimpleAccount.sol";
import "./IEntryPoint.sol";

/**
 * A factory contract for SimpleAccount
 * Deterministic account deployment using CREATE2
 */
contract SimpleAccountFactory {
    IEntryPoint public immutable entryPoint;

    constructor(IEntryPoint _entryPoint) {
        entryPoint = _entryPoint;
    }

    /**
     * Create an account, and return its address.
     * Returns the address even if the account is already deployed.
     * Note that during UserOperation execution, this method is called only if the account is not deployed.
     * This method returns an existing account address so that entryPoint.getSenderAddress() would work even after account creation
     */
    function createAccount(address owner, uint256 salt) public returns (SimpleAccount ret) {
        address addr = getAddress(owner, salt);
        uint256 codeSize = addr.code.length;
        if (codeSize > 0) {
            return SimpleAccount(payable(addr));
        }
        ret = SimpleAccount(payable(new SimpleAccount{salt: bytes32(salt)}(entryPoint)));
        ret.initialize(owner);
    }

    /**
     * Calculate the counterfactual address of this account as it would be returned by createAccount()
     */
    function getAddress(address owner, uint256 salt) public view returns (address) {
        return address(uint160(uint256(keccak256(abi.encodePacked(
            bytes1(0xff),
            address(this),
            salt,
            keccak256(abi.encodePacked(
                type(SimpleAccount).creationCode,
                abi.encode(entryPoint)
            ))
        )))));
    }
}


