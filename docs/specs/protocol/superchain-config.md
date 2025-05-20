# Superchain Configuration

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Invariants](#invariants)
  - [iSUPC-001: The Guardian and Pause Deputy must be able to trigger the Pause Mechanism](#isupc-001-the-guardian-and-pause-deputy-must-be-able-to-trigger-the-pause-mechanism)
    - [Impact](#impact)
  - [iSUPC-002: The Guardian must be able to reset or undo the Pause Mechanism](#isupc-002-the-guardian-must-be-able-to-reset-or-undo-the-pause-mechanism)
    - [Impact](#impact-1)
- [Function Specification](#function-specification)
  - [initialize](#initialize)
  - [upgrade](#upgrade)
  - [guardian](#guardian)
  - [pauseExpiry](#pauseexpiry)
  - [pause](#pause)
  - [unpause](#unpause)
  - [extend](#extend)
  - [pausable](#pausable)
  - [paused](#paused)
  - [expiration](#expiration)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The SuperchainConfig contract is used to manage global configuration values for multiple OP Chains
within a single Superchain network.

## Invariants

### iSUPC-001: The Guardian and Pause Deputy must be able to trigger the Pause Mechanism

We require that the `SuperchainConfig` is constructed such that both the
[Guardian](./stage-1.md#guardian) and the [Pause Deputy](./stage-1.md#pause-deputy) must be able to
trigger the [Pause Mechanism](./stage-1.md#pause-mechanism) at any time.

#### Impact

**Severity: High**

Existing recovery runbooks would not function as expected if the `SuperchainConfig` prevented one
of these actors from triggering the pause as needed.

### iSUPC-002: The Guardian must be able to reset or undo the Pause Mechanism

We require that the `SuperchainConfig` is constructed such that the
[Guardian](./stage-1.md#guardian) must be able to unpause or extend the
[Pause Mechanism](./stage-1.md#pause-mechanism) at any time.

#### Impact

**Severity: Medium**

If the Pause Mechanism cannot be reset then it cannot be used again without intervention from the
[Proxy Admin Owner](./stage-1.md#proxy-admin-owner). We consider this to be a Medium severity
issue because the Proxy Admin Owner will have several months to coordinate such a fix assuming
that [iSUPC-001][iSUPC-001] holds.

## Function Specification

### initialize

- MUST only be triggerable by the ProxyAdmin or its owner.
- MUST only be triggerable once.
- MUST set the value of the Guardian role.
- MUST emit a ConfigUpdate event with the Guardian address.

### upgrade

- MUST only be triggerable by the ProxyAdmin or its owner.
- MUST migrate the guardian from old storage to new storage.
- MUST clear old storage slots.
- MUST maintain contract version information.

### guardian

Returns the address of the current [Guardian](./stage-1.md#guardian).

### pauseExpiry

Returns the duration after which a pause expires, which is a hardcoded constant of 7,884,000 seconds (approximately 3 months).

### pause

Allows the [Guardian](./stage-1.md#guardian) to trigger the
[Pause Mechanism](./stage-1.md#pause-mechanism). `pause` takes an address
[Pause Identifier](./stage-1.md#pause-identifier) as an input. This identifier determines which
systems or chains are affected by the pause.

- MUST revert if called by an address other than the Guardian.
- MUST revert if the pause timestamp for the given identifier is non-zero (already paused).
- MUST set the pause timestamp for the given identifier to the current block timestamp.
- MUST emit a Paused event with the identifier.

### unpause

Allows the [Guardian](./stage-1.md#guardian) to explicitly unpause the system for a given
[Pause Identifier](./stage-1.md#pause-identifier) rather than waiting for the pause to expire.
Unpausing a specific identifier does NOT unpause the global pause (zero address identifier). If the
global pause is active, all systems will remain paused even if their specific identifiers are
unpaused.

- MUST revert if called by an address other than the Guardian.
- MUST set the pause timestamp for the given identifier to 0, representing "not paused".
- MUST emit an Unpaused event with the identifier.
- Will not revert if the system is not already paused for the given identifier.

### extend

Allows the [Guardian](./stage-1.md#guardian) to extend an active pause by resetting the pause
timestamp to the current block timestamp, effectively restarting the expiry timer.

- MUST revert if called by an address other than the Guardian.
- MUST revert if the pause timestamp for the given identifier is zero (not currently paused).
- MUST set the pause timestamp for the given identifier to the current block timestamp.
- MUST emit a Paused event with the identifier.

### pausable

Allows any user to check if the [Pause Mechanism](./stage-1.md#pause-mechanism) can be triggered
for a specific [Pause Identifier](./stage-1.md#pause-identifier). The pausable status of a specific
identifier is independent of the pausable status of the global pause (zero address identifier).

- MUST return true if the pause timestamp for the given identifier is 0 (not currently paused).
- MUST return false if the pause timestamp for the given identifier is non-zero (currently paused).

### paused

Allows any user to check if the system is currently paused for a specific
[Pause Identifier](./stage-1.md#pause-identifier).

- MUST return true if the pause timestamp for the given identifier is non-zero AND not expired
  (current time < pause timestamp + expiry duration).
- MUST return false otherwise.
- When called without parameters, MUST check the pause status for the global identifier (address(0)).

### expiration

Returns the timestamp at which the pause for a given
[Pause Identifier](./stage-1.md#pause-identifier) will expire. This function only returns the
expiration for the specific identifier provided.

- MUST return the pause timestamp plus the configured expiry duration if the pause timestamp is non-zero.
- MUST return 0 if the pause timestamp is 0 (system is not paused) for the given identifier.

<!-- references -->

[iSUPC-001]: #isupc-001-the-guardian-and-pause-deputy-must-be-able-to-trigger-the-pause-mechanism
