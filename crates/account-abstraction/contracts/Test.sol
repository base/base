// SPDX-License-Identifier: GPL-3.0
pragma solidity ^0.8.23;

contract Test {
    address public owner;
    constructor(address _owner) {
        owner = _owner;
    }
}
