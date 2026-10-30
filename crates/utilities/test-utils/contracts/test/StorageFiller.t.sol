// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

import {Test} from "forge-std/Test.sol";
import {StorageFiller} from "../src/StorageFiller.sol";

contract StorageFillerTest is Test {
    StorageFiller internal filler;

    function setUp() public {
        filler = new StorageFiller();
    }

    function testWriteStorageOverwritesOnlyEarlierSlots() public {
        filler.writeStorage(100, 0, 1);
        assertEq(filler.written(), 101);

        vm.record();
        filler.writeStorage(2, 20, 2);
        (, bytes32[] memory writes) = vm.accesses(address(filler));
        assertEq(filler.written(), 103);

        // 2 fresh slots + the `written` counter + 20 overwrites.
        assertEq(writes.length, 23);
        uint256 overwrites;
        for (uint256 i = 0; i < 101; i++) {
            if (uint256(vm.load(address(filler), _slotOf(i))) != i + 1) overwrites++;
        }
        assertGt(overwrites, 15);
        assertEq(uint256(vm.load(address(filler), _slotOf(101))), 102);
        assertEq(uint256(vm.load(address(filler), _slotOf(102))), 103);
    }

    function testWriteStorageChangesValueWhenSeedRepeatsInNextBlock() public {
        filler.writeStorage(0, 1, 7);
        bytes32 first = vm.load(address(filler), _slotOf(0));
        vm.roll(block.number + 1);
        filler.writeStorage(0, 1, 7);
        assertEq(filler.written(), 1);
        assertNotEq(vm.load(address(filler), _slotOf(0)), first);
        assertNotEq(uint256(first), 1);
    }

    function _slotOf(uint256 key) internal pure returns (bytes32) {
        return keccak256(abi.encode(key, uint256(0)));
    }
}
