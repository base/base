// SPDX-License-Identifier: MIT
pragma solidity ^0.8.23;

interface IEntryPoint {
    function balanceOf(address) external view returns (uint256);
}

contract SimpleAccountFactory {
    IEntryPoint public immutable entryPoint;
    
    constructor(IEntryPoint _entryPoint) {
        entryPoint = _entryPoint;
    }
    
    function test() public pure returns (uint256) {
        return 42;
    }
}
