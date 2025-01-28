// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Test, console} from "forge-std/Test.sol";
import {Utils} from "../helpers/Utils.sol";
import {OPSuccinctL2OutputOracle} from "../../src/validity/OPSuccinctL2OutputOracle.sol";
import {OPSuccinctDisputeGame} from "../../src/validity/OPSuccinctDisputeGame.sol";
import {IDisputeGame} from "@optimism/src/dispute/interfaces/IDisputeGame.sol";
import {LibCWIA} from "@solady-v0.0.281/utils/legacy/LibCWIA.sol";

contract OPSuccinctL2OutputOracleTest is Test, Utils {
    using LibCWIA for address;

    // Example proof data for the BoB testnet. Tx: https://sepolia.etherscan.io/tx/0x35df99dce5db3d7644a005bd582af2d66533b56fdb01970f248d96e8053fc0ba
    uint256 checkpointedL1BlockNum = 7438547;
    bytes32 claimedOutputRoot = 0x974323e1f533bf40923f6a5f9d8752d42743bb5b784d9a6d1ce223a5cc368ae6;
    uint256 claimedL2BlockNum = 6940641;
    bytes proof =
        hex"09069090289d338bbce470b324757ae21b8846ba36d88feb8fc9e32aa477d193153db2bc1ffead4fb681196de556343a1cd61954d5e6863327d35e0f2e0b9781278b58231af27bb83226d60c1573639e400130ed49318f28dddb9768c8a71f20de8bc07d0355ef76ec0661b0d720d36943e7d8660b6e603733afb549ffba8773cec52097011525d1239e39b8da29bec5fb18d6f4bdfd84890fedd6c0cf67342a6843bb2a28e9ceae9069e52312b7b79d4a39b7d5527bbcfefd66de3887cea63f76b672081dd49279796f07bfdb04e9c5284dd0565ac923bc2c5c01be28a22c314402280001a7aa13b9a8a1c92850ae89fcede9142542fbc13298ecab89ad8fbfbdabbee3";

    function setUp() public {
        // Note: L1_RPC should be a valid Sepolia RPC.
        vm.createSelectFork(vm.envString("L1_RPC"), checkpointedL1BlockNum + 1);
    }

    // Test the DisputeGame contract.
    function testOPSuccinctDisputeGame() public {
        vm.startBroadcast();

        Config memory cfg = readJson("opsuccinctl2ooconfig-test.json");

        cfg.owner = msg.sender;

        address l2ooProxy = deployWithConfig(cfg);

        OPSuccinctL2OutputOracle l2oo = OPSuccinctL2OutputOracle(l2ooProxy);
        OPSuccinctDisputeGame game = new OPSuccinctDisputeGame(l2ooProxy);

        l2oo.addProposer(address(0));
        l2oo.checkpointBlockHash(checkpointedL1BlockNum);

        IDisputeGame proxy = IDisputeGame(
            address(game).clone(
                abi.encodePacked(
                    msg.sender,
                    claimedOutputRoot,
                    bytes32(0), // TODO: This should be parentHash
                    abi.encode(claimedL2BlockNum, checkpointedL1BlockNum, proof)
                )
            )
        );

        vm.stopBroadcast();

        proxy.initialize{value: 10}();
    }
}
