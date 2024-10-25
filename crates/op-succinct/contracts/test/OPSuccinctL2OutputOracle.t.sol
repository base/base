// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Test, console} from "forge-std/Test.sol";
import {Utils} from "./helpers/Utils.sol";
import {OPSuccinctL2OutputOracle} from "../src/OPSuccinctL2OutputOracle.sol";

contract OPSuccinctL2OutputOracleTest is Test, Utils {
    // Example proof data for the BoB testnet. Tx: https://sepolia.etherscan.io/tx/0x3910121f57c2e81ac98f5154eba7a2845f7ed27caf57a73e516ca606ad9d9aab
    uint256 checkpointedL1BlockNum = 6931062;
    bytes32 claimedOutputRoot = 0xf5ef905ba2c0e598c2f5274177700f3dfe37f66db15e8957e63d0732b0e611b8;
    uint256 claimedL2BlockNum = 3677705;
    bytes proof =
        hex"91ff06f303ed1bf4b5dbf52b2dd7201cb9675afd59200464ef55cff01d113ca54d96b52c2689d0a64c90eb674d1cb9119e4f4fde54d9414d056112df7bf01066b86ee5e410d4d6a93c26c287e1c010bf03fcc0ebfaa6ae294650bba1bf177271c96911771624e73cf6192e3f1a5ac0bd7943f5921df5c22e1c2661a40c33a40b70e9f8d6164ab1e3e1abd666c19aae2012ec389a295e9ce148f781a81363685da83b32390785840f77691e93d734863d283a05497f8a8621dd1dc5e410b6bef0ed9ce53422a8b41ebdbc7e82202fafa1dd5a0fcc458932f76390f9d1f1fbf4134cf68dec06bf5b5b1c0cde47bd89198a52e7b92c634da6dadcf59efa6b78d51273e3316d";

    // The owner of the L2OO.
    address OWNER = 0xDEd0000E32f8F40414d3ab3a830f735a3553E18e;

    OPSuccinctL2OutputOracle l2oo;

    function setUp() public {
        // Note: L1_RPC should be a valid Sepolia RPC.
        vm.createSelectFork(vm.envString("L1_RPC"), checkpointedL1BlockNum + 1);
    }

    // Test the L2OO contract.
    function testOPSuccinctL2OOFork() public {
        l2oo = OPSuccinctL2OutputOracle(0x83EBf366f868784c91d49fBEe67651F7a3de74C5);
        l2oo.checkpointBlockHash(checkpointedL1BlockNum);
        vm.prank(OWNER);
        l2oo.proposeL2Output(claimedOutputRoot, claimedL2BlockNum, checkpointedL1BlockNum, proof);
    }
}
