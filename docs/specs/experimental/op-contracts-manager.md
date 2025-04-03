# OP Contracts Manager

[Optimism Monorepo releases]: https://github.com/ethereum-optimism/optimism/releases
[contract releases]: https://github.com/ethereum-optimism/optimism/blob/develop/packages/contracts-bedrock/meta/VERSIONING.md
[standard configuration]: ../protocol/configurability.md
[superchain registry]: https://github.com/ethereum-optimism/superchain-registry
[ethereum-lists/chains]: https://github.com/ethereum-lists/chains
[Batch Inbox]: ../protocol/configurability.md#consensus-parameters

The OP Contracts Manager is a contract that deploys the L1 contracts for an OP Stack chain in a single
transaction. It provides a minimal set of user-configurable parameters to ensure that the resulting
chain meets the [standard configuration] requirements.

The version deployed is always a governance-approved contract release. The set
of governance approved [contract releases] can be found on the
[Optimism Monorepo releases] page, and is the set of releases named
`op-contracts/vX.Y.Z`.

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Getter Methods](#getter-methods)
- [Deployment](#deployment)
  - [Interface](#interface)
    - [`deploy`](#deploy)
  - [Implementation](#implementation)
    - [Batch Inbox Address](#batch-inbox-address)
    - [Contract Deployments](#contract-deployments)
- [Upgrading](#upgrading)
  - [Interface](#interface-1)
    - [`upgrade`](#upgrade)
  - [Implementation](#implementation-1)
    - [Requirements on the OP Chain contracts](#requirements-on-the-op-chain-contracts)
- [Adding game types](#adding-game-types)
  - [Interface](#interface-2)
    - [`addGameType`](#addgametype)
  - [Implementation](#implementation-2)
- [Security Considerations](#security-considerations)
  - [Chain ID Source of Truth](#chain-id-source-of-truth)
  - [Chain ID Frontrunning](#chain-id-frontrunning)
  - [Chain ID Value](#chain-id-value)
  - [Proxy Admin Owner](#proxy-admin-owner)
  - [Safely using `DELEGATECALL`](#safely-using-delegatecall)
  - [Atomicity of upgrades](#atomicity-of-upgrades)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The OP Contracts Manager refers to a series of contracts, of which a new singleton is deployed
for each new release of the OP Stack contracts.

The OP Contracts Manager corresponding to each release can be used to:

1. Deploy a new OP chain.
2. Upgrade the contracts for an existing OP chain from the previous release to the new release.
3. Orchestrate adding a new game type on a per-chain basis

Upgrades must be performed by the [Proxy Admin Owner](../protocol/stage-1.md#roles-for-stage-1) Safe for a chain.

## Getter Methods

The following interface defines the available getter methods:

```solidity
/// @notice Returns the release string with the format `op-contracts/vX.Y.Z`. 
/// Appends "-rc" if this is a release candidate.
function l1ContractsRelease() external view returns (string memory);
/// @notice Addresses of the Blueprint contracts.
function blueprints() external view returns (Blueprints memory);
/// @notice Maps an L2 chain ID to an L1 batch inbox address
function chainIdToBatchInboxAddress(uint256 _l2ChainId) external pure returns (address);
/// @notice Addresses of the latest implementation contracts.
function implementations() external view returns (Implementations memory);
/// @notice Address of the ProtocolVersions contract shared by all chains.
function protocolVersions() external view returns (address);
/// @notice Address of the SuperchainConfig contract shared by all chains.
function superchainConfig() external view returns (ISuperchainConfig);
/// @notice Semver version specific to the OPContractsManager
function version() external view returns (string memory);
```

## Deployment

### Interface

#### `deploy`

The `deploy` method is used to deploy the full set of L1 contracts required to setup a new OP Stack
chain that complies with the [standard configuration]. It has the following interface:

```solidity
/// @notice Represents the roles that can be set when deploying a standard OP Stack chain.
struct Roles {
    address opChainProxyAdminOwner;
    address systemConfigOwner;
    address batcher;
    address unsafeBlockSigner;
    address proposer;
    address challenger;
}

/// @notice The full set of inputs to deploy a new OP Stack chain.
struct DeployInput {
    Roles roles;
    uint32 basefeeScalar;
    uint32 blobBasefeeScalar;
    uint256 l2ChainId;
    bytes startingAnchorRoot;
    string saltMixer;
    uint64 gasLimit;
    uint32 disputeGameType;
    bytes32 disputeAbsolutePrestate;
    uint256 disputeMaxGameDepth;
    uint256 disputeSplitDepth;
    uint64 disputeClockExtension;
    uint64 disputeMaxClockDuration;
}

/// @notice The full set of outputs from deploying a new OP Stack chain.
struct DeployOutput {
    IProxyAdmin opChainProxyAdmin;
    IAddressManager addressManager;
    IL1ERC721Bridge l1ERC721BridgeProxy;
    ISystemConfig systemConfigProxy;
    IOptimismMintableERC20Factory optimismMintableERC20FactoryProxy;
    IL1StandardBridge l1StandardBridgeProxy;
    IL1CrossDomainMessenger l1CrossDomainMessengerProxy;
    // Fault proof contracts below.
    IOptimismPortal2 optimismPortalProxy;
    IDisputeGameFactory disputeGameFactoryProxy;
    IAnchorStateRegistry anchorStateRegistryProxy;
    IFaultDisputeGame faultDisputeGame;
    IPermissionedDisputeGame permissionedDisputeGame;
    IDelayedWETH delayedWETHPermissionedGameProxy;
    IDelayedWETH delayedWETHPermissionlessGameProxy;
}

/// @notice Deploys a new OP Chain
/// @param _input DeployInput containing chain specific config information.
/// @return DeployOutput containing the new addresses.
function deploy(DeployInput calldata _input) external returns (DeployOutput memory)
```

The `l2ChainId` has the following restrictions:

- It must not be equal to 0.
- It must not be equal to the chain ID of the chain the OP Contracts Manager is
  deployed on.
- It must not be equal to a chain ID that is already present in the
  [ethereum-lists/chains] repository. This is not enforced onchain, but may matter
  for future versions of OP Contracts Manager that handle upgrades.

On success, the following event is emitted:

```solidity
event Deployed(uint256 indexed l2ChainId, address indexed deployer, bytes deployOutput);
```

This method reverts on failure. This occurs when:

- The input `l2ChainId` does not comply with the enforced restrictions above.
- The resulting configuration is not compliant with the [standard configuration].

### Implementation

#### Batch Inbox Address

The chain's [Batch Inbox] address is computed at deploy time using the recommend approach defined
in the [standard configuration]. This improves UX by removing an input, and ensures uniqueness of
the batch inbox addresses.

#### Contract Deployments

All contracts deployed by the OP Contracts Manager are deployed with `CREATE2`, using the following salt:

```solidity
keccak256(abi.encode(_l2ChainId, _saltMixer, _contractName));
```

The `saltMixer` value is provided as a field in the `DeployInput` struct.

This provides the following benefits:

- Contract addresses for a chain can be derived as a function of chain ID without any RPC calls.
- Chain ID uniqueness is enforced for free, as a deploy using the same chain ID
  will result in attempting to deploy to the same address, which is prohibited by
  the EVM.
  - This property is contingent on the proxy and `AddressManager` code not
    changing when OP Contracts Manager is upgraded. Both of these are not planned to
    change.
  - The OP Contracts Manager is not responsible for enforcing chain ID uniqueness, so it is acceptable
    if this property is not preserved in future versions of the OP Contracts Manager.

## Upgrading

### Interface

#### `upgrade`

The `upgrade` method is used by the Proxy Admin Owner to upgrade the full set of L1 contracts for
all chains that it controls.

It has the following interface:

```solidity
/// @notice The input required to identify a chain for upgrading, along with new prestate hashes
struct OpChainConfig {
    ISystemConfig systemConfigProxy;
    IProxyAdmin proxyAdmin;
    bytes32 absolutePrestate;
}

/// @notice Upgrades a set of chains to the latest implementation contracts
/// @param _opChainConfigs Array of OpChain structs, one per chain to upgrade
/// @dev This function is intended to be called via DELEGATECALL from the Proxy Admin Owner Safe
function upgrade(OpChainConfig[] memory _opChainConfigs) external
```

For each chain successfully upgraded, the following event is emitted:

```solidity
event Upgraded(uint256 indexed l2ChainId, ISystemConfig indexed systemConfig, address indexed upgrader);
```

This method reverts if the upgrade is not successful for any of the chains.

### Implementation

The high level logic of the upgrade method is as follows:

1. The Proxy Admin Owner Safe will `DELEGATECALL` to the `OPCM.upgrade()` method.
2. The SuperchainConfig contract will be upgraded, if not yet done.
3. The ProtocolVersions contract will be upgraded, if not yet done.
4. For each `_systemConfig`, the list of addresses in the chain is retrieved.
5. For each address:
   1. If it is receiving new state variables, a call is made to:
      `ProxyAdmin.upgradeAndCall()` with data corresponding to the new value being set.
   2. Otherwise, `ProxyAdmin.upgrade()` is called on that address.

This approach requires that any contracts which are receiving new state variables
have an upgrade function which:

1. Writes the new state variables
2. MUST only be callable once

#### Requirements on the OP Chain contracts

In general, all contracts used in an OP Chain SHOULD be proxied with a single shared implementation.
This means that all values which are not constant across OP Chains SHOULD be held in storage rather
than the bytecode of the implementation.

Any contracts which do not meet this requirement will need to be deployed by the `upgrade()`
function, increasing the cost and reducing the number of OP Chains which can be atomically upgraded.

## Adding game types

Because different OP Chains within a Superchain may use different dispute game types, and are
expected to move from a permissioned to permissionless game over time, an `addGameType()` method is
provided to enable adding a new game type to multiple games at once.

### Interface

#### `addGameType`

The `addGameType` method is used to orchestrate the actions required to add a new game type to one
or more chains.

```solidity
struct AddGameInput {
    string saltMixer;
    ISystemConfig systemConfig;
    IProxyAdmin proxyAdmin;
    IDelayedWETH delayedWETH;
    uint32 disputeGameType;
    bytes32 disputeAbsolutePrestate;
    uint256 disputeMaxGameDepth;
    uint256 disputeSplitDepth;
    uint64 disputeClockExtension;
    uint64 disputeMaxClockDuration;
    uint256 initialBond;
    IBigStepper vm;
    bool permissioned;
}

struct AddGameOutput {
    IDelayedWETH delayedWETH;
    IFaultDisputeGame faultDisputeGame;
}

/// @notice addGameType deploys a new dispute game and links it to the DisputeGameFactory. The inputted _gameConfigs
/// must be added in ascending GameType order.
function addGameType(AddGameInput[] memory _gameConfigs) external returns (AddGameOutput[] memory)
```

On success, the following event is emitted:

```solidity
event GameTypeAdded(uint256 indexed l2ChainId, uint32 indexed gameType, address indexed deployer);
```

### Implementation

The high level logic of the `addGameType` method is as follows (for each chain):

1. Deploy and initialize new `DelayedWethProxy` for the new game type, if one hasn't already been specified.
2. Deploy a new dispute game contract, based on the specified game type. The constructor args must be provided as
   arguments, unless otherwise specified in the table below.
3. Read the `DisputeGameFactory` address from the `SystemConfig`.
4. Call `DisputeGameFactory.setImplementation()` to register the new game.

| Name                | Type    | Description                                        | Source                                     |
| ------------------- | ------- | -------------------------------------------------- | ------------------------------------------ |
| weth                | address | Address of the DelayedWeth contract | Newly deployed contract, or provided       |
| anchorStateRegistry | address | Registry contract address                          | Copied from existing `PermissionedGame`    |
| l2ChainId           | uint256 | Chain ID of the L2 network                         | Copied from existing `PermissionedGame`    |

## Security Considerations

### Chain ID Source of Truth

One of the implicit restrictions on chain ID is that `deploy` can only be called
once per chain ID, because contract addresses are a function of chain ID. However,
future versions of OP Contracts Manager may:

- Change the Proxy code used, which would allow a duplicate chain ID to be deployed
  if there is only the implicit check.
- Manage upgrades, which will require "registering" existing pre-OP Contracts Manager
  chains in the OP Contracts Manager. Registration will be a privileged action, and the [superchain registry] will be
  used as the source of truth for registrations.

This means, for example, if deploying a chain with a chain ID of 10—which is OP
Mainnet's chain ID—deployment will execute successfully, but the entry in OP
Contracts Manager may be overwritten in a future upgrade. Therefore, chain ID
uniqueness is not enforced by the OP Contracts Manager, and it is strongly
recommended to only use chain IDs that are not already present in the
[ethereum-lists/chains] repository.

### Chain ID Frontrunning

Contract addresses for a chain are a function of chain ID, which implies you
can counterfactually compute and use those chain addresses before the chain is
deployed. However, this property should not be relied upon—new chain deployments
are permissionless, so you cannot guarantee usage of a given chain ID, as deploy
transactions can be frontrun.

### Chain ID Value

While not specific to OP Contracts Manager, when choosing a chain ID is important
to consider that not all chain IDs are well supported by tools. For example,
MetaMask [only supports](https://gist.github.com/rekmarks/a47bd5f2525936c4b8eee31a16345553)
chain IDs up to `4503599627370476`, well below the max allowable 256-bit value.

OP Contracts Manager does not consider factors such as these. The EVM supports
256-bit chain IDs, so OP Contracts Manager sticks with the full 256-bit range to
maximize compatibility.

### Proxy Admin Owner

The proxy admin owner is a very powerful role, as it allows upgrading protocol
contracts. When choosing the initial proxy admin owner, a Safe is recommended
to ensure admin privileges are sufficiently secured.

### Safely using `DELEGATECALL`

Because a Safe will `DELEGATECALL` to the `upgrade()` and `addGameType()` methods, it is
critical that no storage writes occur. This should be enforced in multiple ways, including:

- By static analysis of the `upgrade()` and `addGameType()` methods during the development process.
- By simulating and verifying the state changes which occur in the Proxy Admin Owner Safe prior to execution.

### Atomicity of upgrades

Although atomicity of a superchain upgrade is not essential for many types of upgrade, it will
at times be necessary. It is certainly always desirable for operational reasons.

For this reason, efficiency should be kept in mind when designing the upgrade path. When the size of
the superchain reaches a size that nears the block gas limit, upgrades may need to be broken up into
stages, so that components which must be upgrade atomically can be. For example, all
`OptimismPortal` contracts may need to be upgraded in one transaction, followed by another
transaction which upgrades all `L1CrossDomainMessenger` contracts.
