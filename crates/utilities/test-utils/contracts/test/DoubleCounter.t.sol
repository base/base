// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

import {Test} from "forge-std/Test.sol";
import {DoubleCounter} from "../src/DoubleCounter.sol";

contract DoubleCounterTest is Test {
    DoubleCounter internal counter;

    function setUp() public {
        counter = new DoubleCounter();
    }

    function testCountersOccupyIndependentSequentialSlots() public {
        assertEq(uint256(vm.load(address(counter), bytes32(uint256(0)))), 1);
        assertEq(uint256(vm.load(address(counter), bytes32(uint256(1)))), 1);

        counter.increment();
        assertEq(uint256(vm.load(address(counter), bytes32(uint256(0)))), 2);
        assertEq(uint256(vm.load(address(counter), bytes32(uint256(1)))), 1);

        counter.increment2();
        assertEq(uint256(vm.load(address(counter), bytes32(uint256(0)))), 2);
        assertEq(uint256(vm.load(address(counter), bytes32(uint256(1)))), 2);
    }
}
