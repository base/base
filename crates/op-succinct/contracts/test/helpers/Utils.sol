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
        address l2OutputOracleProxy = address(new Proxy(address(this)));

        // Upgrade the proxy to point to the implementation and call initialize().
        // Override the starting output root and timestmp with the passed values.
        upgradeAndInitialize(OPSuccinctL2OutputOracleImpl, cfg, l2OutputOracleProxy, address(0));

        // Transfer ownership of proxy to owner specified in the config.
        Proxy(payable(l2OutputOracleProxy)).changeAdmin(cfg.owner);

        return l2OutputOracleProxy;
    }

    function upgradeAndInitialize(address impl, Config memory cfg, address l2OutputOracleProxy, address _spoofedAdmin)
        public
    {
        // require that the verifier gateway is deployed
        require(
            address(cfg.verifierGateway).code.length > 0,
            "OPSuccinctL2OutputOracleUpgrader: verifier gateway not deployed"
        );

        OPSuccinctL2OutputOracle.InitParams memory initParams = OPSuccinctL2OutputOracle.InitParams({
            chainId: cfg.chainId,
            verifierGateway: cfg.verifierGateway,
            aggregationVkey: cfg.aggregationVkey,
            rangeVkeyCommitment: cfg.rangeVkeyCommitment,
            owner: cfg.owner,
            startingOutputRoot: cfg.startingOutputRoot,
            rollupConfigHash: cfg.rollupConfigHash
        });

        // If we are spoofing the admin (used in testing), start prank.
        if (_spoofedAdmin != address(0)) vm.startPrank(_spoofedAdmin);

        Proxy(payable(l2OutputOracleProxy)).upgradeToAndCall(
            impl,
            abi.encodeCall(
                OPSuccinctL2OutputOracle.initialize,
                (
                    cfg.submissionInterval,
                    cfg.l2BlockTime,
                    cfg.startingBlockNumber,
                    cfg.startingTimestamp,
                    cfg.proposer,
                    cfg.challenger,
                    cfg.finalizationPeriod,
                    initParams
                )
            )
        );
    }

    // Read the config from the json file.
    function readJson(string memory filepath) public view returns (Config memory) {
        string memory root = vm.projectRoot();
        string memory path = string.concat(root, "/", filepath);
        string memory json = vm.readFile(path);
        bytes memory data = vm.parseJson(json);
        return abi.decode(data, (Config));
    }

    // This script updates the rollup config hash and the block number in the config.
    function updateRollupConfig() public {
        // If ENV_FILE is set, pass it to the fetch-rollup-config binary.
        string memory envFile = vm.envOr("ENV_FILE", string(".env.conduit"));

        // Build the fetch-rollup-config binary. Use the quiet flag to suppress build output.
        string[] memory inputs = new string[](6);
        inputs[0] = "cargo";
        inputs[1] = "build";
        inputs[2] = "--bin";
        inputs[3] = "fetch-rollup-config";
        inputs[4] = "--release";
        inputs[5] = "--quiet";
        vm.ffi(inputs);

        // Run the fetch-rollup-config binary which updates the rollup config hash and the block number in the config.
        // Use the quiet flag to suppress build output.
        string[] memory inputs2 = new string[](9);
        inputs2[0] = "cargo";
        inputs2[1] = "run";
        inputs2[2] = "--bin";
        inputs2[3] = "fetch-rollup-config";
        inputs2[4] = "--release";
        inputs2[5] = "--quiet";
        inputs2[6] = "--";
        inputs2[7] = "--env-file";
        inputs2[8] = envFile;

        vm.ffi(inputs2);
    }
}
