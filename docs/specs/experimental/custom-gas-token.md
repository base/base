# Custom Gas Token

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Constants](#constants)
- [Properties of a Gas Paying Token](#properties-of-a-gas-paying-token)
- [Configuring the Gas Paying Token](#configuring-the-gas-paying-token)
- [OptimismPortal](#optimismportal)
  - [`depositERC20Transaction`](#depositerc20transaction)
  - [`depositTransaction`](#deposittransaction)
- [StandardBridge](#standardbridge)
- [CrossDomainMessenger](#crossdomainmessenger)
- [User Flow](#user-flow)
- [Security Considerations](#security-considerations)
  - [OptimismPortal Token Allowance](#optimismportal-token-allowance)
  - [Decimal Scaling](#decimal-scaling)
  - [Interoperability Support](#interoperability-support)
  - [Wrapped Ether](#wrapped-ether)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

Custom gas token is also known as "custom L2 native token". It allows for an ERC20 token to be collateralized on L1
which backs the native asset on L2. The native asset is used to pay for compute on the network in the form of gas
and is represented by EVM opcodes such as `CALLVALUE` (`msg.value`).

Both the L1 and L2 smart contract systems MUST be able to introspect on the address of the gas paying token to
ensure the security of the system.

## Constants

| Constant          | Value                           | Description |
| ----------------- | ------------------------------- | ----------- |
| `ETHER_TOKEN_ADDRESS` |  `address(0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE)` | Represents ether for gas paying asset |
| `DEPOSITOR_ACCOUNT` | `address(0xDeaDDEaDDeAdDeAdDEAdDEaddeAddEAdDEAd0001)` | Account with auth permissions on `L1Block` contract |

## Properties of a Gas Paying Token

The gas paying token MUST satisfy the standard [ERC20](https://eips.ethereum.org/EIPS/eip-20) interface.
It MUST be a L1 contract.

The gas paying token cannot:

- Have a fee on transfer
- Have rebasing logic
- Have callback hooks on transfer
- Have more than 18 decimals
- Have out of band methods for modifying balance or allowance

## Configuring the Gas Paying Token

The gas paying token is set within the L1 `SystemConfig` smart contract. This allows for easy access to the information
required to know if an OP Stack chain is configured to use a custom gas paying token. The gas paying token is set
during initialization and cannot be modified until another initialization. It is assumed that the chain operator
will not modify the gas paying token address unless they specifically decide to do so and appropriately handle collateralization.

If the gas paying token address is set to the `ETHER_TOKEN_ADDRESS` then the system is configured to use `ether` as the
native asset on L2.

When the gas paying token is configured in the `SystemConfig`, it kicks off a special deposit transaction that is from the
`DEPOSITOR_ACCOUNT` and calls the `L1Block` contract so that it can store the gas paying token address and its number of decimals
in the L2 system.

![](../static/assets/custom-token-setup.png)

## OptimismPortal

The `OptimismPortal` is updated with a new interface specifically for depositing custom tokens.

### `depositERC20Transaction`

The `depositERC20Transaction` is useful for sending custom gas tokens to L2. It is broken out into its own interface
to maintain backwards compatibility, simplify the implementation and prevent footguns when trying to deposit an
ERC20 native asset into a chain that uses `ether` as its native asset.

```solidity
function depositERC20Transaction(
    address _to,
    uint256 _mint,
    uint256 _value,
    uint64 _gasLimit,
    bool _isCreation,
    bytes memory _data
) public;
```

### `depositTransaction`

The `depositTransaction` function is updated to revert if there is non-zero `msg.value` when the system uses a custom
native token. This allows existing applications that do cross domain calls to continue functioning as is.

## StandardBridge

The `StandardBridge` contracts on both L1 and L2 MUST be aware of the custom gas token. The `ether` specific ABI on the
`StandardBridge` is disabled when custom gas token is enabled.

This includes the following methods:

- `bridgeETH(uint32,bytes)`
- `bridgeETHTo(address,uint32,bytes)`
- `receive()`

The following diagram shows the control flow for when a user attempts to send `ether` through the `StandardBridge`, either
on L1 or L2.

![](../static/assets/custom-token-bridge-revert.png)

## CrossDomainMessenger

The `CrossDomainMessenger` contracts on both L1 and L2 SHOULD not be used to send the native asset between domains.
It is possible to support this in the future, but for now the feature is disabled due to the high probability of footguns.
Using the `CrossDomainMessenger` with a custom gas token would involve modifying the `sendMessage` implementation.

## User Flow

Users should deposit native asset by calling `depositERC20Transaction` on the `OptimismPortal` contract.
Users must first `approve` the address of the `OptimismPortal` so that the `OptimismPortal` can use
`transferFrom` to take ownership of the ERC20 asset. If the `transferFrom` fails, a [permit2](https://github.com/Uniswap/permit2)
`transferFrom` is attempted as a fallback.

Users should withdraw value by calling the `L2ToL1MessagePasser` directly.

## Security Considerations

### OptimismPortal Token Allowance

The `OptimismPortal` makes calls on behalf of users. It is therefore unsafe to be able to call the address of the custom gas
token itself from the `OptimismPortal` because it would be a simple way to `approve` an attacker's balance and steal the
entire ERC20 token balance of the `OptimismPortal`.

### Decimal Scaling

A library for scaling `uint256` representations of floating point numbers is used on both ERC20 token deposits and withdrawals.
This ensures that from the point of view of the L2 system, it always appears as if the number of tokens that were deposited
are the same as the native balance. Tokens with less than or 18 decimals are supported, meaning that there is no potential for
loss of precision due to sending in too small of a unit.

### Interoperability Support

Interop is supported between chains that use a custom gas token. The token address and the number of decimals are legible on
chain. In the future we may add the ability to poke a chain such that it emits an event that includes the custom gas token address
and its number of decimals to easily be able to introspect on the native asset of another chain.

### Wrapped Ether

The `WETH9` predeploy at `0x4200000000000000000000000000000000000006` represents wrapped native asset and not wrapped `ether`.
Portable and fungible `ether` across different domains is left for a future project.
