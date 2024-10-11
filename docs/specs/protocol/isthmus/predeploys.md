# Overview

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Constants](#constants)
- [Predeploys](#predeploys)
  - [ProxyAdmin](#proxyadmin)
    - [Rationale](#rationale)
  - [L1Block](#l1block)
    - [Storage](#storage)
    - [Interface](#interface)
      - [`setIsthmus`](#setisthmus)
      - [`setConfig`](#setconfig)
      - [`getConfig`](#getconfig)
  - [FeeVault](#feevault)
    - [Interface](#interface-1)
      - [`config`](#config)
  - [L2CrossDomainMessenger](#l2crossdomainmessenger)
    - [Interface](#interface-2)
  - [L2ERC721Bridge](#l2erc721bridge)
    - [Interface](#interface-3)
  - [L2StandardBridge](#l2standardbridge)
    - [Interface](#interface-4)
  - [OptimismMintableERC721Factory](#optimismmintableerc721factory)
- [Security Considerations](#security-considerations)
  - [GovernanceToken](#governancetoken)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

This upgrade enables a deterministic L2 genesis state by moving all network
specific configuration out of the initial L2 genesis state. All network specific
configuration is sourced from deposit transactions during the initialization
of the `SystemConfig`.

## Constants

| Name | Value | Definition |
| --------- | ------------------------- | -- |
| `ConfigType` | `uint8` | An enum representing the type of config being set |
| `WithdrawalNetwork` | `uint8(0)` or `uint8(1)` | `0` means withdraw to L1, `1` means withdraw to L2  |
| `RECIPIENT` | `address` | The account that will receive funds sent out of the `FeeVault` |
| `MIN_WITHDRAWAL_AMOUNT` | `uint256` | The minimum amount of native asset held in the `FeeVault` before withdrawal is authorized |
| Fee Vault Config | `bytes32` | `bytes32((WithdrawalNetwork << 248) \|\| uint256(uint88(MIN_WITHDRAWAL_AMOUNT)) \|\| uint256(uint160(RECIPIENT)))` |
| `BASE_FEE_VAULT_CONFIG` | `bytes32(uint256(keccak256("opstack.basefeevaultconfig")) - 1)` | The Fee Vault Config for the `BaseFeeVault` |
| `L1_FEE_VAULT_CONFIG` | `bytes32(uint256(keccak256("opstack.l1feevaultconfig")) - 1)` | The Fee Vault Config for the `L1FeeVault` |
| `SEQUENCER_FEE_VAULT_CONFIG` | `bytes32(uint256(keccak256("opstack.sequencerfeevaultconfig")) - 1)` | The Fee Vault Config for the `SequencerFeeVault` |
| `L1_CROSS_DOMAIN_MESSENGER_ADDRESS` | `bytes32(uint256(keccak256("opstack.l1crossdomainmessengeraddress")) - 1)` | `abi.encode(address(L1CrossDomainMessengerProxy))` |
| `L1_ERC_721_BRIDGE_ADDRESS` | `bytes32(uint256(keccak256("opstack.l1erc721bridgeaddress")) - 1)` | `abi.encode(address(L1ERC721BridgeProxy))` |
| `L1_STANDARD_BRIDGE_ADDRESS` | `bytes32(uint256(keccak256("opstack.l1standardbridgeaddress")) - 1)` | `abi.encode(address(L1StandardBridgeProxy))` |
| `REMOTE_CHAIN_ID` | `bytes32(uint256(keccak256("opstack.remotechainid")) - 1)` | Chain ID of the remote chain |

## Predeploys

All network specific configuration is moved to a single contract, the `L1Block` predeploy.
All predeploys make calls to the `L1Block` contract to fetch network specific configuration
rather than reading it from local state.

```mermaid
graph LR
  subgraph L1
  SystemConfig -- "setConfig(uint8,bytes)" --> OptimismPortal
  end
  subgraph L2
  L1Block
  BaseFeeVault -- "getConfig(ConfigType.GAS_PAYING_TOKEN)(address,uint256,uint8)" --> L1Block
  SequencerFeeVault -- "getConfig(ConfigType.SEQUENCER_FEE_VAULT_CONFIG)(address,uint256,uint8)" --> L1Block
  L1FeeVault -- "getConfig(ConfigType.L1_FEE_VAULT_CONFIG)(address,uint256,uint8)" --> L1Block
  L2CrossDomainMessenger -- "getConfig(ConfigType.L1_CROSS_DOMAIN_MESSENGER_ADDRESS)(address)" --> L1Block
  L2StandardBridge -- "getConfig(ConfigType.L1_STANDARD_BRIDGE_ADDRESS)(address)" --> L1Block
  L2ERC721Bridge -- "getConfig(ConfigType.L1_ERC721_BRIDGE_ADDRESS)(address)" --> L1Block
  OptimismMintableERC721Factory -- "getConfig(ConfigType.REMOTE_CHAIN_ID)(uint256)" --> L1Block
  end
  OptimismPortal -- "setConfig(uint8,bytes)" --> L1Block
```

### ProxyAdmin

The `ProxyAdmin` is updated to have its `owner` be the `DEPOSITOR_ACCOUNT`.
This means that it can be deterministically called by network upgrade transactions
or by special deposit transactions emitted by the `OptimismPortal` that assume
the identity of the `DEPOSITOR_ACCOUNT`.

#### Rationale

It is much easier to manage the overall roles of the full system under this model.
The owner of the `ProxyAdmin` can upgrade any of the predeploys, meaning it can
write storage slots that correspond to withdrawals. This ensures that only the
system or a chain governor can issue upgrades to the predeploys.

### L1Block

#### Storage

The following storage slots are defined:

- `BASE_FEE_VAULT_CONFIG`
- `L1_FEE_VAULT_CONFIG`
- `SEQUENCER_FEE_VAULT_CONFIG`
- `L1_CROSS_DOMAIN_MESSENGER_ADDRESS`
- `L1_ERC_721_BRIDGE_ADDRESS`
- `L1_STANDARD_BRIDGE_ADDRESS`
- `REMOTE_CHAIN_ID`

Each slot MUST have a defined `ConfigType` that authorizes the setting of the storage slot
via a deposit transaction from the `DEPOSITOR_ACCOUNT`.

#### Interface

##### `setIsthmus`

This function is meant to be called once on the activation block of the holocene network upgrade.
It MUST only be callable by the `DEPOSITOR_ACCOUNT` once. When it is called, it MUST call
call each getter for the network specific config and set the returndata into storage.

##### `setConfig`

This function MUST only be callable by the `DEPOSITOR_ACCOUNT`. It modifies the storage directly
of the `L1Block` contract. It MUST handle all defined `ConfigType`s. To ensure a simple ABI, the
`bytes` value MUST be abi decoded based on the `ConfigType`.

```solidity
function setConfig(ConfigType,bytes)
```

Note that `ConfigType` is an enum which is an alias for a `uint8`.

##### `getConfig`

This function is called by each contract with the appropriate `ConfigType` to fetch
the network specific configuration. Using this pattern reduces the ABI of the `L1Block`
contract by removing the need for special getters for each piece of config.

```solidity
function getConfig(ConfigType)(bytes)
```

The caller needs to ABI decode the data into the desired type.

### FeeVault

The following changes apply to each of the `BaseFeeVault`, the `L1FeeVault` and the `SequencerFeeVault`.

#### Interface

The following functions are updated to read from the `L1Block` contract:

- `recipient()(address)`
- `withdrawalNetwork()(WithdrawalNetwork)`
- `minWithdrawalAmount()(uint256)`
- `withdraw()`

| Name | Call |
| ---- | -------- |
| `BaseFeeVault` | `L1Block.getConfig(ConfigType.BASE_FEE_VAULT_CONFIG)` |
| `SequencerFeeVault` | `L1Block.getConfig(ConfigType.SEQUENCER_FEE_VAULT_CONFIG)` |
| `L1FeeVault` | `L1Block.getConfig(ConfigType.L1_FEE_VAULT_CONFIG)` |

##### `config`

A new function is added to fetch the full Fee Vault Config.

```solidity
function config()(address,uint256,WithdrawalNetwork)
```

### L2CrossDomainMessenger

#### Interface

The following functions are updated to read from the `L1Block` contract by calling `L1Block.getConfig(ConfigType.L1_CROSS_DOMAIN_MESSENGER_ADDRESS)`:

- `otherMessenger()(address)`
- `OTHER_MESSENGER()(address)`

### L2ERC721Bridge

#### Interface

The following functions are updated to read from the `L1Block` contract by calling `L1Block.getConfig(ConfigType.L1_ERC721_BRIDGE_ADDRESS)`:

- `otherBridge()(address)`
- `OTHER_BRIDGE()(address)`

### L2StandardBridge

#### Interface

The following functions are updated to read from the `L1Block` contract by calling `L1Block.getConfig(ConfigType.L1_STANDARD_BRIDGE_ADDRESS)`:

- `otherBridge()(address)`
- `OTHER_BRIDGE()(address)`

### OptimismMintableERC721Factory

The chain id is no longer read from storage but instead is read from the `L1Block` contract by calling
`L1Block.getConfig(ConfigType.REMOTE_CHAIN_ID)`

## Security Considerations

### GovernanceToken

The predeploy defined by `GovernanceToken` should be an empty account until it is defined by
a future hardfork.
