// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Test, console} from "forge-std/Test.sol";
import {Utils} from "./helpers/Utils.sol";
import {OPSuccinctL2OutputOracle} from "../src/OPSuccinctL2OutputOracle.sol";

contract OPSuccinctL2OutputOracleTest is Test, Utils {
    uint256 STARTING_BLOCK_NUM = 3087613;

    bytes32 AGGREGATION_VKEY = 0x002d397eaa6f2bd3a873f2b996a6d486eb20774092e68a75471e287084180c13;
    bytes32 RANGE_VKEY_COMMITMENT = 0x3237870c3fe7a735661b52f641bd41c85a886c916a962526533c8c9d17dc0831;
    uint256 OP_STACK_CHAIN_ID = 808813;

    uint256 constant L1_BLOCK_NUM = 6919874;
    bytes32 L1_HEAD = 0x734703707095dd98640db082c0735a9de3a27b815acb9f3e4cd53648df2b830f;

    bytes32 claimedOutputRoot = 0x918d98ae75c4aa89ac9098a85dc18e0777bf073d1943d991e1f1f54a1f92450c;
    uint256 claimedL2BlockNum = 3381733;
    bytes proof =
        hex"91ff06f30532505692c10b9a952d3392017a67fc9c5997246150379c74ef11b28087b1912a2e591c0b3d5a80ed7b224a7cecfc0b882dca09ad9bcc4ac26a76dfbecc2e0f1aac3b85587c867bed8eea3402f10d381f5df4ddb54249dd6c29d967a458cb6c2b09d136197641e2894503a52dc095bcb3c783c6171ed0bbc77d05a967e8d15e1f31fe6af7cf3d669f1e62b5c1c66404baa7c7717ac57fdd803e0938bab3369822889c5b8eb4ab50dfbcdc6f99ac0d3b3290876a17d36604cb5c88ec5c8780c10ef7d49135d9b835e8f49308b86576cbf4d3e3b955807d6c592c0c42817099b617bea8309f49d31ba6630f1b73ba10f051942a43d22cd118612b6d3c252f15b6";

    OPSuccinctL2OutputOracle l2oo;
    Config config;

    function setUp() public {
        vm.createSelectFork(vm.envString("L1_RPC"), L1_BLOCK_NUM + 1);
        config = readJson("opsuccinctl2ooconfig.json");

        // set default params for testing
        config.aggregationVkey = AGGREGATION_VKEY;
        config.rangeVkeyCommitment = RANGE_VKEY_COMMITMENT;
        config.startingBlockNumber = STARTING_BLOCK_NUM;
        config.verifierGateway = 0x3B6041173B80E77f038f3F2C0f9744f04837185e;
        config.chainId = OP_STACK_CHAIN_ID;
    }

    function testOPSuccinctL2OOFork() public {
        vm.createSelectFork(vm.envString("L1_RPC"), L1_BLOCK_NUM + 1);
        l2oo = OPSuccinctL2OutputOracle(0xd9979DD3cbE74C46fdD8bDB122775Fc7D0DF0BCE);
        l2oo.checkpointBlockHash(L1_BLOCK_NUM, L1_HEAD);
        vm.prank(config.owner);
        l2oo.proposeL2Output(claimedOutputRoot, claimedL2BlockNum, L1_HEAD, L1_BLOCK_NUM, proof);
    }
}
