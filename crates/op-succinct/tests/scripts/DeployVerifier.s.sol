// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Script, console} from "forge-std/Script.sol";
import {SP1Verifier as SP1VerifierPlonk} from "@sp1-contracts-v5/src/v5.0.0/SP1VerifierPlonk.sol";
import {SP1Verifier as SP1VerifierGroth16} from "@sp1-contracts-v5/src/v5.0.0/SP1VerifierGroth16.sol";

/// @notice Deploys SP1VerifierPlonk (v5.0.0) for E2E testing.
/// @dev For production deployments, use the canonical sp1-contracts scripts:
///      https://github.com/succinctlabs/sp1-contracts
contract DeployVerifierPlonk is Script {
    function run() external returns (address) {
        vm.startBroadcast();
        SP1VerifierPlonk verifier = new SP1VerifierPlonk();
        vm.stopBroadcast();
        console.log("Deployed SP1VerifierPlonk at:", address(verifier));
        return address(verifier);
    }
}

/// @notice Deploys SP1VerifierGroth16 (v5.0.0) for E2E testing.
/// @dev For production deployments, use the canonical sp1-contracts scripts:
///      https://github.com/succinctlabs/sp1-contracts
contract DeployVerifierGroth16 is Script {
    function run() external returns (address) {
        vm.startBroadcast();
        SP1VerifierGroth16 verifier = new SP1VerifierGroth16();
        vm.stopBroadcast();
        console.log("Deployed SP1VerifierGroth16 at:", address(verifier));
        return address(verifier);
    }
}
