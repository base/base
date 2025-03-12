// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Test, console} from "forge-std/Test.sol";
import {Utils} from "../helpers/Utils.sol";
import {OPSuccinctL2OutputOracle} from "../../src/validity/OPSuccinctL2OutputOracle.sol";

contract OPSuccinctL2OutputOracleTest is Test, Utils {
    // Example proof data for the Base Sepolia. Tx: https://sepolia.etherscan.io/tx/0x97a7c9f2d5c23fa788e209b1fc539c94dbdf0c7216eac2cb5227c0c899d3c204
    uint256 checkpointedL1BlockNum = 7876068;
    bytes32 claimedOutputRoot = 0x9555cbad67886211aa4727b3b9f0d2cfd411f3631b7453b36906b9ef85d65e2c;
    uint256 claimedL2BlockNum = 22935345;
    bytes proof =
        hex"11b6a09d2881110ed4143ef95109be99e2ff705857ead350d8b92fcb5f273aac19bfd7eb2444565b9865012e5e908ca43a6f75e62c6d6c0e20e2f43014152226f3c069682c572051b98015166a54540777b369a3c47b50d4f9c05e6a54626a724c452570116300116572228fe1660b9a28c40f60614e6c59ed0ecac1d02ab42a07d1b1b0208e4a10231d970605957cd8705224d3cf7b2dc32e1b4935625605982d1a3f9d0828d29b09b715e5556d2981e38079a7f11b0d54151f9fc10b9ba819d95985762f542b871e5f17201501c2996828495eb69d420aeccd4cced9aacbc09809213d2993fd1e61fd08c130cd1f14ca7e4e7060ee543a6a726f384539e264f2105c29";

    // The owner of the L2OO.
    address OWNER = 0x9193a78157957F3E03beE50A3E6a51F0f1669E23;

    OPSuccinctL2OutputOracle l2oo;

    function setUp() public {
        // Note: L1_RPC should be a valid Sepolia RPC.
        vm.createSelectFork(vm.envString("L1_RPC"), checkpointedL1BlockNum + 1);
    }

    // Test the L2OO contract.
    function testOPSuccinctL2OOFork() public {
        l2oo = OPSuccinctL2OutputOracle(0xDD9393B0E2FfB3B8DFd94C91f53a492cFB5561DC);
        l2oo.checkpointBlockHash(checkpointedL1BlockNum);
        vm.prank(OWNER);
        l2oo.proposeL2Output(claimedOutputRoot, claimedL2BlockNum, checkpointedL1BlockNum, proof);
    }
}
