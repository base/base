# DelayedWETH

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Withdrawal Delay](#withdrawal-delay)
  - [Sub-Account](#sub-account)
  - [Withdrawal Request](#withdrawal-request)
- [Assumptions](#assumptions)
  - [a01-001: Owner operates within governance constraints](#a01-001-owner-operates-within-governance-constraints)
    - [Mitigations](#mitigations)
  - [a01-002: SuperchainConfig provides accurate pause state](#a01-002-superchainconfig-provides-accurate-pause-state)
    - [Mitigations](#mitigations-1)
- [Invariants](#invariants)
  - [i01-001: Withdrawal delay enforcement](#i01-001-withdrawal-delay-enforcement)
    - [Impact](#impact)
  - [i01-002: Owner emergency intervention capability](#i01-002-owner-emergency-intervention-capability)
    - [Impact](#impact-1)
- [Function Specification](#function-specification)
  - [constructor](#constructor)
  - [initialize](#initialize)
  - [delay](#delay)
  - [config](#config)
  - [unlock](#unlock)
  - [withdraw](#withdraw)
  - [withdraw (single parameter)](#withdraw-single-parameter)
  - [recover](#recover)
  - [hold (single parameter)](#hold-single-parameter)
  - [hold (two parameters)](#hold-two-parameters)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

DelayedWETH extends the standard WETH contract to provide time-delayed withdrawals with emergency recovery
mechanisms. It serves as the bond holding contract for the Fault Dispute Game system, ensuring that dispute
participants cannot immediately withdraw their bonds and providing a safety window for governance intervention
in case of incorrect bond distributions.

## Definitions

### Withdrawal Delay

The minimum time period (in seconds) that must elapse between unlocking a withdrawal and executing it.
This delay provides a window for the contract owner to intervene if the Fault Dispute Game incorrectly
distributes bonds.

### Sub-Account

An address parameter used to segregate withdrawal requests within a single caller's account. This allows
the Fault Dispute Game to track individual user withdrawals separately, enabling precise accounting and
selective intervention by the owner if bugs occur in bond distribution logic.

### Withdrawal Request

A record tracking the amount of WETH unlocked for withdrawal and the timestamp when it was unlocked.
Each withdrawal request is uniquely identified by the combination of the caller address and the
[Sub-Account](#sub-account) address.

## Assumptions

### a01-001: Owner operates within governance constraints

The contract owner (typically the System Owner multisig) acts honestly and only uses emergency functions
(`recover`, `hold`) to correct errors in dispute game bond distributions, not to arbitrarily confiscate user
funds.

#### Mitigations

- Owner is expected to be a multisig with multiple signers across different timezones
- [Withdrawal Delay](#withdrawal-delay) of 7 days provides time for community oversight before withdrawals
complete
- Owner actions are transparent on-chain and subject to community monitoring

### a01-002: SuperchainConfig provides accurate pause state

The SystemConfig contract correctly references a SuperchainConfig contract that accurately reflects the
intended pause state of the system.

#### Mitigations

- SuperchainConfig is a well-audited core protocol contract
- Pause functionality is controlled by the Guardian role with established governance processes

## Invariants

### i01-001: Withdrawal delay enforcement

Withdrawals cannot be executed immediately after unlocking. A time delay must elapse to ensure the owner has
sufficient time to intervene if the Fault Dispute Game has incorrectly distributed bonds.

#### Impact

**Severity: High**

If this invariant is violated, malicious actors could immediately withdraw incorrectly awarded bonds before
governance can intervene, leading to loss of funds that should have been redistributed to honest participants.
The severity is High rather than Critical because violation requires the [Owner operates within governance
constraints](#a01-001-owner-operates-within-governance-constraints) assumption to also fail for actual fund
loss to occur.

### i01-002: Owner emergency intervention capability

The contract owner can always recover ETH from the contract balance via `recover()` and can always transfer
WETH from any account to themselves via `hold()`, regardless of withdrawal request states. This provides an
emergency backstop if the Fault Dispute Game distributes bonds incorrectly.

#### Impact

**Severity: High**

If this invariant is violated, the owner cannot correct erroneous bond distributions from buggy dispute games,
potentially resulting in permanent loss of funds that should have been awarded to honest participants. The
severity is High because it depends on the [Owner operates within governance constraints](#a01-001-owner-
operates-within-governance-constraints) assumption - the owner must act correctly to utilize this capability
for its intended purpose.

## Function Specification

### constructor

Initializes the immutable [Withdrawal Delay](#withdrawal-delay) and disables initializers for the
implementation contract.

**Parameters:**

- `_delay`: The withdrawal delay in seconds

**Behavior:**

- MUST set the immutable `DELAY_SECONDS` to `_delay`
- MUST call `_disableInitializers()` to prevent initialization of the implementation contract

### initialize

Initializes the proxy contract with the SystemConfig address.

**Parameters:**

- `_systemConfig`: Address of the SystemConfig contract

**Behavior:**

- MUST revert if caller is not the ProxyAdmin or ProxyAdmin owner
- MUST set `systemConfig` to `_systemConfig`
- MUST only be callable once per initialization version via the `reinitializer` modifier

### delay

Returns the [Withdrawal Delay](#withdrawal-delay) in seconds.

**Behavior:**

- MUST return the value of `DELAY_SECONDS`

### config

Returns the SuperchainConfig contract address.

**Behavior:**

- MUST return the result of calling `systemConfig.superchainConfig()`

### unlock

Creates or updates a [Withdrawal Request](#withdrawal-request) for a [Sub-Account](#sub-account), setting
the unlock timestamp to the current block timestamp and increasing the unlocked amount.

**Parameters:**

- `_guy`: The [Sub-Account](#sub-account) address for which to unlock withdrawals
- `_wad`: The amount of WETH to unlock

**Behavior:**

- MUST set `withdrawals[msg.sender][_guy].timestamp` to `block.timestamp`
- MUST increase `withdrawals[msg.sender][_guy].amount` by `_wad`
- MUST be callable by any address

### withdraw

Withdraws ETH to `msg.sender` after the [Withdrawal Delay](#withdrawal-delay) has elapsed.

**Parameters:**

- `_guy`: The [Sub-Account](#sub-account) address to withdraw from
- `_wad`: The amount of WETH to withdraw

**Behavior:**

- MUST revert if `systemConfig.paused()` returns true
- MUST revert if `withdrawals[msg.sender][_guy].amount` is less than `_wad`
- MUST revert if `withdrawals[msg.sender][_guy].timestamp` is 0
- MUST revert if `withdrawals[msg.sender][_guy].timestamp + DELAY_SECONDS` is greater than `block.timestamp`
- MUST decrease `withdrawals[msg.sender][_guy].amount` by `_wad`
- MUST call the parent `WETH98.withdraw(_wad)` function to transfer ETH to `msg.sender`

### withdraw (single parameter)

Convenience function that withdraws from the caller's own [Sub-Account](#sub-account).

**Parameters:**

- `_wad`: The amount of WETH to withdraw

**Behavior:**

- MUST call `withdraw(msg.sender, _wad)`

### recover

Allows the owner to recover ETH from the contract balance in emergency situations.

**Parameters:**

- `_wad`: The requested amount of ETH to recover

**Behavior:**

- MUST revert if `msg.sender` is not the ProxyAdmin owner
- MUST calculate the actual recovery amount as the minimum of `_wad` and `address(this).balance`
- MUST transfer the calculated amount to `msg.sender` via a low-level call
- MUST revert if the transfer fails

### hold (single parameter)

Allows the owner to transfer all WETH from a specific account to themselves.

**Parameters:**

- `_guy`: The address to transfer WETH from

**Behavior:**

- MUST call `hold(_guy, balanceOf(_guy))`

### hold (two parameters)

Allows the owner to transfer a specific amount of WETH from any account to themselves by setting an allowance
and immediately executing a transfer.

**Parameters:**

- `_guy`: The address to transfer WETH from
- `_wad`: The amount of WETH to transfer

**Behavior:**

- MUST revert if `msg.sender` is not the ProxyAdmin owner
- MUST set `_allowance[_guy][msg.sender]` to `_wad`
- MUST emit an `Approval` event with parameters `(_guy, msg.sender, _wad)`
- MUST call `transferFrom(_guy, msg.sender, _wad)` to execute the transfer
