# Superchain WETH

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Motivation and Constraints](#motivation-and-constraints)
  - [Handling native assets other than ETH](#handling-native-assets-other-than-eth)
  - [Minimizing protocol complexity](#minimizing-protocol-complexity)
- [Constants](#constants)
- [SuperchainWETH](#superchainweth)
  - [Invariants](#invariants)
    - [`deposit`](#deposit)
    - [`withdraw`](#withdraw)
    - [`sendERC20`](#senderc20)
    - [`sendERC20To`](#senderc20to)
    - [`relayERC20`](#relayerc20)
- [ETHLiquidity](#ethliquidity)
  - [Invariants](#invariants-1)
    - [Global Invariants](#global-invariants)
    - [`burn`](#burn)
    - [`mint`](#mint)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

Superchain WETH is a version of the standard WETH contract that allows ETH to be interoperable across the Superchain.
Superchain WETH treats ETH as an ERC-20 token for interoperability and avoids native ETH support.
Superchain WETH also introduces a liquidity contract used to provide liquidity for native ETH across chains.

## Motivation and Constraints

ETH is the native asset of Ethereum and has by extension also become the native asset of many Ethereum L2 blockchains.
In its role as a native asset, ETH can be used to pay for transaction fees and can be transferred from account to
account via calls with attached value. ETH plays a significant role in the economics of most L2s and any protocol that
enables interoperability between chains must be able to account for ETH.

### Handling native assets other than ETH

Not all chains using the OP Stack use ETH as the native asset. We would like these chains to be able to interoperate
with chains that *do* use ETH as a native asset. Certain solutions that might work when all chains use ETH as a native
asset begin to break down when alternative native assets are introduced. For example, a protocol that burns the native
asset on one chain and mints it on another will work if both chains use the same native asset but will obviously fail if
either chain uses a different native asset.

### Minimizing protocol complexity

Support for native ETH opens the door to unnecessary complexity. Any solution to this problem should aim to minimize the
amount of protocol code required to support native ETH. This generally points towards an app-layer solution if possible
but does not preclude a protocol-layer solution as long as we minimize implementation size.

## Constants

| Name                     | Value                                        |
| ------------------------ | -------------------------------------------- |
| `SuperchainWETH` Address | `0x4200000000000000000000000000000000000024` |
| `ETHLiquidity` Address   | `0x4200000000000000000000000000000000000025` |

## SuperchainWETH

### Invariants

#### `deposit`

- Reverts if triggered on a chain that does not use ETH as a native token.

#### `withdraw`

- Reverts if triggered on a chain that does not use ETH as a native token.

#### `sendERC20`

- Reverts if attempting to send more than the sender's available balance.
- Reduce's the sender's balance by the sent amount.
- Emits a transfer event from sender to null address for the sent amount.
- Emits a `SendERC20` event.
- Burns liquidity by sending the sent amount of ETH into the `ETHLiquidity` contract if native token is ETH.
  - Must not revert.
- Sends a message to `SuperchainWETH` on the recipient chain finalizing a send of WETH.
  - Must not revert.

#### `sendERC20To`

- All invariants of `sendERC20`.
- Message sent to `SuperchainWETH` on recipient chain includes `to` address as recipient.

#### `relayERC20`

- Reverts if called by any address other than the `L2ToL2CrossDomainMessenger`.
- Reverts if `crossDomainMessageSender` is not the `SuperchainWETH` contract.
- Mints liquidity from the `ETHLiquidity` contract if native token is ETH.
  - Must not revert.
- Increases the recipient's balance by the sent amount.
- Emits a transfer event from null address to recipient for the sent amount.
- Emits a `RelayERC0` event.

## ETHLiquidity

### Invariants

#### Global Invariants

- Initial balance must be set to `type(uint248).max` (wei). Purpose for using `type(uint248).max` is to guarantees that
the balance will be sufficient to credit all use within the `SuperchainWETH` contract but will never overflow on calls
to `burn` because there is not ETH in the total ETH supply to cause such an overflow. Invariant that avoids overflow is
maintained by  `SuperchainWETH` but could theoretically be broken by some future contract that is allowed to integrate
with `ETHLiquidity`. Maintainers should be careful to ensure that such future contracts do not break this invariant.

#### `burn`

- Must never be callable such that balance would increase beyond `type(uint256).max`.
  - This is an invariant and NOT a revert.
  - Maintained by considering total available ETH supply and the initial balance of `ETHLiquidity`.
- Reverts if called by any address other than `SuperchainWETH`.
- Reverts if called on a chain that does not use ETH as a native token.
- Accepts ETH value.
- Emits an event including address that triggered the burn and the burned ETH value.

#### `mint`

- Must never be callable such that balance would decrease below `0`.
  - This is an invariant and NOT a revert.
  - Maintained by considering total available ETH supply and the initial balance of `ETHLiquidity`.
- Reverts if called by any address other than `SuperchainWETH`.
- Reverts if called on a chain that does not use ETH as a native token.
- Transfers requested ETH value to the sending address.
- Emits an event including address that triggered the mint and the minted ETH value.
