// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Test, console} from "forge-std/Test.sol";
import {JSONDecoder} from "./JSONDecoder.sol";
import {Strings} from "@openzeppelin/contracts/utils/Strings.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";
import {ProxyAdmin} from "@optimism/src/universal/ProxyAdmin.sol";
import {OPSuccinctL2OutputOracle} from "src/OPSuccinctL2OutputOracle.sol";

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
            submissionInterval: cfg.submissionInterval
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
}
