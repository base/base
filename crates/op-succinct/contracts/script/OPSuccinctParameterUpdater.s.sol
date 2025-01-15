// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Script} from "forge-std/Script.sol";
import {OPSuccinctL2OutputOracle} from "../src/OPSuccinctL2OutputOracle.sol";
import {Utils} from "../test/helpers/Utils.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";
import {console} from "forge-std/console.sol";

// This script is used to update the parameters of the OPSuccinctL2OutputOracle contract.
// If the parameters in the contract don't match the parameters in the config file, the script will update the parameters.
// If executeUpgradeCall is false, the script will only log the calldata for the parameter update calls.
contract OPSuccinctParameterUpdater is Script, Utils {
    function run() public {
        vm.startBroadcast();

        Config memory cfg = readJson("opsuccinctl2ooconfig.json");

        address l2OutputOracleProxy = vm.envAddress("L2OO_ADDRESS");
        bool executeUpgradeCall = vm.envOr("EXECUTE_UPGRADE_CALL", true);

        OPSuccinctL2OutputOracle oracleImpl = OPSuccinctL2OutputOracle(l2OutputOracleProxy);

        if (cfg.aggregationVkey != oracleImpl.aggregationVkey()) {
            if (executeUpgradeCall) {
                oracleImpl.updateAggregationVkey(cfg.aggregationVkey);
            } else {
                bytes memory aggregationVkeyCalldata =
                    abi.encodeWithSelector(OPSuccinctL2OutputOracle.updateAggregationVkey.selector, cfg.aggregationVkey);
                console.log("The calldata for upgrading the aggregationVkey is:");
                console.logBytes(aggregationVkeyCalldata);
            }
        }

        if (cfg.rangeVkeyCommitment != oracleImpl.rangeVkeyCommitment()) {
            if (executeUpgradeCall) {
                oracleImpl.updateRangeVkeyCommitment(cfg.rangeVkeyCommitment);
            } else {
                bytes memory rangeVkeyCommitmentCalldata = abi.encodeWithSelector(
                    OPSuccinctL2OutputOracle.updateRangeVkeyCommitment.selector, cfg.rangeVkeyCommitment
                );
                console.log("The calldata for upgrading the rangeVkeyCommitment is:");
                console.logBytes(rangeVkeyCommitmentCalldata);
            }
        }

        if (cfg.rollupConfigHash != oracleImpl.rollupConfigHash()) {
            if (executeUpgradeCall) {
                oracleImpl.updateRollupConfigHash(cfg.rollupConfigHash);
            } else {
                bytes memory rollupConfigHashCalldata = abi.encodeWithSelector(
                    OPSuccinctL2OutputOracle.updateRollupConfigHash.selector, cfg.rollupConfigHash
                );
                console.log("The calldata for upgrading the rollupConfigHash is:");
                console.logBytes(rollupConfigHashCalldata);
            }
        }

        if (cfg.submissionInterval != oracleImpl.submissionInterval()) {
            if (executeUpgradeCall) {
                oracleImpl.updateSubmissionInterval(cfg.submissionInterval);
            } else {
                bytes memory submissionIntervalCalldata = abi.encodeWithSelector(
                    OPSuccinctL2OutputOracle.updateSubmissionInterval.selector, cfg.submissionInterval
                );
                console.log("The calldata for upgrading the submissionInterval is:");
                console.logBytes(submissionIntervalCalldata);
            }
        }

        if (cfg.verifier != oracleImpl.verifier()) {
            if (executeUpgradeCall) {
                oracleImpl.updateVerifier(cfg.verifier);
            } else {
                bytes memory verifierCalldata =
                    abi.encodeWithSelector(OPSuccinctL2OutputOracle.updateVerifier.selector, cfg.verifier);
                console.log("The calldata for upgrading the verifier is:");
                console.logBytes(verifierCalldata);
            }
        }

        vm.stopBroadcast();
    }
}
