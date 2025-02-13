// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

// Testing
import {Test, console} from "forge-std/Test.sol";
import {Utils} from "../helpers/Utils.sol";

// Libraries
import {IDisputeGame} from "@optimism/src/dispute/interfaces/IDisputeGame.sol";
import {LibCWIA} from "@solady-v0.0.281/utils/legacy/LibCWIA.sol";

import {GameType, Claim} from "@optimism/src/dispute/lib/Types.sol";

// Contracts
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";
import {OPSuccinctL2OutputOracle} from "../../src/validity/OPSuccinctL2OutputOracle.sol";
import {OPSuccinctDisputeGame} from "../../src/validity/OPSuccinctDisputeGame.sol";
import {DisputeGameFactory} from "@optimism/src/dispute/DisputeGameFactory.sol";

contract OPSuccinctDisputeGameFactoryTest is Test, Utils {
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

    // Test the DisputeGameFactory contract.
    function testDisputeGameFactory() public {
        vm.startBroadcast();

        Config memory cfg = readJson("opsuccinctl2ooconfig-test.json");

        cfg.owner = msg.sender;

        address l2ooProxy = deployWithConfig(cfg);

        OPSuccinctL2OutputOracle l2oo = OPSuccinctL2OutputOracle(l2ooProxy);
        OPSuccinctDisputeGame game = new OPSuccinctDisputeGame(l2ooProxy);

        // Deploy the implementation contract for DisputeGameFactory
        DisputeGameFactory factoryImpl = new DisputeGameFactory();

        // Deploy a proxy pointing to the factory implementation
        ERC1967Proxy factoryProxy = new ERC1967Proxy(
            address(factoryImpl), abi.encodeWithSelector(DisputeGameFactory.initialize.selector, address(msg.sender))
        );

        // Cast the proxy to the factory contract
        DisputeGameFactory factory = DisputeGameFactory(address(factoryProxy));

        // NOTE(fakedev9999): GameType 6 is the game type for the OP_SUCCINCT proof system.
        // See https://github.com/ethereum-optimism/optimism/blob/6d7f3bcf1e3a80749a5d70f224e35b49dbd3bb3c/packages/contracts-bedrock/src/dispute/lib/Types.sol#L63-L64
        // Will be updated to GameTypes.OP_SUCCINCT once we upgrade to a new version of the Optimism contracts.
        factory.setInitBond(GameType.wrap(uint32(6)), 1 ether);
        factory.setImplementation(GameType.wrap(uint32(6)), IDisputeGame(address(game)));

        l2oo.addProposer(address(0));
        l2oo.checkpointBlockHash(checkpointedL1BlockNum);

        vm.stopBroadcast();

        factory.create{value: 1 ether}(
            GameType.wrap(uint32(6)),
            Claim.wrap(claimedOutputRoot),
            abi.encode(claimedL2BlockNum, checkpointedL1BlockNum, proof)
        );
    }
}
