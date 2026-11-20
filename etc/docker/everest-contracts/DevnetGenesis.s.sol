// SPDX-License-Identifier: MIT
pragma solidity 0.8.15;

import { SystemDeploy } from "scripts/deploy/SystemDeploy.s.sol";
import { L2Genesis } from "scripts/L2Genesis.s.sol";
import { Fork } from "scripts/libraries/Config.sol";
import { Types } from "scripts/libraries/Types.sol";
import { Hash, Proposal } from "src/libraries/bridge/Types.sol";

/// @dev Calls the pinned release's deployer in the offline genesis VM. The default
/// execution devnet has no proof producers and does not register a dispute game.
contract DevnetL1 is SystemDeploy {
    function build(
        uint256 l1ChainId, uint256 l2ChainId, address owner, address batcher, address sequencer,
        uint64 gasLimit, bytes32 salt, uint32 baseFeeScalar, uint32 blobFeeScalar
    ) external returns (string memory) {
        vm.chainId(l1ChainId);
        DeployInput memory input;
        input.implementationsInput.withdrawalDelaySeconds = 302400;
        input.implementationsInput.proofMaturityDelaySeconds = 604800;
        input.implementationsInput.disputeGameFinalityDelaySeconds = 302400;
        input.implementationsInput.guardian = owner;
        input.opChainInput.roles = Types.Roles(owner, owner, batcher, sequencer, address(0));
        input.opChainInput.basefeeScalar = baseFeeScalar;
        input.opChainInput.blobBasefeeScalar = blobFeeScalar;
        input.opChainInput.l2ChainId = l2ChainId;
        input.opChainInput.startingAnchorRoot = Proposal(Hash.wrap(bytes32(uint256(1))), 0);
        input.opChainInput.saltMixer = vm.toString(salt);
        input.opChainInput.gasLimit = gasLimit;
        input.opChainInput.initialMinimumProtocolVersion = 1 << 32;
        DeployOutput memory out = deploy(input);
        return addresses(out);
    }

    /// @dev Names match op-deployer's l1-addresses.json. OP-only contracts stay absent.
    function addresses(DeployOutput memory out) internal returns (string memory) {
        Types.DeployOutput memory c = out.opChain;
        Types.Implementations memory i = out.impls;
        string memory k = "l1";
        vm.serializeAddress(k, "OpChainProxyAdminImpl", address(c.opChainProxyAdmin));
        vm.serializeAddress(k, "AddressManagerImpl", address(c.addressManager));
        vm.serializeAddress(k, "L1ERC721BridgeProxy", address(c.l1ERC721BridgeProxy));
        vm.serializeAddress(k, "SystemConfigProxy", address(c.systemConfigProxy));
        vm.serializeAddress(k, "OptimismMintableERC20FactoryProxy", address(c.optimismMintableERC20FactoryProxy));
        vm.serializeAddress(k, "L1StandardBridgeProxy", address(c.l1StandardBridgeProxy));
        vm.serializeAddress(k, "L1CrossDomainMessengerProxy", address(c.l1CrossDomainMessengerProxy));
        vm.serializeAddress(k, "OptimismPortalProxy", address(c.optimismPortalProxy));
        vm.serializeAddress(k, "DisputeGameFactoryProxy", address(c.disputeGameFactoryProxy));
        vm.serializeAddress(k, "AnchorStateRegistryProxy", address(c.anchorStateRegistryProxy));
        vm.serializeAddress(k, "DelayedWethPermissionlessGameProxy", address(c.delayedWETHProxy));
        vm.serializeAddress(k, "ProtocolVersionsProxy", address(c.protocolVersionsProxy));
        vm.serializeAddress(k, "L1ERC721BridgeImpl", i.l1ERC721BridgeImpl);
        vm.serializeAddress(k, "OptimismPortalImpl", i.optimismPortalImpl);
        vm.serializeAddress(k, "SystemConfigImpl", i.systemConfigImpl);
        vm.serializeAddress(k, "OptimismMintableERC20FactoryImpl", i.optimismMintableERC20FactoryImpl);
        vm.serializeAddress(k, "L1CrossDomainMessengerImpl", i.l1CrossDomainMessengerImpl);
        vm.serializeAddress(k, "L1StandardBridgeImpl", i.l1StandardBridgeImpl);
        vm.serializeAddress(k, "DisputeGameFactoryImpl", i.disputeGameFactoryImpl);
        vm.serializeAddress(k, "AnchorStateRegistryImpl", i.anchorStateRegistryImpl);
        vm.serializeAddress(k, "DelayedWethImpl", i.delayedWETHImpl);
        return vm.serializeAddress(k, "ProtocolVersionsImpl", i.protocolVersionsImpl);
    }
}

contract DevnetL2 is L2Genesis {
    function build(
        uint256 l1ChainId, uint256 l2ChainId, address owner, address payable messenger,
        address payable bridge, address payable erc721Bridge, uint256 minWithdrawal, bool fundAccounts
    ) external {
        Input memory input;
        input.l1ChainID = l1ChainId;
        input.l2ChainID = l2ChainId;
        input.l1CrossDomainMessengerProxy = messenger;
        input.l1StandardBridgeProxy = bridge;
        input.l1ERC721BridgeProxy = erc721Bridge;
        input.opChainProxyAdminOwner = owner;
        input.sequencerFeeVaultRecipient = owner;
        input.baseFeeVaultRecipient = owner;
        input.l1FeeVaultRecipient = owner;
        input.operatorFeeVaultRecipient = owner;
        input.sequencerFeeVaultMinimumWithdrawalAmount = minWithdrawal;
        input.baseFeeVaultMinimumWithdrawalAmount = minWithdrawal;
        input.l1FeeVaultMinimumWithdrawalAmount = minWithdrawal;
        input.operatorFeeVaultMinimumWithdrawalAmount = minWithdrawal;
        // The devnet pays fees on L2 (LOCAL), not via withdrawals to L1.
        input.sequencerFeeVaultWithdrawalNetwork = 1;
        input.baseFeeVaultWithdrawalNetwork = 1;
        input.l1FeeVaultWithdrawalNetwork = 1;
        input.operatorFeeVaultWithdrawalNetwork = 1;
        input.fork = uint256(Fork.JOVIAN);
        input.fundDevAccounts = fundAccounts;
        run(input);
    }
}
