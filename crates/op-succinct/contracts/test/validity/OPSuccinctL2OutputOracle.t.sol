// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Test, console} from "forge-std/Test.sol";
import {Utils} from "../helpers/Utils.sol";
import {OPSuccinctL2OutputOracle} from "../../src/validity/OPSuccinctL2OutputOracle.sol";

contract OPSuccinctL2OutputOracleTest is Test, Utils {
    // Example proof data for a mock proof for Phala Testnet. Tx: https://sepolia.etherscan.io/tx/0x640441cfcba322574a0b153fa3a520bc7bbf1591fdee32f7984dfcf4e18fde4f
    uint256 checkpointedL1BlockNum = 7931837;
    bytes32 claimedOutputRoot = 0xfb2b5dde22744d80ef752a49227a8a4927f998999a66338a22b06f093e9ccd3c;
    uint256 claimedL2BlockNum = 1432001;
    bytes proof = hex"";
    address proverAddress = 0x788c45CafaB3ea427b9079889BE43D7d3cd7500C;

    // The owner of the L2OO.
    address OWNER = 0x788c45CafaB3ea427b9079889BE43D7d3cd7500C;

    OPSuccinctL2OutputOracle l2oo;

    function setUp() public {
        // Note: L1_RPC should be a valid Sepolia RPC.
        vm.createSelectFork(vm.envString("L1_RPC"), checkpointedL1BlockNum + 1);
    }

    // Test the L2OO contract.
    function testOPSuccinctL2OOFork() public {
        l2oo = OPSuccinctL2OutputOracle(0x5f0c7178CF4d7520f347d1334e5fc219da9b8Da4);
        l2oo.checkpointBlockHash(checkpointedL1BlockNum);
        vm.prank(OWNER);
        l2oo.proposeL2Output(claimedOutputRoot, claimedL2BlockNum, checkpointedL1BlockNum, proof, proverAddress);
    }
}
