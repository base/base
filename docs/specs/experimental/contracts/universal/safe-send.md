# SafeSend

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Forced ETH Transfer](#forced-eth-transfer)
- [Assumptions](#assumptions)
  - [aSS-001: EVM selfdestruct behavior remains consistent](#ass-001-evm-selfdestruct-behavior-remains-consistent)
    - [Mitigations](#mitigations)
- [Dependencies](#dependencies)
- [Invariants](#invariants)
  - [iSS-001: ETH must be transferred without triggering recipient code](#iss-001-eth-must-be-transferred-without-triggering-recipient-code)
    - [Impact](#impact)
  - [iSS-002: Transfer must succeed for any recipient address](#iss-002-transfer-must-succeed-for-any-recipient-address)
    - [Impact](#impact-1)
- [Function Specification](#function-specification)
  - [constructor](#constructor)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

SafeSend is a utility contract that enables forced ETH transfers to any address without triggering code
execution at the recipient. It accomplishes this by using the `selfdestruct` opcode during construction,
which transfers the contract's balance to the recipient without invoking any fallback or receive functions.

## Definitions

### Forced ETH Transfer

A transfer of ETH that bypasses the recipient's code execution. Unlike standard ETH transfers via `call` or
`transfer`, a [Forced ETH Transfer] does not invoke the recipient's `receive()` or `fallback()` functions,
and cannot be reverted by the recipient.

## Assumptions

### aSS-001: EVM selfdestruct behavior remains consistent

The `selfdestruct` opcode continues to transfer all ETH from the contract to the specified recipient without
triggering code execution at the recipient address. This behavior is fundamental to the contract's security
properties.

#### Mitigations

- EIP-6780 (Cancun hardfork) modified `selfdestruct` behavior but preserved the same-transaction ETH transfer
  semantics that SafeSend relies on
- The contract only uses `selfdestruct` within the constructor, ensuring same-transaction execution
- Protocol-level monitoring of EVM specification changes

## Dependencies

N/A

## Invariants

### iSS-001: ETH must be transferred without triggering recipient code

The ETH transfer to the recipient MUST NOT invoke any code at the recipient address, including `receive()`,
`fallback()`, or any other functions. This prevents reentrancy attacks and ensures the recipient cannot
reject the transfer.

#### Impact

**Severity: Critical**

If recipient code were executed during the transfer, it would:
- Enable reentrancy attacks against the calling contract
- Allow malicious recipients to revert and block legitimate ETH transfers
- Violate the security guarantees that SafeSend provides to its callers (e.g., SuperchainETHBridge, Faucet)

### iSS-002: Transfer must succeed for any recipient address

The ETH transfer MUST succeed regardless of the recipient address type, including:
- Externally Owned Accounts (EOAs)
- Smart contracts with or without receive/fallback functions
- Smart contracts that revert on ETH receipt
- The zero address (0x0)

#### Impact

**Severity: Critical**

If transfers could fail based on recipient characteristics:
- Cross-chain ETH bridging via SuperchainETHBridge could be blocked by malicious recipients
- Faucet withdrawals could be denied by recipient manipulation
- System reliability would depend on recipient cooperation

## Function Specification

### constructor

Creates a SafeSend contract instance that immediately transfers all provided ETH to the recipient and
self-destructs.

**Parameters:**

- `_recipient`: The address (payable) to receive the ETH. Can be any valid address including contracts and
  the zero address.

**Behavior:**
- MUST be payable to accept ETH during construction
- MUST call `selfdestruct(_recipient)` to transfer all ETH to the recipient
- MUST NOT execute any code at the recipient address during the transfer
- MUST succeed regardless of recipient address type or code
- MUST leave zero balance at the SafeSend contract address after execution
- MUST complete within the same transaction as the contract creation
