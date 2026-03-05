# ETH Bridging

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Constants](#constants)
- [Design](#design)
  - [Diagram](#diagram)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

ETH is bridged between chains within the Superchain interop set by using the `SuperchainETHBridge`
predeploy for message passing and the `ETHLiquidity` contract for supplying native ETH liquidity.

## Constants

| Name                          | Value                                        |
| ----------------------------- | -------------------------------------------- |
| `SuperchainETHBridge` Address | `0x4200000000000000000000000000000000000024` |
| `ETHLiquidity` Address        | `0x4200000000000000000000000000000000000025` |

## Design

See the [SuperchainETHBridge](./superchain-eth-bridge.md) spec for the design of the
`SuperchainETHBridge` predeploy and the [ETHLiquidity](./eth-liquidity.md) spec for
the design of the `ETHLiquidity` contract.

### Diagram

The following diagram depicts a cross-chain ETH transfer.

```mermaid
---
config:
  theme: dark
  fontSize: 48
---
sequenceDiagram
  participant from as from (Chain A)
  participant L2SBA as SuperchainETHBridge (Chain A)
  participant ETHLiquidity_A as ETHLiquidity (Chain A)
  participant Messenger_A as L2ToL2CrossDomainMessenger (Chain A)
  participant Inbox as CrossL2Inbox
  participant Messenger_B as L2ToL2CrossDomainMessenger (Chain B)
  participant L2SBB as SuperchainETHBridge (Chain B)
  participant ETHLiquidity_B as ETHLiquidity (Chain B)
  participant recipient as recipient (Chain B)

  from->>L2SBA: sendETH{value: amount}(to, chainId)
  L2SBA->>ETHLiquidity_A: burn{value: amount}()
  ETHLiquidity_A-->ETHLiquidity_A: emit LiquidityBurned(from, amount)
  L2SBA->>Messenger_A: sendMessage(chainId, message)
  Messenger_A->>L2SBA: return msgHash_
  L2SBA-->L2SBA: emit SendETH(from, to, amount, destination)
  L2SBA->>from: return msgHash_
  Inbox->>Messenger_B: relayMessage()
  Messenger_B->>L2SBB: relayETH(from, to, amount)
  L2SBB->>ETHLiquidity_B: mint(amount)
  ETHLiquidity_B-->ETHLiquidity_B: emit LiquidityMinted(SuperchainETHBridge address, amount)
  L2SBB->>recipient: new SafeSend{value: amount}(to)
  L2SBB-->L2SBB: emit RelayETH(from, to, amount, source)
```
