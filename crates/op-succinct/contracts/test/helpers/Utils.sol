// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Test, console} from "forge-std/Test.sol";
import {JSONDecoder} from "./JSONDecoder.sol";
import {Strings} from "@openzeppelin/contracts/utils/Strings.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";
import {OPSuccinctL2OutputOracle} from "src/OPSuccinctL2OutputOracle.sol";

contract Utils is Test, JSONDecoder {
    function deployWithConfig(Config memory cfg) public returns (address) {
        address OPSuccinctL2OutputOracleImpl = address(new OPSuccinctL2OutputOracle());
        Proxy l2OutputOracleProxy = new Proxy(msg.sender);
        upgradeAndInitialize(OPSuccinctL2OutputOracleImpl, cfg, address(l2OutputOracleProxy), true);

        return address(l2OutputOracleProxy);
    }

    // If `executeUpgradeCall` is false, the upgrade call will not be executed.
    function upgradeAndInitialize(address impl, Config memory cfg, address l2OutputOracleProxy, bool executeUpgradeCall)
        public
    {
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
            submissionInterval: cfg.submissionInterval
        });

        bytes memory initializationParams =
            abi.encodeWithSelector(OPSuccinctL2OutputOracle.initialize.selector, initParams);

        if (executeUpgradeCall) {
            Proxy existingProxy = Proxy(payable(l2OutputOracleProxy));
            existingProxy.upgradeToAndCall(impl, initializationParams);
        } else {
            // Raw calldata for an upgrade call by a multisig.
            bytes memory multisigCalldata =
                abi.encodeWithSelector(Proxy.upgradeToAndCall.selector, impl, initializationParams);
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
}
