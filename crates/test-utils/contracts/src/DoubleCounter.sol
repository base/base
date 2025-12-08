// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract DoubleCounter {
    uint256 public count1 = 1;
    uint256 public count2 = 1;

    function increment() public {
        count1++;
    }

    function increment2() public {
        count2++;
    }
}
