// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Script} from "forge-std/Script.sol";
import {OPSuccinctL2OutputOracle} from "../../src/validity/OPSuccinctL2OutputOracle.sol";
import {Utils} from "../../test/helpers/Utils.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";
import {console} from "forge-std/console.sol";

// This script is used to manage OpSuccinctConfig configurations in the OPSuccinctL2OutputOracle contract.
// If executeUpgradeCall is false, the script will only log the calldata for the parameter update calls.
// Usage:
//   Add config: forge script OpSuccinctParameterUpdater --sig "addConfig(string)" <config_name>
//   Remove config: forge script OpSuccinctParameterUpdater --sig "removeConfig(string)" <config_name>
contract OPSuccinctParameterUpdater is Script, Utils {
    /// @notice Add a new OpSuccinctConfig to the oracle
    /// @param configName The name of the config to add
    function addConfig(string memory configName) public {
        vm.startBroadcast();

        Config memory cfg = readJson("opsuccinctl2ooconfig.json");

        address l2OutputOracleProxy = vm.envAddress("L2OO_ADDRESS");
        bool executeUpgradeCall = vm.envOr("EXECUTE_UPGRADE_CALL", true);

        OPSuccinctL2OutputOracle oracleImpl = OPSuccinctL2OutputOracle(l2OutputOracleProxy);
        bytes32 configNameBytes = keccak256(abi.encodePacked(configName));

        if (executeUpgradeCall) {
            oracleImpl.addOpSuccinctConfig(
                configNameBytes, cfg.rollupConfigHash, cfg.aggregationVkey, cfg.rangeVkeyCommitment
            );
            console.log("Added OpSuccinct config:", configName);
        } else {
            bytes memory configAddCalldata = abi.encodeWithSelector(
                OPSuccinctL2OutputOracle.addOpSuccinctConfig.selector,
                configNameBytes,
                cfg.rollupConfigHash,
                cfg.aggregationVkey,
                cfg.rangeVkeyCommitment
            );
            console.log("The calldata for adding the OP Succinct configuration is:");
            console.logBytes(configAddCalldata);
        }

        vm.stopBroadcast();
    }

    /// @notice Remove an OpSuccinctConfig from the oracle
    /// @param configName The name of the config to remove
    function removeConfig(string memory configName) public {
        vm.startBroadcast();

        address l2OutputOracleProxy = vm.envAddress("L2OO_ADDRESS");
        bool executeUpgradeCall = vm.envOr("EXECUTE_UPGRADE_CALL", true);

        OPSuccinctL2OutputOracle oracleImpl = OPSuccinctL2OutputOracle(l2OutputOracleProxy);
        bytes32 configNameBytes = keccak256(abi.encodePacked(configName));

        if (executeUpgradeCall) {
            oracleImpl.deleteOpSuccinctConfig(configNameBytes);
            console.log("Removed OpSuccinct config:", configName);
        } else {
            bytes memory configRemoveCalldata =
                abi.encodeWithSelector(OPSuccinctL2OutputOracle.deleteOpSuccinctConfig.selector, configNameBytes);
            console.log("The calldata for removing the OP Succinct configuration is:");
            console.logBytes(configRemoveCalldata);
        }

        vm.stopBroadcast();
    }
}
