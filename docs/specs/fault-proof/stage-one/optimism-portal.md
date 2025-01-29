# OptimismPortal

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Proof Maturity Delay](#proof-maturity-delay)
  - [Proven Withdrawal](#proven-withdrawal)
  - [Finalized Withdrawal](#finalized-withdrawal)
  - [Valid Withdrawal](#valid-withdrawal)
  - [Invalid Withdrawal](#invalid-withdrawal)
  - [Respected Game Type](#respected-game-type)
  - [Retirement Timestamp](#retirement-timestamp)
  - [L2 Withdrawal Sender](#l2-withdrawal-sender)
- [Assumptions](#assumptions)
  - [aOP-001: Dispute Game contracts properly report important properties](#aop-001-dispute-game-contracts-properly-report-important-properties)
    - [Mitigations](#mitigations)
  - [aOP-002: DisputeGameFactory properly reports its created games](#aop-002-disputegamefactory-properly-reports-its-created-games)
    - [Mitigations](#mitigations-1)
  - [aOP-003: Incorrectly resolving games will be invalidated within the airgap delay period](#aop-003-incorrectly-resolving-games-will-be-invalidated-within-the-airgap-delay-period)
    - [Mitigations](#mitigations-2)
- [Invariants](#invariants)
  - [iOP-001: Invalid Withdrawals can never be finalized](#iop-001-invalid-withdrawals-can-never-be-finalized)
    - [Impact](#impact)
  - [iOP-002: Valid Withdrawals can always be finalized in bounded time](#iop-002-valid-withdrawals-can-always-be-finalized-in-bounded-time)
    - [Impact](#impact-1)
- [Function Specification](#function-specification)
  - [setRespectedGameType](#setrespectedgametype)
  - [blacklistDisputeGame](#blacklistdisputegame)
  - [proveWithdrawalTransaction](#provewithdrawaltransaction)
  - [checkWithdrawal](#checkwithdrawal)
  - [finalizeWithdrawalTransaction](#finalizewithdrawaltransaction)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The `OptimismPortal` contract is the primary interface for deposits and withdrawals between the L1
and L2 chains within an OP Stack system. The `OptimismPortal` contract allows users to create
"deposit transactions" on the L1 chain that are automatically executed on the L2 chain within a
bounded amount of time. Additionally, the `OptimismPortal` contract allows users to execute
withdrawal transactions by proving that such a withdrawal was initiated on the L2 chain. The
`OptimismPortal` verifies the correctness of these withdrawal transactions against Output Roots
that have been declared valid by the L1 Fault Proof system.

## Definitions

### Proof Maturity Delay

The **Proof Maturity Delay** is the minimum amount of time that a withdrawal must be a
[Proven Withdrawal](#proven-withdrawal) before it can be finalized.

### Proven Withdrawal

A **Proven Withdrawal** is a withdrawal transaction that has been proven against some Output Root
by a user. Users can prove withdrawals against any Dispute Game contract that meets the following
conditions:

- The game is a [Registered Game](./anchor-state-registry.md#registered-game)
- The game is not a [Retired Game](./anchor-state-registry.md#retired-game)
- The game has a game type that matches the current
  [Respected Game Type](./anchor-state-registry.md#respected-game-type)
- The game has not resolved in favor of the Challenger

Notably, the `OptimismPortal` allows users to prove withdrawals against games that are currently
in progress (games that are not [Resolved Games](./anchor-state-registry.md#resolved-game)).

Note that the `OptimismPortal` currently allows users to prove withdrawals against games that have
been blacklisted, though these withdrawals cannot be [finalized](#finalized-withdrawal). To avoid
user confusion, this functionality will likely be removed in a future release.

Users may re-prove a withdrawal at any time. User withdrawals are stored on a per-user basis such
that re-proving a withdrawal cannot cause the timer for
[finalizing a withdrawal](#finalized-withdrawal) to be reset for another user.

### Finalized Withdrawal

A **Finalized Withdrawal** is a withdrawal transaction that was previously a Proven Withdrawal and
meets a number of additional conditions that allow the withdrawal to be executed.

Users can finalize a withdrawal if they have previously proven the withdrawal and their withdrawal
meets the following conditions:

- Withdrawal is a [Proven Withdrawal](#proven-withdrawal)
- Withdrawal was proven at least [Proof Maturity Delay](#proof-maturity-delay) seconds ago
- Withdrawal was proven against a game with a [Valid Claim](./anchor-state-registry.md#valid-claim)
- Withdrawal was not previously finalized

### Valid Withdrawal

A **Valid Withdrawal** is a withdrawal transaction that was correctly executed on the L2 system as
would be reported by a perfect oracle for the query.

### Invalid Withdrawal

An **Invalid Withdrawal** is any withdrawal that is not a [Valid Withdrawal](#valid-withdrawal).

### Respected Game Type

See [Respected Game Type](./anchor-state-registry.md#respected-game-type). The Respected Game Type
can only be set by the Guardian.

### Retirement Timestamp

Any game whose creation timestamp is less than or equal to the **Retirement Timestamp** is
considered to be a [Retired Game](./anchor-state-registry.md#retired-game). Retired Games cannot be
used to [prove](#proven-withdrawal) or [finalize](#finalized-withdrawal) withdrawals. The
Retirement Timestamp can only be set by the Guardian.

### L2 Withdrawal Sender

The **L2 Withdrawal Sender** is the address of the account that triggered a given withdrawal
transaction on L2. The `OptimismPortal` is expected to expose a variable that includes this value
when [finalizing](#finalized-withdrawal) a withdrawal.

## Assumptions

### aOP-001: Dispute Game contracts properly report important properties

We assume that the `FaultDisputeGame` and `PermissionedDisputeGame` contracts properly and
faithfully report the following properties:

- Game type
- L2 block number
- Root claim value
- Game extra data
- Creation timestamp
- Resolution timestamp
- Resolution result
- Whether the game was the respected game type at creation

We also specifically assume that the game creation timestamp and the resolution timestamp are not
set to values in the future.

#### Mitigations

- Existing audit on the `FaultDisputeGame` contract
- Integration testing

### aOP-002: DisputeGameFactory properly reports its created games

We assume that the `DisputeGameFactory` contract properly and faithfully reports the games it has
created.

#### Mitigations

- Existing audit on the `DisputeGameFactory` contract
- Integration testing

### aOP-003: Incorrectly resolving games will be invalidated within the airgap delay period

We assume that any games that are resolved incorrectly will be invalidated within the airgap delay
period.

#### Mitigations

- Stakeholder incentives / processes
- Incident response plan
- Monitoring

## Invariants

### iOP-001: Invalid Withdrawals can never be finalized

We require that [Invalid Withdrawals](#invalid-withdrawal) can never be
[finalized](#finalized-withdrawal) for any reason.

#### Impact

**Severity: Critical**

If this invariant is broken, any number of arbitrarily bad outcomes could happen. Most obviously,
we would expect all bridge systems relying on the `OptimismPortal` to be immediately compromised.

### iOP-002: Valid Withdrawals can always be finalized in bounded time

We require that [Valid Withdrawals](#valid-withdrawal) can always be
[finalized](#finalized-withdrawal) within some reasonable, bounded amount of time.

#### Impact

**Severity: Critical**

If this invariant is broken, we would expect that users are unable to withdraw bridged assets. We
see this as a critical system risk.

## Function Specification

### setRespectedGameType

Allows the Guardian to change the [Respected Game Type](#respected-game-type) and to update the
[Retirement Timestamp](#retirement-timestamp).

- MUST revert if the sender is not the Guardian.
- If the game type is `type(uint32).max`, MUST update the Retirement Timestamp to the current
  block timestamp and MUST NOT update the Respected Game Type.
- If the game type is not `type(uint32).max`, MUST set the Respected Game Type to the provided game
  type and MUST NOT update the Retirement Timestamp.

### blacklistDisputeGame

Allows the Guardian to [blacklist](./anchor-state-registry.md#blacklisted-game) a Dispute Game.

- MUST revert if the sender is not the Guardian.
- MUST set the provided dispute game as blacklisted.

### proveWithdrawalTransaction

Allows a user to [prove](#proven-withdrawal) a withdrawal transaction.

- MUST revert if the withdrawal is being proven against a game that is not a
  [Registered Game](./anchor-state-registry.md#registered-game).
- MUST revert if the withdrawal is being proven against a game that has a game type not equal to
  the current [Respected Game Type](#respected-game-type).
- MUST revert if the withdrawal is being proven against a game that is not a
  [Respected Game](./anchor-state-registry.md#respected-game).
- MUST revert if the withdrawal is a [Retired Game](./anchor-state-registry.md#retired-game).
- MUST revert if the withdrawal is being proven against a game that has resolved in favor of the
  Challenger.
- MUST revert if the provided merkle trie proof that the withdrawal was included within the root
  claim of the provided dispute game is invalid.
- MUST otherwise store a record of the withdrawal proof that includes the hash of the proven  
  withdrawal, the address of the game against which it was proven, and the block timestamp at which
  the proof transaction was submitted.

### checkWithdrawal

Checks that a withdrawal transaction can be [finalized](#finalized-withdrawal).

- MUST revert if the withdrawal being finalized has not been proven.
- MUST revert if the withdrawal being finalized has been proven less than
  [Proof Maturity Delay](#proof-maturity-delay) seconds ago.
- MUST revert if the withdrawal being finalized was proven against a game that is not a
  [Valid Game](./anchor-state-registry.md#valid-game).
- MUST revert if the withdrawal being finalized has already been finalized.
- MUST otherwise return `true`.

### finalizeWithdrawalTransaction

Allows a user to [finalize](#finalized-withdrawal) a withdrawal transaction.

- MUST revert if the function is called while a previous withdrawal is being executed.
- MUST revert if the withdrawal being finalized does not pass `checkWithdrawal`.
- MUST mark the withdrawal as finalized.
- MUST set the L2 Withdrawal Sender variable correctly.
- MUST execute the withdrawal transaction by executing a contract call to the target address with
  the data and ETH value specified within the withdrawal using AT LEAST the minimum amount of gas
  specified by the withdrawal.
- MUST unset the L2 Withdrawal Sender after the withdrawal call.
