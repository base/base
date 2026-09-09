// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract Proxy {
    // Storage layout must match Logic contract
    address public implementation; // slot 0
    uint256 public value; // slot 1
    uint256 public value2; // slot 2

    constructor(address _impl) {
        implementation = _impl;
    }

    fallback() external payable {
        address impl = implementation;
        assembly {
            calldatacopy(0, 0, calldatasize())
            let result := delegatecall(gas(), impl, 0, calldatasize(), 0, 0)
            returndatacopy(0, 0, returndatasize())
            switch result
            case 0 {
                revert(0, returndatasize())
            }
            default {
                return(0, returndatasize())
            }
        }
    }

    receive() external payable {}
}

contract Logic {
    // Storage layout must match Proxy contract
    address public implementation; // slot 0 - unused but must be here
    uint256 public value; // slot 1
    uint256 public value2; // slot 2

    function setValue(uint256 v) public {
        value = v;
    }

    function setValue2(uint256 v) public {
        value2 = v;
    }

    function getValue() public view returns (uint256) {
        return value;
    }

    function setBoth(uint256 v1, uint256 v2) public {
        value = v1;
        value2 = v2;
    }
}

contract Logic2 {
    address public implementation;
    uint256 public value;
    uint256 public value2;
    address public nextLogic;

    function setNextLogic(address _next) public {
        nextLogic = _next;
    }

    function chainedDelegatecall(
        bytes memory data
    ) public returns (bytes memory) {
        (bool success, bytes memory result) = nextLogic.delegatecall(data);
        require(success, "Chained delegatecall failed");
        return result;
    }
}
