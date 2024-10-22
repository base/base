// SPDX-License-Identifier: MIT
pragma solidity ^0.8.15;

import {Script} from "forge-std/Script.sol";
import {OPSuccinctL2OutputOracle} from "../src/OPSuccinctL2OutputOracle.sol";
import {Utils} from "../test/helpers/Utils.sol";
import {Proxy} from "@optimism/src/universal/Proxy.sol";
import {console} from "forge-std/console.sol";

contract OPSuccinctDeployer is Script, Utils {
    function run() public returns (address) {
        vm.startBroadcast();

        // Update the rollup config to match the current chain. If the starting block number is 0, the latest block number and starting output root will be fetched.
        updateRollupConfig();

        Config memory config = readJson("opsuccinctl2ooconfig.json");

        // This initializes the proxy.
        OPSuccinctL2OutputOracle oracleImpl = new OPSuccinctL2OutputOracle();
        Proxy proxy = new Proxy(msg.sender);

        // Upgrade the proxy to the implementation.
        proxy.upgradeTo(address(oracleImpl));

        OPSuccinctL2OutputOracle oracle = OPSuccinctL2OutputOracle(address(proxy));

        OPSuccinctL2OutputOracle.InitParams memory initParams = OPSuccinctL2OutputOracle.InitParams({
            chainId: config.chainId,
            verifierGateway: config.verifierGateway,
            aggregationVkey: config.aggregationVkey,
            rangeVkeyCommitment: config.rangeVkeyCommitment,
            owner: config.owner,
            startingOutputRoot: config.startingOutputRoot,
            rollupConfigHash: config.rollupConfigHash
        });

        oracle.initialize(
            config.submissionInterval,
            config.l2BlockTime,
            config.startingBlockNumber,
            config.startingTimestamp,
            config.proposer,
            config.challenger,
            config.finalizationPeriod,
            initParams
        );

        vm.stopBroadcast();

        return address(oracle);
    }
}
