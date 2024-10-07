// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Test, console} from "forge-std/Test.sol";
import {Utils} from "./helpers/Utils.sol";
import {OPSuccinctL2OutputOracle} from "../src/OPSuccinctL2OutputOracle.sol";

contract OPSuccinctL2OutputOracleTest is Test, Utils {
    bytes32 STARTING_OUTPUT_ROOT = 0x31da1e7188abc165b685a144f92e4a15a61ba33b6dcb7b827e55f488134513ab;
    uint256 STARTING_TIMESTAMP = 1727131352;
    uint256 STARTING_BLOCK_NUM = 1822000;

    bytes32 AGGREGATION_VKEY = 0x002fff475261fad7d14e48354686fb33a2a8263f2f603846eca4f0a44eca67f4;
    bytes32 RANGE_VKEY_COMMITMENT = 0x3dfb83525f0a3a9118cb9c5547729551043fa7c626d4081805307b336db899ed;
    uint256 OP_STACK_CHAIN_ID = 13269;

    uint256 constant L1_BLOCK_NUM = 6747867;
    bytes32 L1_HEAD = 0x7a19632360578cb90f01528b7aa241f13328eb76d536fd546403fe88d834322b;

    bytes32 claimedOutputRoot = 0x7d1e2bc90c3907aec2a34118a9c858a5254b05812d0cabb4635847fd1e40e67e;
    uint256 claimedL2BlockNum = 1822022;
    bytes proof =
        hex"5a1551d60cc24fe58c6c9b859081cf151a3e606628dbeb73767f334b3208cd25d9d87a2e0985c84df18d4289a5bffeb0c80ce9bc44ce1d4bb8c9ae79c9a0a34cc1d0f4c3211acf2fa71fa4c8ac0a3d90ca4c7cbf336b7c62ad68ccb50bdbdd415b5aca41287cf97a3929b60e68e456a3cea1669fe9d0ed2ed5954ab77ce391cff767032723a4b084bac9fe5ba41b2daefb17a7fbd9b06ddeed6ab03d6776dfdce1fc68d11cf42284705eeade326cee41c2230014185904be10c922b23f82539990bec2113058622bd1c8756e8687c8b7c916984a3ec90f96f0de06bc33e628b407a452171729c1540586033831732ccc54ca97500d0fb4aa21390f21be107d654b4b736d";

    OPSuccinctL2OutputOracle l2oo;
    Config config;

    function setUp() public {
        vm.createSelectFork("https://ethereum-sepolia-rpc.publicnode.com", L1_BLOCK_NUM + 1);
        config = readJson("opsuccinctl2ooconfig.json");

        // set default params for testing
        config.aggregationVkey = AGGREGATION_VKEY;
        config.rangeVkeyCommitment = RANGE_VKEY_COMMITMENT;
        config.startingBlockNumber = STARTING_BLOCK_NUM;
        config.verifierGateway = 0x3B6041173B80E77f038f3F2C0f9744f04837185e;
        config.chainId = OP_STACK_CHAIN_ID;
    }

    function testOPSuccinctL2OOFork() public {
        vm.createSelectFork("https://ethereum-sepolia-rpc.publicnode.com", L1_BLOCK_NUM + 1);
        l2oo = OPSuccinctL2OutputOracle(0xE8F5d09640Fe9Fc7C7A05c8ee9a49836b4862502);
        l2oo.checkpointBlockHash(L1_BLOCK_NUM, L1_HEAD);
        l2oo.proposeL2Output(claimedOutputRoot, claimedL2BlockNum, L1_HEAD, L1_BLOCK_NUM, proof);
    }
}
