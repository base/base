// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Test, console} from "forge-std/Test.sol";
import {Utils} from "./helpers/Utils.sol";
import {ZKL2OutputOracle} from "../src/ZKL2OutputOracle.sol";

contract ZKL2OutputOracleTest is Test, Utils {
    bytes32 STARTING_OUTPUT_ROOT = 0x5DE5BC23543212ACFE6D68CCB2F243C80CBD3765782F424DEF4B1E68BABCFE94;
    uint256 STARTING_TIMESTAMP = 1723717092;
    uint256 STARTING_BLOCK_NUM = 15954957;

    bytes32 VK = 0x0092df62aaa9095510bc567d8e3f4c43fe3f4b1884e1a7610c1a892666089519;
    uint256 OP_SEPOLIA_CHAIN_ID = 11155420;

    uint256 constant L1_BLOCK_NUM = 6504359;
    bytes32 L1_HEAD = 0x648a03238b239b1afa20953c99812a0640fec18776b07af8b8c0cdbceacf2dc1;

    bytes32 claimedOutputRoot = 0x48A9017ABA4E1E3FCDCF9E792FD2B749F9852FA7CF677021033E39FA2E2608EB;
    uint256 claimedL2BlockNum = 15955537;
    bytes proof =
        hex"3686e09218b6775028743447d077c686ece6c3756e9290febc1f4dc3367b4c23f8087a1f1b24905ab1843df0aa045e40a5c1fb04d54f62e93bc3db041efc51f8bf3d06d10bb2423f68ea9db9689c14cf2fd7ed6b770b57c442377254433c7f633274e1c905cfc6331cc59eb07372e7d4d125854ead8d58e898dec0fa39290c51875dfd270a9a2f42281bdd336119ec17c174006b6296448ca869f9c30113d52a45e45790111f499eed695572a4857e32b31e563fcdd51245d07967eed7a48c490121fcf816aed1d6883d5438281b3f17c10ef49bbdb0d7abdfd63cbf6b1f78d73152f2520132e5d22138ce4f1e602c9a8e79c3ad82ab9fc43a7ac7d11b3ca541ed0fc4920303f11973e6f3d87ac1839108b82b951f40f81f13899b3f97f40e0e4d26b7a30d4672db3f25a9cd31a37c3f9c74f2bbae172f92a7d2d885d0643ffd3a8137cd0ca9c2aa2fb013a7555a2cc7ba0d605503e073633a263e3cd3e181eb2176edd000f9f0c7b01f44039ed3ab12a3d32987d5884ab10bdd6ecad5864c7c5ad21d990ca24b4478fe0033af8342445446993f955e748e46e0c4d7bc4da241397df86126a217535ef4ce885453dad1de1f1f5ff250de040929c12657b13a83005c9e841565c3de4222afa621cc916b1396ad1c99114f9d13d4f9d3defc690d34d0e7181c97d40f410c6c4197950729da5387159a95496079e37abd80a0cb2c91d08b260fa49472d6b96c6ef55c5c1b08c0f6f799379e4f65e4440b0cb4c4ec960abb54286ca1fa17aaac062b5c22640a19b299ffcbf46eeae0bb9d88266194694a4a131c1bc0397495dea01e80ba0657b0325e0cd0e8bb68d1a5a1a810e44b708a865b0609daaf83ef8c1535af6ec321ed92da1eaf9099c31b52612e62d66a96d0662b200dfcd18c955d609ed09f32bb07b27a7a40590754dbd447a2530651756f2562099f20e9e37d9291eaa0c9ac9ba4ef18ee39113c1d177a2f719b320d8dddbbef187c70b41f398cdd8983317736955c0bb15b81f32a825162490e7ecba4cc5e9900b2d7bf9b4d0231dd5b1ecf9a963a1efc26725821e44e545d65d1cfec836c942e10f1ab07d827bd4526e121329ae8f2b87dba8f93b8d132a31fc72a82f6248f121c29236cb339d86205648d85c5afc03e0570f8013b244b8df2b312e2b6e7983030cc5afdba50c25455a0032a15902819ea07b1c9c7ecc6fc0b8e859f36fdbd";

    ZKL2OutputOracle l2oo;
    Config config;

    function setUp() public {
        vm.createSelectFork("https://sepolia.gateway.tenderly.co", L1_BLOCK_NUM + 1);
        config = readJson("zkconfig.json");

        // set default params for testing
        config.vkey = VK;
        config.startingBlockNumber = STARTING_BLOCK_NUM;
        config.verifierGateway = 0x9AB9824A1529bd745470EDb237E0326dD421e20B;
        config.chainId = OP_SEPOLIA_CHAIN_ID;
    }

    function testZKL2OOFork() public {
        vm.createSelectFork("https://sepolia.gateway.tenderly.co", 6504361);
        l2oo = ZKL2OutputOracle(0xC4b9F9c275B3729774e2D00D8BC0bb09AFe9adF3);
        l2oo.checkpointBlockHash(L1_BLOCK_NUM, L1_HEAD);
        l2oo.proposeL2Output(claimedOutputRoot, claimedL2BlockNum, L1_HEAD, L1_BLOCK_NUM, proof);
    }

    function testZKL2OOFreshDeployment() public {
        l2oo = ZKL2OutputOracle(deployWithConfig(config, STARTING_OUTPUT_ROOT, STARTING_TIMESTAMP));
        vm.startPrank(l2oo.PROPOSER());

        // fails if block hash hasn't been checkpointed
        vm.expectRevert();
        l2oo.proposeL2Output(claimedOutputRoot, claimedL2BlockNum, L1_HEAD, L1_BLOCK_NUM, proof);

        // set block hash
        vm.setBlockhash(L1_BLOCK_NUM, L1_HEAD);
        l2oo.checkpointBlockHash(L1_BLOCK_NUM, L1_HEAD);
        vm.warp(block.timestamp * 2);

        // succeeds after
        l2oo.proposeL2Output(claimedOutputRoot, claimedL2BlockNum, L1_HEAD, L1_BLOCK_NUM, proof);

        assertEq(l2oo.getL2Output(1).outputRoot, claimedOutputRoot);
    }

    function testZKL2OOFailsWithWrongParams() public {
        l2oo = ZKL2OutputOracle(deployWithConfig(config, STARTING_OUTPUT_ROOT, STARTING_TIMESTAMP));

        vm.setBlockhash(L1_BLOCK_NUM, L1_HEAD);
        l2oo.checkpointBlockHash(L1_BLOCK_NUM, L1_HEAD);
        vm.warp(block.timestamp * 2);

        vm.startPrank(l2oo.PROPOSER());

        // fails with wrong claimed output root
        vm.expectRevert();
        l2oo.proposeL2Output(bytes32(0), claimedL2BlockNum, L1_HEAD, L1_BLOCK_NUM, proof);

        // fails with wrong claimed block num
        vm.expectRevert();
        l2oo.proposeL2Output(claimedOutputRoot, claimedL2BlockNum + 1, L1_HEAD, L1_BLOCK_NUM, proof);

        // fails with wrong L1 head
        vm.setBlockhash(L1_BLOCK_NUM, keccak256(""));
        l2oo.checkpointBlockHash(L1_BLOCK_NUM, keccak256(""));
        vm.expectRevert();
        l2oo.proposeL2Output(claimedOutputRoot, claimedL2BlockNum, keccak256(""), L1_BLOCK_NUM, proof);

        // fails with wrong proof
        vm.expectRevert();
        l2oo.proposeL2Output(claimedOutputRoot, claimedL2BlockNum, L1_HEAD, L1_BLOCK_NUM, "");
    }
}
