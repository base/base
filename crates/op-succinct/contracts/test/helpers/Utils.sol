// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Test, console} from "forge-std/Test.sol";
import {JSONDecoder} from "./JSONDecoder.sol";
import {Strings} from "@openzeppelin/contracts/utils/Strings.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";
import {ProxyAdmin} from "@optimism/src/universal/ProxyAdmin.sol";
import {OPSuccinctL2OutputOracle} from "../../src/validity/OPSuccinctL2OutputOracle.sol";
import {SP1MockVerifier} from "@sp1-contracts/src/SP1MockVerifier.sol";

contract Utils is Test, JSONDecoder {
    function deployWithConfig(Config memory cfg) public returns (address) {
        if (cfg.opSuccinctL2OutputOracleImpl == address(0)) {
            cfg.opSuccinctL2OutputOracleImpl = address(new OPSuccinctL2OutputOracle());
        }

        Proxy l2OutputOracleProxy = new Proxy(msg.sender);
        upgradeAndInitialize(cfg, address(l2OutputOracleProxy), true);

        return address(l2OutputOracleProxy);
    }

    // If `executeUpgradeCall` is false, the upgrade call will not be executed.
    function upgradeAndInitialize(Config memory cfg, address l2OutputOracleProxy, bool executeUpgradeCall) public {
        // Require that the verifier gateway is deployed
        require(
            address(cfg.verifier).code.length > 0, "OPSuccinctL2OutputOracleUpgrader: verifier gateway not deployed"
        );

        OPSuccinctL2OutputOracle.InitParams memory initParams = OPSuccinctL2OutputOracle.InitParams({
            verifier: cfg.verifier,
            aggregationVkey: cfg.aggregationVkey,
            rangeVkeyCommitment: cfg.rangeVkeyCommitment,
            startingOutputRoot: cfg.startingOutputRoot,
            rollupConfigHash: cfg.rollupConfigHash,
            proposer: cfg.proposer,
            challenger: cfg.challenger,
            owner: cfg.owner,
            finalizationPeriodSeconds: cfg.finalizationPeriod,
            l2BlockTime: cfg.l2BlockTime,
            startingBlockNumber: cfg.startingBlockNumber,
            startingTimestamp: cfg.startingTimestamp,
            submissionInterval: cfg.submissionInterval,
            fallbackTimeout: cfg.fallbackProposalTimeout
        });

        bytes memory initializationParams =
            abi.encodeWithSelector(OPSuccinctL2OutputOracle.initialize.selector, initParams);

        if (executeUpgradeCall) {
            if (cfg.proxyAdmin == address(0)) {
                Proxy existingProxy = Proxy(payable(l2OutputOracleProxy));
                existingProxy.upgradeToAndCall(cfg.opSuccinctL2OutputOracleImpl, initializationParams);
            } else {
                // This is used if the ProxyAdmin contract is deployed.
                ProxyAdmin(payable(cfg.proxyAdmin)).upgradeAndCall(
                    payable(l2OutputOracleProxy), cfg.opSuccinctL2OutputOracleImpl, initializationParams
                );
            }
        } else {
            // Raw calldata for an upgrade call by a multisig.
            bytes memory multisigCalldata = "";

            if (cfg.proxyAdmin == address(0)) {
                multisigCalldata = abi.encodeWithSelector(
                    Proxy.upgradeToAndCall.selector, cfg.opSuccinctL2OutputOracleImpl, initializationParams
                );
            } else {
                multisigCalldata = abi.encodeWithSelector(
                    ProxyAdmin.upgradeAndCall.selector, cfg.opSuccinctL2OutputOracleImpl, initializationParams
                );
            }

            console.log("The calldata for upgrading the contract with the new initialization parameters is:");
            console.logBytes(multisigCalldata);
        }
    }

    // Read the config from the json file.
    function readJson(string memory filepath) public view returns (Config memory) {
        string memory root = vm.projectRoot();
        string memory path = string.concat(root, "/", filepath);
        string memory json = vm.readFile(path);
        bytes memory data = vm.parseJson(json);
        return abi.decode(data, (Config));
    }

    // Helper functions for test setup
    /**
     * @dev Creates standard InitParams for OPSuccinctL2OutputOracle with sensible defaults
     */
    function createStandardInitParams(address verifier, address proposer, address challenger, address owner)
        public
        view
        returns (OPSuccinctL2OutputOracle.InitParams memory)
    {
        return OPSuccinctL2OutputOracle.InitParams({
            verifier: verifier,
            aggregationVkey: bytes32(0),
            rangeVkeyCommitment: bytes32(0),
            startingOutputRoot: bytes32(0),
            rollupConfigHash: bytes32(0),
            proposer: proposer,
            challenger: challenger,
            owner: owner,
            finalizationPeriodSeconds: 1000 seconds,
            l2BlockTime: 2 seconds,
            startingBlockNumber: 0,
            startingTimestamp: block.timestamp,
            submissionInterval: 1000 seconds,
            fallbackTimeout: 3600 seconds
        });
    }

    /**
     * @dev Creates InitParams with custom fallback timeout and other parameters
     */
    function createInitParamsWithFallback(
        address verifier,
        address proposer,
        address challenger,
        address owner,
        uint256 submissionInterval,
        uint256 l2BlockTime,
        uint256 startingBlockNumber,
        uint256 finalizationPeriod,
        uint256 fallbackTimeout
    ) public view returns (OPSuccinctL2OutputOracle.InitParams memory) {
        return OPSuccinctL2OutputOracle.InitParams({
            verifier: verifier,
            aggregationVkey: keccak256("aggregation_vkey"),
            rangeVkeyCommitment: keccak256("range_vkey"),
            startingOutputRoot: keccak256("starting_output"),
            rollupConfigHash: keccak256("rollup_config"),
            proposer: proposer,
            challenger: challenger,
            owner: owner,
            finalizationPeriodSeconds: finalizationPeriod,
            l2BlockTime: l2BlockTime,
            startingBlockNumber: startingBlockNumber,
            startingTimestamp: block.timestamp,
            submissionInterval: submissionInterval,
            fallbackTimeout: fallbackTimeout
        });
    }

    /**
     * @dev Deploys and initializes an OPSuccinctL2OutputOracle with a proxy
     */
    function deployL2OutputOracle(OPSuccinctL2OutputOracle.InitParams memory initParams)
        public
        returns (OPSuccinctL2OutputOracle)
    {
        bytes memory initializationParams =
            abi.encodeWithSelector(OPSuccinctL2OutputOracle.initialize.selector, initParams);

        Proxy l2OutputOracleProxy = new Proxy(address(this));
        l2OutputOracleProxy.upgradeToAndCall(address(new OPSuccinctL2OutputOracle()), initializationParams);

        return OPSuccinctL2OutputOracle(address(l2OutputOracleProxy));
    }

    /**
     * @dev Convenience function to create verifier, params, and deploy L2OutputOracle
     */
    function deployL2OutputOracleWithStandardParams(address proposer, address challenger, address owner)
        public
        returns (OPSuccinctL2OutputOracle, SP1MockVerifier)
    {
        SP1MockVerifier verifier = new SP1MockVerifier();
        OPSuccinctL2OutputOracle.InitParams memory initParams =
            createStandardInitParams(address(verifier), proposer, challenger, owner);
        OPSuccinctL2OutputOracle oracle = deployL2OutputOracle(initParams);
        return (oracle, verifier);
    }

    /**
     * @dev Helper to checkpoint an L1 block hash and roll forward
     */
    function checkpointAndRoll(OPSuccinctL2OutputOracle oracle, uint256 l1BlockNumber) public {
        vm.roll(l1BlockNumber + 1);
        oracle.checkpointBlockHash(l1BlockNumber);
    }

    /**
     * @dev Helper to setup time and block for proposal testing
     */
    function setupTimeAndBlock(uint256 timeOffset, uint256 l1BlockNumber) public {
        vm.warp(block.timestamp + timeOffset);
        vm.roll(l1BlockNumber + 1);
    }

    /**
     * @dev Common pattern: warp time, roll block, checkpoint
     */
    function warpRollAndCheckpoint(OPSuccinctL2OutputOracle oracle, uint256 timeOffset, uint256 l1BlockNumber) public {
        setupTimeAndBlock(timeOffset, l1BlockNumber);
        oracle.checkpointBlockHash(l1BlockNumber);
    }
}
