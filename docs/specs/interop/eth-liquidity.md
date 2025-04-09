# ETHLiquidity

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Constants](#constants)
- [Definitions](#definitions)
  - [ETH Liquidity](#eth-liquidity)
  - [Liquidity Minting](#liquidity-minting)
  - [Liquidity Burning](#liquidity-burning)
- [Assumptions](#assumptions)
  - [aEL-001: SafeSend correctly transfers ETH to recipients](#ael-001-safesend-correctly-transfers-eth-to-recipients)
    - [Mitigations](#mitigations)
- [Invariants](#invariants)
  - [iEL-001: ETH balance is always sufficient for minting / burning](#iel-001-eth-balance-is-always-sufficient-for-minting--burning)
    - [Impact](#impact)
  - [iEL-002: Only authorized contracts can call mint and burn](#iel-002-only-authorized-contracts-can-call-mint-and-burn)
    - [Impact](#impact-1)
  - [iEL-003: Contract balance never overflows](#iel-003-contract-balance-never-overflows)
    - [Impact](#impact-2)
- [Function Specification](#function-specification)
  - [mint](#mint)
  - [burn](#burn)
- [Events](#events)
  - [LiquidityMinted](#liquidityminted)
  - [LiquidityBurned](#liquidityburned)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The `ETHLiquidity` contract is a predeploy that manages native ETH liquidity for cross-chain
transfers within the Superchain interop set. It works in conjunction with the
`SuperchainETHBridge` to facilitate the movement of ETH between chains without requiring
modifications to the EVM to generate new ETH.

The contract is initialized with a very large balance (`type(uint248).max` wei) to ensure it can
handle all legitimate minting operations. This design allows the `SuperchainETHBridge` to have a
guaranteed source of ETH liquidity on each chain, which is essential for the cross-chain ETH
transfer mechanism.

## Constants

| Name                    | Value                                        |
| ----------------------- | -------------------------------------------- |
| `ETHLiquidity` Address  | `0x4200000000000000000000000000000000000025` |
| Initial Balance         | `type(uint248).max` wei                      |

## Definitions

### ETH Liquidity

ETH Liquidity refers to the availability of native ETH on a particular chain. The `ETHLiquidity`
contract manages this liquidity by providing a mechanism to burn and mint ETH as needed for
cross-chain transfers.

### Liquidity Minting

Liquidity Minting is the process of withdrawing ETH from the `ETHLiquidity` contract. This
operation is performed when ETH needs to be sent to a recipient on a destination chain as part of
a cross-chain transfer.

### Liquidity Burning

Liquidity Burning is the process of depositing ETH into the `ETHLiquidity` contract. This operation
is performed when ETH is being sent from a source chain to a destination chain as part of a
cross-chain transfer.

## Assumptions

### aEL-001: SafeSend correctly transfers ETH to recipients

We assume that the `SafeSend` mechanism correctly transfers ETH to recipients without reverting
for valid addresses.

#### Mitigations

- Extensive testing of the `SafeSend` mechanism
- Audits of the ETH transfer logic

## Invariants

### iEL-001: ETH balance is always sufficient for minting / burning

The `ETHLiquidity` contract must always have enough ETH to fulfill all legitimate minting / burning
requests.

#### Impact

**Severity: Critical**

If this invariant is broken, the cross-chain ETH transfer mechanism would fail, potentially leading
to users being unable to receive their ETH on destination chains.

### iEL-002: Only authorized contracts can call mint and burn

The `mint` and `burn` functions must only be callable by authorized contracts (specifically the
`SuperchainETHBridge`).

#### Impact

**Severity: Critical**

If this invariant is broken, unauthorized parties could mint ETH without burning an equivalent
amount on a source chain, leading to inflation.

### iEL-003: Contract balance never overflows

The `ETHLiquidity` contract's balance must never overflow due to excessive burning operations.

#### Impact

**Severity: High**

If this invariant is broken, the contract could reach an invalid state, potentially disrupting the
cross-chain ETH transfer mechanism. The invariant that avoids overflow is maintained by
`SuperchainETHBridge`, but could theoretically be broken by some future contract that is allowed to
integrate with `ETHLiquidity`. Maintainers should be careful to ensure that such future contracts
do not break this invariant.

## Function Specification

### mint

Withdraws ETH from the `ETHLiquidity` contract equal to the `_amount` and sends it to the
`msg.sender`.

```solidity
function mint(uint256 _amount) external
```

- MUST revert if called by any address other than the `SuperchainETHBridge`.
- MUST transfer the `_amount` of ETH to the `msg.sender` using `SafeSend`.
- MUST emit a `LiquidityMinted` event with the `msg.sender` and `_amount`.

### burn

Locks ETH into the `ETHLiquidity` contract.

```solidity
function burn() external payable;
```

- MUST accept ETH via `msg.value`.
- MUST revert if called by any address other than the `SuperchainETHBridge`.
- MUST emit a `LiquidityBurned` event with the `msg.sender` and `msg.value`.

## Events

### LiquidityMinted

MUST be triggered when `mint` is called.

```solidity
event LiquidityMinted(address indexed caller, uint256 value);
```

### LiquidityBurned

MUST be triggered when `burn` is called.

```solidity
event LiquidityBurned(address indexed caller, uint256 value);
```
