// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract AccessList {
    uint256 public value;

    uint128 public a;
    uint128 public b;

    mapping(uint256 => uint256) public data;

    function updateValue(uint256 newValue) public {
        value = newValue;
    }

    function updateA(uint128 newA) public {
        a = newA;
    }

    function updateB(uint128 newB) public {
        b = newB;
    }

    function insertMultiple(
        uint256[] calldata keys,
        uint256[] calldata values
    ) public {
        require(
            keys.length == values.length,
            "Keys and values length mismatch"
        );
        for (uint256 i = 0; i < keys.length; i++) {
            data[keys[i]] = values[i];
        }
    }

    function getAB() public view returns (uint128, uint128) {
        return (a, b);
    }

    function getMultiple(
        uint256[] calldata keys
    ) public view returns (uint256[] memory) {
        uint256[] memory results = new uint256[](keys.length);
        for (uint256 i = 0; i < keys.length; i++) {
            results[i] = data[keys[i]];
        }
        return results;
    }
}
