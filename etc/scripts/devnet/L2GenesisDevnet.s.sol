// SPDX-License-Identifier: MIT
pragma solidity 0.8.15;

import { Script } from "lib/forge-std/src/Script.sol";
import { L2Genesis } from "scripts/L2Genesis.s.sol";
import { DeployConfig } from "scripts/deploy/DeployConfig.s.sol";

/// @title L2GenesisDevnet
/// @notice Thin wrapper that reads deploy-config + deployed L1 addresses,
///         runs L2Genesis.run(Input), and dumps the resulting state.
contract L2GenesisDevnet is Script {
    function run() public {
        // Read deploy config
        DeployConfig cfg = new DeployConfig();
        cfg.read(vm.envString("DEPLOY_CONFIG_PATH"));

        // Read deployed L1 addresses
        string memory artifactJson = vm.readFile(vm.envString("L1_DEPLOY_ARTIFACT"));
        address l1CDMProxy = vm.parseJsonAddress(artifactJson, ".L1CrossDomainMessengerProxy");
        address l1BridgeProxy = vm.parseJsonAddress(artifactJson, ".L1StandardBridgeProxy");
        address l1ERC721Proxy = vm.parseJsonAddress(artifactJson, ".L1ERC721BridgeProxy");

        // Build Input struct
        L2Genesis.Input memory input = L2Genesis.Input({
            l1ChainID: cfg.l1ChainId(),
            l2ChainID: cfg.l2ChainId(),
            l1CrossDomainMessengerProxy: payable(l1CDMProxy),
            l1StandardBridgeProxy: payable(l1BridgeProxy),
            l1ERC721BridgeProxy: payable(l1ERC721Proxy),
            opChainProxyAdminOwner: cfg.proxyAdminOwner(),
            sequencerFeeVaultRecipient: cfg.sequencerFeeVaultRecipient(),
            sequencerFeeVaultMinimumWithdrawalAmount: cfg.sequencerFeeVaultMinimumWithdrawalAmount(),
            sequencerFeeVaultWithdrawalNetwork: cfg.sequencerFeeVaultWithdrawalNetwork(),
            baseFeeVaultRecipient: cfg.baseFeeVaultRecipient(),
            baseFeeVaultMinimumWithdrawalAmount: cfg.baseFeeVaultMinimumWithdrawalAmount(),
            baseFeeVaultWithdrawalNetwork: cfg.baseFeeVaultWithdrawalNetwork(),
            l1FeeVaultRecipient: cfg.l1FeeVaultRecipient(),
            l1FeeVaultMinimumWithdrawalAmount: cfg.l1FeeVaultMinimumWithdrawalAmount(),
            l1FeeVaultWithdrawalNetwork: cfg.l1FeeVaultWithdrawalNetwork(),
            operatorFeeVaultRecipient: cfg.operatorFeeVaultRecipient(),
            operatorFeeVaultMinimumWithdrawalAmount: cfg.operatorFeeVaultMinimumWithdrawalAmount(),
            operatorFeeVaultWithdrawalNetwork: cfg.operatorFeeVaultWithdrawalNetwork(),
            fork: 5, // JOVIAN — all forks active at genesis
            fundDevAccounts: cfg.fundDevAccounts()
        });

        // Run L2Genesis
        L2Genesis genesis = new L2Genesis();
        genesis.run(input);

        // Dump the resulting state
        vm.dumpState(vm.envOr("L2_GENESIS_STATE_DUMP", string("state-dump.json")));
    }
}
