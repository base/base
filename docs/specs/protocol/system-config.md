# System Config

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Batch Inbox](#batch-inbox)
  - [Batcher Hash](#batcher-hash)
  - [Fee Scalars](#fee-scalars)
    - [Pre-Ecotone Parameters](#pre-ecotone-parameters)
    - [Post-Ecotone Parameters](#post-ecotone-parameters)
    - [Post-Ecotone Scalar Encoding](#post-ecotone-scalar-encoding)
  - [Unsafe Block Signer](#unsafe-block-signer)
  - [L2 Gas Limit](#l2-gas-limit)
  - [Customizable Feature](#customizable-feature)
- [Functionality](#functionality)
  - [System Config Updates](#system-config-updates)
- [Function Specification](#function-specification)
  - [initialize](#initialize)
  - [upgrade](#upgrade)
  - [setFeatureEnabled](#setfeatureenabled)
  - [isFeatureEnabled](#isfeatureenabled)
  - [minimumGasLimit](#minimumgaslimit)
  - [maximumGasLimit](#maximumgaslimit)
  - [unsafeBlockSigner](#unsafeblocksigner)
  - [l1CrossDomainMessenger](#l1crossdomainmessenger)
  - [l1ERC721Bridge](#l1erc721bridge)
  - [l1StandardBridge](#l1standardbridge)
  - [disputeGameFactory](#disputegamefactory)
  - [optimismPortal](#optimismportal)
  - [optimismMintableERC20Factory](#optimismmintableerc20factory)
  - [getAddresses](#getaddresses)
  - [batchInbox](#batchinbox)
  - [startBlock](#startblock)
  - [paused](#paused)
  - [superchainConfig](#superchainconfig)
  - [setUnsafeBlockSigner](#setunsafeblocksigner)
  - [setBatcherHash](#setbatcherhash)
  - [setGasConfig](#setgasconfig)
  - [setGasConfigEcotone](#setgasconfigecotone)
  - [setGasLimit](#setgaslimit)
  - [setEIP1559Params](#seteip1559params)
  - [setOperatorFeeScalars](#setoperatorfeescalars)
  - [resourceConfig](#resourceconfig)
  - [guardian](#guardian)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The `SystemConfig` is a contract on L1 that can emit rollup configuration changes as log events.
The rollup [block derivation process](derivation.md) picks up on these log events and applies the
changes. `SystemConfig` generally acts as the source of truth for configuration values within an
OP Stack chain.

## Definitions

### Batch Inbox

The **Batch Inbox** is the address that Sequencer transaction batches are published to. Sequencers
publish transactions to the Batch Inbox by setting it as the `to` address on a transaction
containing batched L2 transactions either in calldata or as blobdata.

### Batcher Hash

The **Batcher Hash** identifies the sender(s) whose transactions to the [Batch Inbox](#batch-inbox)
will be recognized by the L2 clients for a given OP Chain.

The Batcher Hash is versioned by the first byte of the hash. The structure of the V0 Batcher Hash
is a 32 byte hash defined as follows:

| 1 byte         | 11 bytes | 20 bytes |
| -------------- | -------- | -------- |
| version (0x00) | empty    | address  |

This can also be understood as:

```solidity
bytes32(address(batcher))
```

Where `batcher` is the address of the account that sends transactions to the Batch Inbox. Put
simply, the V0 hash identifies a _single_ address whose transaction batches will be recognized by
L2 clients. This hash is versioned so that it could, for instance, be repurposed to be a commitment
to a list of permitted accounts or some other form of batcher identification.

### Fee Scalars

The **Fee Scalars** are parameters used to calculate the L1 data fee for L2 transactions. These
parameters are also known as Gas Price Oracle (GPO) parameters.

#### Pre-Ecotone Parameters

Before the Ecotone upgrade, these include:

- **Scalar**: A multiplier applied to the L1 base fee, interpreted as a big-endian `uint256`
- **Overhead**: A constant gas overhead, interpreted as a big-endian `uint256`

#### Post-Ecotone Parameters

After the Ecotone upgrade:

- The **Scalar** attribute encodes additional scalar information in a versioned encoding scheme
- The **Overhead** value is ignored and does not affect the L2 state-transition output

#### Post-Ecotone Scalar Encoding

The Scalar is encoded as big-endian `uint256`, interpreted as `bytes32`, and composed as follows:

- Byte `0`: scalar-version byte
- Bytes `[1, 32)`: depending on scalar-version:
  - Scalar-version `0`:
    - Bytes `[1, 28)`: padding, should be zero
    - Bytes `[28, 32)`: big-endian `uint32`, encoding the L1-fee `baseFeeScalar`
    - This version implies the L1-fee `blobBaseFeeScalar` is set to 0
    - If there are non-zero bytes in the padding area, `baseFeeScalar` must be set to MaxUint32
  - Scalar-version `1`:
    - Bytes `[1, 24)`: padding, must be zero
    - Bytes `[24, 28)`: big-endian `uint32`, encoding the `blobBaseFeeScalar`
    - Bytes `[28, 32)`: big-endian `uint32`, encoding the `baseFeeScalar`

The `baseFeeScalar` corresponds to the share of the user-transaction (per byte) in the total
regular L1 EVM gas usage consumed by the data-transaction of the batch-submitter. For blob
transactions, this is the fixed intrinsic gas cost of the L1 transaction.

The `blobBaseFeeScalar` corresponds to the share of a user-transaction (per byte) in the total
blobdata that is introduced by the data-transaction of the batch-submitter.

### Unsafe Block Signer

The **Unsafe Block Signer** is an Ethereum address whose corresponding private key is used to sign
"unsafe" blocks before they are published to L1. This signature allows nodes in the P2P network to
recognize these blocks as the canonical unsafe blocks, preventing denial of service attacks on the
P2P layer.

To ensure that its value can be fetched with a storage proof in a storage layout independent
manner, it is stored at a special storage slot corresponding to
`keccak256("systemconfig.unsafeblocksigner")`.

Unlike other system config parameters, the Unsafe Block Signer only operates on blockchain policy
and is not a consensus level parameter.

### L2 Gas Limit

The **L2 Gas Limit** defines the maximum amount of gas that can be used in a single L2 block.
This parameter ensures that L2 blocks remain of reasonable size to be processed and proven.

Changes to the L2 gas limit are fully applied in the first L2 block with the L1 origin that
introduced the change, as opposed to the 1/1024 adjustments towards a target as seen in limit
updates of L1 blocks.

The gas limit may not be set to a value larger than the
[maximum gas limit](./configurability.md#gas-limit). This is to ensure that L2 blocks are fault
provable and of reasonable size to be processed by the client software.

### Customizable Feature

A **Customizable Feature** is a component of the OP Stack that is maintained as a production-grade
element of the stack behind some sort of toggle. A Customizable Feature is distinct from other
types of feature-flagged code because it is part of the mainline OP Stack and is intended to remain
as a supported configuration option indefinitely. Unlike short-lived feature flags, which exist
only to keep develop releasable while work is in progress, customizable features are permanent,
user-facing options. They must be fully documented, tested in all supported modes, and designed for
long-term maintainability.

## Functionality

### System Config Updates

System config updates are signaled through the `ConfigUpdate(uint256,uint8,bytes)` event. The event
structure includes:

- The first topic determines the version (unknown versions are critical derivation errors)
- The second topic determines the type of update (unknown types are critical derivation errors)
- The remaining event data encodes the configuration update

In version `0`, the following update types are supported:

- Type `0`: `batcherHash` overwrite, as `bytes32` payload
- Type `1`: Pre-Ecotone, `overhead` and `scalar` overwrite, as two packed `uint256` entries. After
  Ecotone upgrade, `overhead` is ignored and `scalar` is interpreted as a versioned encoding that
  updates `baseFeeScalar` and `blobBaseFeeScalar`
- Type `2`: `gasLimit` overwrite, as `uint64` payload
- Type `3`: `unsafeBlockSigner` overwrite, as `address` payload
- Type `4`: `eip1559Params` overwrite, as `uint256` payload encoding denomination and elasticity
- Type `5`: `operatorFeeParams` overwrite, as `uint256` payload encoding scalar and constant

## Function Specification

### initialize

- MUST only be triggerable by the ProxyAdmin or its owner.
- MUST only be triggerable once.
- MUST set the owner of the contract to the provided `_owner` address.
- MUST set the SuperchainConfig contract address.
- MUST set the batcher hash, gas config, gas limit, unsafe block signer, resource config, batch
  inbox, L1 contract addresses, and L2 chain ID.
- MUST set the start block to the current block number if it hasn't been set already.
- MUST validate the resource configuration parameters against system constraints.

### upgrade

- MUST only be triggerable by the ProxyAdmin or its owner.
- MUST set the L2 chain ID to the provided value.
- MUST set the SuperchainConfig contract address to the provided value.
- MUST clear the old dispute game factory address from storage (now derived from OptimismPortal).

### setFeatureEnabled

(+v4.1.0)

- Used to enable or disable a [Customizable Feature](#customizable-feature).
- Takes a bytes32 feature string and a boolean as an input.
- MUST only be triggerable by the ProxyAdmin or its owner.
- MUST toggle the feature flag on or off, based on the value of the boolean.

### isFeatureEnabled

(+v4.1.0)

- Takes a bytes32 feature string as an input.
- Returns true if the feature is enabled and false otherwise.

### minimumGasLimit

Returns the minimum L2 gas limit that can be safely set for the system to operate, calculated as
the sum of the maximum resource limit and the system transaction maximum gas.

### maximumGasLimit

Returns the maximum L2 gas limit that can be safely set for the system to operate.

### unsafeBlockSigner

Returns the address of the [Unsafe Block Signer](#unsafe-block-signer).

### l1CrossDomainMessenger

Returns the address of the L1CrossDomainMessenger contract.

### l1ERC721Bridge

Returns the address of the L1ERC721Bridge contract.

### l1StandardBridge

Returns the address of the L1StandardBridge contract.

### disputeGameFactory

Returns the address of the DisputeGameFactory contract, derived from the OptimismPortal.

### optimismPortal

Returns the address of the OptimismPortal contract.

### optimismMintableERC20Factory

Returns the address of the OptimismMintableERC20Factory contract.

### getAddresses

Returns a consolidated struct containing all the L1 contract addresses.

### batchInbox

Returns the address of the [Batch Inbox](#batch-inbox).

### startBlock

Returns the block number at which the op-node can start searching for logs.

### paused

(-v4.1.0) This function integrates with the [Pause Mechanism](./stage-1.md#pause-mechanism) by
using the chain's `ETHLockbox` address as the [Pause Identifier](./stage-1.md#pause-identifier).
Returns the current pause state of the system by checking if the `SuperchainConfig` is paused for
this chain's `ETHLockbox`.

(+v4.1.0) This function integrates with the [Pause Mechanism](./stage-1.md#pause-mechanism) by
using either the chain's `ETHLockbox` address or the chain's `OptimismPortal` address as the
[Pause Identifier](./stage-1.md#pause-identifier).

- (-v4.1.0) MUST return true if `SuperchainConfig.paused(optimismPortal().ethLockbox())` returns
  true OR if `SuperchainConfig.paused(address(0))` returns true.
- (+v4.1.0) MUST return true if `SuperchainConfig.paused(optimismPortal().ethLockbox())` returns
  true AND the system is configured to use the `ETHLockbox` contract.
- (+v4.1.0) MUST return true if `SuperchainConfig.paused(optimismPortal())` returns true AND the
  system is NOT configured to use the `ETHLockbox` contract.
- (+v4.1.0) MUST return true if `SuperchainConfig.paused(address(0))` returns true.
- MUST return false otherwise.

### superchainConfig

Returns the address of the SuperchainConfig contract that manages the pause state.

### setUnsafeBlockSigner

Allows the owner to update the [Unsafe Block Signer](#unsafe-block-signer) address.

- MUST revert if called by an address other than the owner.
- MUST update the unsafe block signer address.
- MUST emit a ConfigUpdate event with the UpdateType.UNSAFE_BLOCK_SIGNER type.

### setBatcherHash

Allows the owner to update the [Batcher Hash](#batcher-hash).

- MUST revert if called by an address other than the owner.
- MUST update the batcher hash.
- MUST emit a ConfigUpdate event with the UpdateType.BATCHER type.

### setGasConfig

Allows the owner to update the gas configuration parameters (pre-Ecotone).

- MUST revert if called by an address other than the owner.
- MUST revert if the scalar exceeds the maximum allowed value (no upper 8 bits should be set).
- MUST update the overhead and scalar values.
- MUST emit a ConfigUpdate event with the UpdateType.FEE_SCALARS type.

### setGasConfigEcotone

Allows the owner to update the gas configuration parameters (post-Ecotone).

- MUST revert if called by an address other than the owner.
- MUST update the basefeeScalar and blobbasefeeScalar values.
- MUST update the scalar value with the versioned encoding.
- MUST emit a ConfigUpdate event with the UpdateType.FEE_SCALARS type.

### setGasLimit

Allows the owner to update the [L2 Gas Limit](#l2-gas-limit).

- MUST revert if called by an address other than the owner.
- MUST revert if the gas limit is less than the minimum gas limit.
- MUST revert if the gas limit is greater than the maximum gas limit.
- MUST update the gas limit.
- MUST emit a ConfigUpdate event with the UpdateType.GAS_LIMIT type.

### setEIP1559Params

Allows the owner to update the EIP-1559 parameters of the chain.

- MUST revert if called by an address other than the owner.
- MUST revert if the denominator is less than 1.
- MUST revert if the elasticity is less than 1.
- MUST update the eip1559Denominator and eip1559Elasticity values.
- MUST emit a ConfigUpdate event with the UpdateType.EIP_1559_PARAMS type.

### setOperatorFeeScalars

Allows the owner to update the operator fee parameters.

- MUST revert if called by an address other than the owner.
- MUST update the operatorFeeScalar and operatorFeeConstant values.
- MUST emit a ConfigUpdate event with the UpdateType.OPERATOR_FEE_PARAMS type.

### resourceConfig

Returns the current resource metering configuration.

- MUST perform validation checks when setting the resource config:
  - Minimum base fee must be less than or equal to maximum base fee
  - Base fee change denominator must be greater than 1
  - Max resource limit plus system transaction gas must be less than or equal to the L2 gas limit
  - Elasticity multiplier must be greater than 0
  - No precision loss when computing target resource limit

### guardian

Returns the address of the guardian from the SuperchainConfig contract.

- MUST return the result of a call to `superchainConfig.guardian()`.
