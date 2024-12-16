# DeputyPauseModule

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Status](#status)
- [Overview](#overview)
- [Context](#context)
- [Upgradeability](#upgradeability)
- [Assumptions](#assumptions)
  - [aSCP-001: Superchain-wide pause authorization process is well-defined](#ascp-001-superchain-wide-pause-authorization-process-is-well-defined)
  - [aSCP-002: Superchain-wide pause authorization process is robust](#ascp-002-superchain-wide-pause-authorization-process-is-robust)
  - [aSCP-003: Superchain-wide pause authorization process is fast](#ascp-003-superchain-wide-pause-authorization-process-is-fast)
  - [aDPM-001: Contract is configured correctly](#adpm-001-contract-is-configured-correctly)
    - [Mitigations](#mitigations)
  - [aDPM-002: Deputy key is not compromised](#adpm-002-deputy-key-is-not-compromised)
    - [Mitigations](#mitigations-1)
  - [aDPM-003: Deputy key is not deleted](#adpm-003-deputy-key-is-not-deleted)
    - [Mitigations](#mitigations-2)
  - [aDPM-004: Ethereum will not censor transactions for extended periods of time](#adpm-004-ethereum-will-not-censor-transactions-for-extended-periods-of-time)
    - [Mitigations](#mitigations-3)
  - [aDPM-005: DeputyGuardianModule pause function correctly triggers pause](#adpm-005-deputyguardianmodule-pause-function-correctly-triggers-pause)
    - [Mitigations](#mitigations-4)
  - [aDPM-006: OpenZeppelin ECDSA and EIP712 contracts are free of critical bugs](#adpm-006-openzeppelin-ecdsa-and-eip712-contracts-are-free-of-critical-bugs)
    - [Mitigations](#mitigations-5)
  - [aDPM-007: Safe contracts are free of critical bugs](#adpm-007-safe-contracts-are-free-of-critical-bugs)
    - [Mitigations](#mitigations-6)
  - [aDPM-008: Deputy key is capable of creating signatures](#adpm-008-deputy-key-is-capable-of-creating-signatures)
    - [Mitigations](#mitigations-7)
- [System Invariants](#system-invariants)
  - [iSCP-001: Superchain-wide pause can be activated within a short bounded time of authorization](#iscp-001-superchain-wide-pause-can-be-activated-within-a-short-bounded-time-of-authorization)
    - [Impact](#impact)
    - [Dependencies](#dependencies)
  - [iSCP-002: Superchain-wide pause is not activated outside of the standard process](#iscp-002-superchain-wide-pause-is-not-activated-outside-of-the-standard-process)
    - [Impact](#impact-1)
    - [Dependencies](#dependencies-1)
- [Component Invariants](#component-invariants)
  - [iDPM-001: Only the Deputy may act through the module](#idpm-001-only-the-deputy-may-act-through-the-module)
    - [Impact](#impact-2)
    - [Mitigations](#mitigations-8)
    - [Dependencies](#dependencies-2)
  - [iDPM-002: Deputy must only be able to trigger the Superchain-wide pause](#idpm-002-deputy-must-only-be-able-to-trigger-the-superchain-wide-pause)
    - [Impact](#impact-3)
    - [Mitigations](#mitigations-9)
    - [Dependencies](#dependencies-3)
  - [iDPM-003: Deputy must always be able to act through the module](#idpm-003-deputy-must-always-be-able-to-act-through-the-module)
    - [Impact](#impact-4)
    - [Mitigations](#mitigations-10)
    - [Dependencies](#dependencies-4)
  - [iDPM-004: Deputy authorizations must not be replayable](#idpm-004-deputy-authorizations-must-not-be-replayable)
    - [Impact](#impact-5)
    - [Mitigations](#mitigations-11)
    - [Dependencies](#dependencies-5)
  - [iDPM-005: Foundation Safe must be able to change the Deputy account easily](#idpm-005-foundation-safe-must-be-able-to-change-the-deputy-account-easily)
  - [Impact](#impact-6)
  - [Mitigations](#mitigations-12)
- [iDPM-006: Safe must be able to change the DeputyGuardianModule address easily](#idpm-006-safe-must-be-able-to-change-the-deputyguardianmodule-address-easily)
  - [Impact](#impact-7)
  - [Mitigations](#mitigations-13)
  - [Assumptions](#assumptions-1)
- [Implementation Spec](#implementation-spec)
  - [constructor](#constructor)
  - [pause](#pause)
  - [setDeputy](#setdeputy)
  - [setDeputyGuardianModule](#setdeputyguardianmodule)
  - [foundationSafe](#foundationsafe)
  - [deputyGuardianModule](#deputyguardianmodule)
  - [superchainConfig](#superchainconfig)
  - [deputy](#deputy)
  - [pauseMessageTypehash](#pausemessagetypehash)
  - [deputyAuthMessageTypehash](#deputyauthmessagetypehash)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Status

Proposed

## Overview

The `DeputyPauseModule` is a [Safe Module][safe-modules] designed to be installed into the Optimism
Foundation Operations Safe that allows a dedicated Deputy address to create an ECDSA signature that
authorizes the execution of the Superchain-wide pause mechanism. The `DeputyPauseModule` assumes
that the Optimism Foundation Operations Safe is the Deputy Guardian that is permitted to act within
the `DeputyGuardianModule` installed into the Optimism Security Council Safe.

## Context

Additional context and motivation for this component can be found in the corresponding
[design document][design-doc].

## Upgradeability

The `DeputyPauseModule` is not expected to change frequently and is therefore not a proxied
contract. The `DeputyPauseModule` allows basic parameters to be modified by the Operations Safe
directly without needing to deploy a new instance of the contract. Additionally, the address of the
module itself is only utilized within the Optimism Foundation Operations Safe and can be safely
replaced.

## Assumptions

> **NOTE**: Assumptions are utilized by specific invariants and do not apply globally. Invariants
> typically only rely on a subset of the following assumptions. Different invariants may rely on
> different assumptions. Refer to individual invariants for their dependencies.

### aSCP-001: Superchain-wide pause authorization process is well-defined

We assume that the process by which a Superchain-wide pause can be authorized is well-defined such
that the cases in which the pause should and should not be triggered are apparent.

### aSCP-002: Superchain-wide pause authorization process is robust

We assume that the process by which a Superchain-wide pause can be authorized correctly accounts
for all cases in which such a pause would be necessary.

### aSCP-003: Superchain-wide pause authorization process is fast

We assume that the process by which a Superchain-wide pause can be authorized is fast acting such
that it can make the decision to trigger the pause within a short period of time of any situation
that would require such a pause.

### aDPM-001: Contract is configured correctly

We assume that all inputs to the contract are configured correctly including:

- Address of the Operations Safe.
- Address of the `DeputyGuardianModule`.
- Address of the `SuperchainConfig` contract.
- Address of the Deputy account.

#### Mitigations

- Verify a signature from the Deputy in the contract constructor.
- Verify the configured values in the deployment script.
- Generate and test a signature on testnet.
- Monitoring if the `DeputyGuardianModule` is not installed into the Security Council Safe.

### aDPM-002: Deputy key is not compromised

We assume that the Deputy key is maintained securely and is not compromised by a malicious actor.

#### Mitigations

- Enforce strong access control around the Deputy key.
- Monitoring for any usage of the Deputy key.
- Regular rotation of the Deputy key.

### aDPM-003: Deputy key is not deleted

We assume that the Deputy key has not been deleted by a malicious actor.

#### Mitigations

- Maintain duplicate copies of the key in multiple secure locations.

### aDPM-004: Ethereum will not censor transactions for extended periods of time

We assume that Ethereum will not censor transactions for an extended period of time and that any
transaction submitted to the network can be processed within a reasonable time bound (e.g., 1h).

#### Mitigations

- Extremely high priority fee if necessary.

### aDPM-005: DeputyGuardianModule pause function correctly triggers pause

We assume that the `pause` function exposed by the `DeputyGuardianModule` will correctly trigger
the Superchain-wide pause through the Guardian account. We assume that this function will not carry
out any other action.

#### Mitigations

- Existing audit on the `DeputyGuardianModule`.

### aDPM-006: OpenZeppelin ECDSA and EIP712 contracts are free of critical bugs

We assume that both the `ECDSA` library and the `EIP712` contract provided by OpenZeppelin V4 at
commit [ecd2ca2c][oz-v4.7.3] (v4.7.3) are free of any critical bugs that would cause their
behaviors to diverge from their specified behaviors.

#### Mitigations

- Existing audits/safety processes for OpenZeppelin V4.

### aDPM-007: Safe contracts are free of critical bugs

We assume that the Safe contract implementations used by the Security Council Safe and Optimism
Foundation Operations Safe are free of any critical bugs that would cause their behaviors to
diverge from their specified behaviors.

#### Mitigations

- Existing audits/safety processes for Safe.

### aDPM-008: Deputy key is capable of creating signatures

We assume that the Deputy key configured is actually capable of creating signatures.

#### Mitigations

- Signature verification when changing Deputy key.

## System Invariants

### iSCP-001: Superchain-wide pause can be activated within a short bounded time of authorization

It is an important system-level invariant that the Superchain-wide pause functionality can be
activated within a short, bounded amount of time of its authorization by the standard process that
approves the usage of this pause.

#### Impact

**Severity: High (estimated)**

If this invariant is broken, various recovery options that rely on fast activation of the
Superchain-wide pause may fail. We estimate the severity of this invariant being broken to be high
but final severity will depend on the formalization of other invariants that rely on this one.

#### Dependencies

- [aSCP-002](#ascp-002-superchain-wide-pause-authorization-process-is-robust)
- [aSCP-003](#ascp-003-superchain-wide-pause-authorization-process-is-fast)
- [iDPM-003](#idpm-003-deputy-must-always-be-able-to-act-through-the-module)
- [aDPM-001](#adpm-001-contract-is-configured-correctly)

### iSCP-002: Superchain-wide pause is not activated outside of the standard process

We must maintain that the Superchain-wide pause is not activated outside of the standard process
that approves the usage of the pause.

#### Impact

**Severity: High**

If this invariant is broken, all components that rely on the Superchain-wide pause would be placed
into a paused state unexpectedly. This would cause a temporary liveness failure for withdrawals
through the Standard Bridge system and would negatively impact users until liveness could be
restored by either the Guardian or the Deputy Guardian.

#### Dependencies

- [aSCP-001](#ascp-001-superchain-wide-pause-authorization-process-is-well-defined)
- [iDPM-001](#idpm-001-only-the-deputy-may-act-through-the-module)
- [iDPM-004](#idpm-004-deputy-authorizations-must-not-be-replayable)
- [aDPM-002](#adpm-002-deputy-key-is-not-compromised)

## Component Invariants

### iDPM-001: Only the Deputy may act through the module

The Deputy account must be the only address that can act through the module. Other accounts can
execute an action on behalf of the Deputy if the account has a valid authorization signature from
the Deputy for that action. No account may act through the module other than with the explicit
authorization of the Deputy.

#### Impact

**Severity: High**

If this invariant is broken, accounts other than the Deputy would be able to carry out the actions
allowed by this module. Assuming that
[iDPM-002](#idpm-002-deputy-must-only-be-able-to-trigger-the-superchain-wide-pause) holds, this
means that an account other than the Deputy would be able to trigger the Superchain-wide pause,
presumably without the authorization of the social consensus process that typically triggers this
pause.

#### Mitigations

- Signature verification on the pause action.
- Monitoring for pause triggering actions.

#### Dependencies

- [aDPM-006](#adpm-006-openzeppelin-ecdsa-and-eip712-contracts-are-free-of-critical-bugs)

### iDPM-002: Deputy must only be able to trigger the Superchain-wide pause

The Deputy must only be able to trigger the Superchain-wide pause action by causing the module to
call the `pause` function on the `DeputyGuardianModule`. The Deputy must not be able to trigger any
other action.

#### Impact

**Severity: Critical**

If this invariant is broken, the Deputy would be able to cause the Optimism Foundation Operations
Safe to execute some unknown set of possible actions. We would treat this as a critical risk.

#### Mitigations

- Strict auditing and verification of the `DeputyPauseModule`.

#### Dependencies

- [aDPM-005](#adpm-005-deputyguardianmodule-pause-function-correctly-triggers-pause)
- [aDPM-007](#adpm-007-safe-contracts-are-free-of-critical-bugs)

### iDPM-003: Deputy must always be able to act through the module

The Deputy account must always be able to act through the module, even if the Deputy account
private key is compromised. Other than the total deletion of the Deputy key, there should not be
any condition in the contract itself that would prevent the key from being able to quickly and
efficiently execute the pause action.

#### Impact

**Severity: High**

If this invariant is broken, we would not be able to provide the system-level invariant that the
Deputy account can always be used to trigger the Superchain-wide pause within a bounded amount of
time of the decision being made to carry out this action.

#### Mitigations

- Signature-based verification to bypass draining attacks.
- Strict auditing and verification of the `DeputyPauseModule`.

#### Dependencies

- [aDPM-001](#adpm-001-contract-is-configured-correctly)
- [aDPM-003](#adpm-003-deputy-key-is-not-deleted)
- [aDPM-004](#adpm-004-ethereum-will-not-censor-transactions-for-extended-periods-of-time)
- [aDPM-006](#adpm-006-openzeppelin-ecdsa-and-eip712-contracts-are-free-of-critical-bugs)
- [aDPM-008](#adpm-008-deputy-key-is-capable-of-creating-signatures)

### iDPM-004: Deputy authorizations must not be replayable

A Deputy authorization must apply to a specific `DeputyPauseModule` on a specific blockchain as
identified by its chain ID. An authorization created for one `DeputyPauseModule` must not be
reusable on any other `DeputyPauseModule`. An authorization must be usable once and the same
authorization must not be usable again in any `DeputyPauseModule`.

#### Impact

**Severity: High**

If this invariant is broken, a Deputy authorization created by the same Deputy address for
another `DeputyPauseModule` or a previous authorization for the same module could be reused and
would result in the Superchain-wide pause being triggered outside of the standard process by which
such a pause is approved.

#### Mitigations

- EIP-712 signature verification including a specific contract address and chain ID.
- Enforce unique nonces on signatures to prevent signature reuse within the same contract.
- Utilize unique Deputy accounts for each module instance or network.

#### Dependencies

- [aDPM-006](#adpm-006-openzeppelin-ecdsa-and-eip712-contracts-are-free-of-critical-bugs)

### iDPM-005: Foundation Safe must be able to change the Deputy account easily

The Foundation Safe must be able to change the address of the Deputy account without significant
operational overhead.

### Impact

**Severity: Medium**

If this invariant is broken, it would not be possible for the Safe account to easily rotate the
Deputy account if the account is compromised. This creates operational overhead but is not a
security risk as we assume that the Safe code does not have bugs and therefore the Safe can always
remove the module if necessary.

### Mitigations

- Authorized function to change the Deputy address.

## iDPM-006: Safe must be able to change the DeputyGuardianModule address easily

The Foundation Safe must be able to change the address of the `DeputyGuardianModule` without
significant operational overhead.

### Impact

**Severity: Medium**

If this invariant is broken, it would not be possible for the Safe account to easily change the
`DeputyGuardianModule` address if this module is updated. This creates operational overhead but is
not a security risk as we assume that the Safe code does not have bugs and therefore the Safe can
always remove the module if necessary.

### Mitigations

- Authorized function to change the `DeputyGuardianModule` address.

### Assumptions

- [aDPM-007](#adpm-007-safe-contracts-are-free-of-critical-bugs)

## Implementation Spec

### constructor

- Sets the EIP-712 domain name to "DeputyPauseModule".
- Sets the EIP-712 domain version to "1".
- Takes the address of the Operations Safe as an authorized input.
- Takes the address of the `DeputyGuardianModule` as an authorized input.
- Takes the address of the `SuperchainConfig` as an authorized input.
- Takes the address of the Deputy as an authorized input.
- Takes a signature from the Deputy over a known EIP-712 message.
- Must verify that the signature was produced by the provided Deputy address.

### pause

- Takes a nonce as an untrusted input.
- Takes a signature as an untrusted input.
- Callable by any address.
- Must verify that the nonce has not been used.
- Must verify that the signature is over an EIP-712 message that commits to the nonce
  and was produced by the private key corresponding to the Deputy address.
- Must mark the nonce as used.
- Must trigger the pause function on the `DeputyGuardianModule`.
- Must revert if the call to the pause function failed.
- Must revert if the call succeeded but the `SuperchainConfig` contract was not paused.

### setDeputy

- Takes a deputy address as an authorized input.
- Takes a deputy auth signature as an authorized input.
- Can only be called by the Foundation Safe as configured in the constructor.
- Must verify that the signature was produced by the provided Deputy address.

### setDeputyGuardianModule

- Takes an address as an untrusted input.
- Can only be called by the Foundation Safe as configured in the constructor.

### foundationSafe

- Returns the address of the Operations Safe as set in the constructor.

### deputyGuardianModule

- Returns the address of the `DeputyGuardianModule` as set in the constructor.

### superchainConfig

- Returns the address of the `SuperchainConfig` contract as set in the constructor.

### deputy

- Returns the address of the Deputy account as set in the constructor.

### pauseMessageTypehash

- Returns the typehash that corresponds to `struct PauseMessage { bytes32 nonce }`.

### deputyAuthMessageTypehash

- Returns the typehash that corresponds to `struct DeputyAuthMessage { address deputy }`.

<!-- References -->
[design-doc]: https://github.com/ethereum-optimism/design-docs/blob/85c0f822/protocol/deputy-pause-module.md
[safe-modules]: https://docs.safe.global/advanced/smart-account-modules
[oz-v4.7.3]: https://github.com/OpenZeppelin/openzeppelin-contracts/tree/ecd2ca2c
