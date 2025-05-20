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
  - [L2 Withdrawal Sender](#l2-withdrawal-sender)
  - [Block Output](#block-output)
  - [Output Root](#output-root)
  - [Super Output](#super-output)
  - [Super Root](#super-root)
- [Assumptions](#assumptions)
  - [aOP-001: Dispute Game contracts properly report important properties](#aop-001-dispute-game-contracts-properly-report-important-properties)
    - [Mitigations](#mitigations)
  - [aOP-002: DisputeGameFactory properly reports its created games](#aop-002-disputegamefactory-properly-reports-its-created-games)
    - [Mitigations](#mitigations-1)
  - [aOP-003: Incorrectly resolving games will be invalidated before they have Valid Claims](#aop-003-incorrectly-resolving-games-will-be-invalidated-before-they-have-valid-claims)
    - [Mitigations](#mitigations-2)
- [Dependencies](#dependencies)
- [Invariants](#invariants)
  - [iOP-001: Invalid Withdrawals can never be finalized](#iop-001-invalid-withdrawals-can-never-be-finalized)
    - [Impact](#impact)
  - [iOP-002: Valid Withdrawals can always be finalized in bounded time](#iop-002-valid-withdrawals-can-always-be-finalized-in-bounded-time)
    - [Impact](#impact-1)
- [Function Specification](#function-specification)
  - [constructor](#constructor)
  - [initialize](#initialize)
  - [paused](#paused)
  - [guardian](#guardian)
  - [proofMaturityDelaySeconds](#proofmaturitydelayseconds)
  - [superRootsActive](#superrootsactive)
  - [disputeGameFactory](#disputegamefactory)
  - [disputeGameFinalityDelaySeconds](#disputegamefinalitydelayseconds)
  - [respectedGameType](#respectedgametype)
  - [respectedGameTypeUpdatedAt](#respectedgametypeupdatedat)
  - [l2Sender](#l2sender)
  - [proveWithdrawalTransaction (Super Roots)](#provewithdrawaltransaction-super-roots)
  - [proveWithdrawalTransaction (Output Roots)](#provewithdrawaltransaction-output-roots)
  - [checkWithdrawal](#checkwithdrawal)
  - [finalizeWithdrawalTransaction](#finalizewithdrawaltransaction)
  - [migrateLiquidity](#migrateliquidity)
  - [migrateToSuperRoots](#migratetosuperroots)
  - [donateETH](#donateeth)
  - [finalizeWithdrawalTransactionExternalProof](#finalizewithdrawaltransactionexternalproof)
  - [numProofSubmitters](#numproofsubmitters)
  - [depositTransaction](#deposittransaction)

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

### L2 Withdrawal Sender

The **L2 Withdrawal Sender** is the address of the account that triggered a given withdrawal
transaction on L2. The `OptimismPortal` is expected to expose a variable that includes this value
when [finalizing](#finalized-withdrawal) a withdrawal.

### Block Output

A **Block Output**, commonly called an **Output**, is a data structure that wraps the key hash
elements of a given L2 block.

The structure of the Block Output is versioned (32 bytes). The current Block Output version is
`0x0000000000000000000000000000000000000000000000000000000000000000` (V0). A V0 Block Output has
the following structure:

```solidity
struct BlockOutput {
  bytes32 version;
  bytes32 stateRoot;
  bytes32 messagePasserStorageRoot;
  bytes32 blockHash;
}
```

Where:

- `version` is a version identifier that describes the structure of the Output Root
- `stateRoot` is the state root of the L2 block this Output Root corresponds to
- `messagePasserStorageRoot` is the storage root of the `L2ToL1MessagePasser` contract at the L2
  block this Output Root corresponds to
- `blockHash` is the block hash of the L2 block this Output Root corresponds to

### Output Root

An **Output Root** is a commitment to a [Block Output](#block-output). A detailed description of
this commitment can be found [here](../../protocol/proposals.md#l2-output-commitment-construction).

### Super Output

A **Super Output** is a data structure that commits all of the [Block Outputs](#block-output) for
all chains within the Superchain Interop Set at a given timestamp. A Super Output can also commit
to a single Block Output to maintain compatibility with chains outside of the Interop Set.

The structure of the Super Output is versioned (1 byte). The current version is `0x01` (V1). A V1
Super Output has the following structure:

```solidity
struct OutputRootWithChainId {
  uint256 chainId;
  bytes32 root;
}

struct SuperOutput {
  uint64 timestamp;
  OutputRootWithChainid[] outputRoots;
}
```

The output root for each chain in the super root MUST be for the block with a timestamp where
$Time_S - BlockTime < Time_B <= Time_S$ where $Time_S$ is the super root timestamp, $BlockTime$ is the chain block time
and $Time_B$ is the block timestamp. That is the output root must be from the last possible block at or before the super
root timestamp.

The output roots in the super root MUST be sorted by chain ID ascending.

### Super Root

A **Super Root** is a commitment to a [Super Output](#super-output), computed as:

```solidity
keccak256(encodeSuperRoot(SuperRoot))
```

Where `encodeSuperRoot` for the V1 Super Output is:

```solidity
function encodeSuperRoot(SuperRoot memory root) returns (bytes) {
  require(root.outputRoots.length > 0); // Super Root must have at least one Output Root.
  return concat(
    0x01, // Super Root version byte
    root.timestamp,
    [
      concat(outputRoot.chainId, outputRoot.root)
      for outputRoot
      in root.outputRoots
    ]
  );
}
```

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

### aOP-003: Incorrectly resolving games will be invalidated before they have Valid Claims

We assume that any games that are resolved incorrectly will be invalidated either by
[blacklisting](./anchor-state-registry.md#blacklisted-game) or by
[retirement](./anchor-state-registry.md#retired-game) BEFORE they are considered to have
[Valid Claims](./anchor-state-registry.md#valid-claim).

Proper Games that resolve in favor the Defender will be considered to have Valid Claims after the
[Dispute Game Finality Delay](./anchor-state-registry.md#dispute-game-finality-delay-airgap) has
elapsed UNLESS the Pause Mechanism is active. Therefore, in the absence of the Pause Mechanism,
parties responsible for game invalidation have exactly the Dispute Game Finality Delay to
invalidate a withdrawal after it resolves incorrectly. If the Pause Mechanism is active, then any
incorrectly resolving games must be invalidated before the pause is deactivated.

#### Mitigations

- Stakeholder incentives / processes
- Incident response plan
- Monitoring

## Dependencies

- [iASR-001](./anchor-state-registry.md#iasr-001-games-are-represented-as-proper-games-accurately)
- [iASR-002](./anchor-state-registry.md#iasr-002-all-valid-claims-are-truly-valid-claims)

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

### constructor

- MUST set the value of the [Proof Maturity Delay](#proof-maturity-delay).

### initialize

- MUST only be callable by the ProxyAdmin or its owner.
- MUST only be triggerable once.
- MUST set the value of the `SystemConfig` contract.
- MUST set the value of the `AnchorStateRegistry` contract.
- MUST set the value of the `ETHLockbox` contract.
- MUST set `superRootsActive` to either `true` or `false`.
- MUST set the value of the [L2 Withdrawal Sender](#l2-withdrawal-sender) variable to the default
  value if the value is not set already.
- MUST initialize the resource metering configuration.

### paused

Returns the current state of the `SystemConfig.paused()` function.

### guardian

Returns the address of the Guardian as per `SystemConfig.guardian()`.

### proofMaturityDelaySeconds

Returns the value of the [Proof Maturity Delay](#proof-maturity-delay).

### superRootsActive

Returns the value of the `superRootsActive` variable which determines if the `OptimismPortal` will
use the standard Output Root proof or the Super Root proof.

### disputeGameFactory

Returns the DisputeGameFactory contract from the AnchorStateRegistry contract.

### disputeGameFinalityDelaySeconds

**Legacy Function**

Returns the value of the
[Dispute Game Finality Delay](./anchor-state-registry.md#dispute-game-finality-delay-airgap) as per
a call to `AnchorStateRegistry.disputeGameFinalityDelaySeconds()`.

### respectedGameType

**Legacy Function**

Returns the value of the current
[Respected Game Type](./anchor-state-registry.md#respected-game-type) as per a call to
`AnchorStateRegistry.respectedGameType`.

### respectedGameTypeUpdatedAt

**Legacy Function**

Returns the value of the current
[Retirement Timestamp](./anchor-state-registry.md#retirement-timestamp) as per a call to
`AnchorStateRegistry.retirementTimestamp.

### l2Sender

Returns the address of the [L2 Withdrawal Sender](#l2-withdrawal-sender). If the `OptimismPortal`
has not been initialized then this value will be `address(0)` and should not be used. If the
`OptimismPortal` is not currently executing an withdrawal transaction then this value will be
`0x000000000000000000000000000000000000dEaD` and should not be used.

### proveWithdrawalTransaction (Super Roots)

Allows a user to [prove](#proven-withdrawal) a withdrawal transaction within an `OptimismPortal`
that uses dispute games that argue over [Super Roots](#super-root).

- MUST revert if the withdrawal target is the address of the `OptimismPortal` itself.
- MUST revert if the withdrawal target is the address of the `ETHLockbox` contract.
- MUST revert if the withdrawal is being proven against a game that is not a
  [Proper Game](./anchor-state-registry.md#proper-game).
- MUST revert if the withdrawal is being proven against a game that is not a
  [Respected Game](./anchor-state-registry.md#respected-game).
- MUST revert if the withdrawal is being proven against a game that has resolved in favor of the
  Challenger.
- MUST revert if `superRootsActive` is `false`.
- MUST revert if the proof provided by the user of the preimage of the Super Root that the dispute
  game argues about is invalid. This proof is verified by hashing the user-provided Super Root
  preimage and comparing them to the Super Root in the referenced dispute game.
- MUST revert if the pointer index of the Output Root inside of the Super Root provided by the user
  is beyond the size of the Output Roots array or points to a chain ID other than the chain ID
  stored within the `SystemConfig` contract for the `OptimismPortal`.
- MUST revert if the proof provided by the user of the preimage of the Output Root is invalid. This
  proof is verified by hashing the user-provided preimage and comparing them to the Output Root.
- MUST revert if the provided merkle trie proof that the withdrawal was included within the root
  claim of the provided dispute game is invalid.
- MUST otherwise store a record of the withdrawal proof that includes the hash of the proven  
  withdrawal, the address of the game against which it was proven, and the block timestamp at which
  the proof transaction was submitted.
- MUST add the proof submitter to the list of submitters for this withdrawal hash.
- MUST emit a `WithdrawalProven` event with the withdrawal hash, sender, and target.
- MUST emit a `WithdrawalProvenExtension1` event with the withdrawal hash and proof submitter address.

### proveWithdrawalTransaction (Output Roots)

Allows a user to [prove](#proven-withdrawal) a withdrawal transaction within an `OptimismPortal`
that uses dispute games that argue over [Output Roots](#output-root).

- MUST revert if the withdrawal target is the address of the `OptimismPortal` itself.
- MUST revert if the withdrawal target is the address of the `ETHLockbox` contract.
- MUST revert if the withdrawal is being proven against a game that is not a
  [Proper Game](./anchor-state-registry.md#proper-game).
- MUST revert if the withdrawal is being proven against a game that is not a
  [Respected Game](./anchor-state-registry.md#respected-game).
- MUST revert if the withdrawal is being proven against a game that has resolved in favor of the
  Challenger.
- MUST revert if `superRootsActive` is `true`.
- MUST revert if the proof provided by the user of the preimage of the Output Root that the dispute
  game argues about is invalid. This proof is verified by hashing the user-provided preimage and
  comparing them to the root claim of the referenced dispute game.
- MUST revert if the provided merkle trie proof that the withdrawal was included within the root
  claim of the provided dispute game is invalid.
- MUST otherwise store a record of the withdrawal proof that includes the hash of the proven  
  withdrawal, the address of the game against which it was proven, and the block timestamp at which
  the proof transaction was submitted.
- MUST add the proof submitter to the list of submitters for this withdrawal hash.
- MUST emit a `WithdrawalProven` event with the withdrawal hash, sender, and target.
- MUST emit a `WithdrawalProvenExtension1` event with the withdrawal hash and proof submitter address.

### checkWithdrawal

Checks that a withdrawal transaction can be [finalized](#finalized-withdrawal).

- MUST revert if the withdrawal being finalized has already been finalized.
- MUST revert if the withdrawal being finalized has not been proven.
- MUST revert if the withdrawal was proven at a timestamp less than or equal to the creation
  timestamp of the dispute game it was proven against, which would signal an unexpected proving
  bug. Note that prevents withdrawals from being proven in the same block that a dispute game is
  created.
- MUST revert if the withdrawal being finalized has been proven less than
  [Proof Maturity Delay](#proof-maturity-delay) seconds ago.
- MUST revert if the withdrawal being finalized was proven against a game that does not have a
  [Valid Claim](./anchor-state-registry.md#valid-claim).

### finalizeWithdrawalTransaction

Allows a user to [finalize](#finalized-withdrawal) a withdrawal transaction.

- MUST revert if the function is called while a previous withdrawal is being executed.
- MUST revert if the withdrawal being finalized does not pass `checkWithdrawal`.
- MUST mark the withdrawal as finalized.
- MUST set the L2 Withdrawal Sender variable correctly.
- MUST unlock ETH from the ETHLockbox if the withdrawal includes an ETH value.
- MUST execute the withdrawal transaction by executing a contract call to the target address with
  the data and ETH value specified within the withdrawal using AT LEAST the minimum amount of gas
  specified by the withdrawal.
- MUST unset the L2 Withdrawal Sender after the withdrawal call.
- MUST emit a `WithdrawalFinalized` event with the withdrawal hash and success status.
- MUST lock any unused ETH back into the ETHLockbox if the call to the target address fails.

### migrateLiquidity

Allows the ProxyAdmin owner to migrate the total ETH balance to the ETHLockbox contract.

- MUST revert if called by any address other than the ProxyAdmin owner.
- MUST transfer the entire ETH balance of the OptimismPortal to the ETHLockbox contract.
- MUST emit an ETHMigrated event with the ETHLockbox address and the migrated ETH amount.
- MAY be called even if the system is paused.

### migrateToSuperRoots

Allows the ProxyAdmin owner to migrate the OptimismPortal to use a new ETHLockbox, point at a new
AnchorStateRegistry, and start using the Super Roots proof method.

- MUST revert if the system is paused.
- MUST revert if called by any address other than the ProxyAdmin owner.
- MUST revert if the new AnchorStateRegistry is the same as the current one.
- MUST update the ETHLockbox address to the provided new ETHLockbox address.
- MUST update the AnchorStateRegistry address to the provided new AnchorStateRegistry address.
- MUST set the superRootsActive flag to true, enabling Super Roots proof method.
- MUST emit a PortalMigrated event with the old and new ETHLockbox and AnchorStateRegistry
  addresses.

### donateETH

Allows any address to donate ETH to the contract without triggering a deposit to L2.

- MUST accept ETH payments via the payable modifier.
- MUST not perform any state-changing operations.
- MUST not trigger a deposit transaction to L2.

### finalizeWithdrawalTransactionExternalProof

Allows a user to [finalize](#finalized-withdrawal) a withdrawal transaction using a proof submitted
by another address.

- MUST revert if the system is paused.
- MUST revert if the function is called while a previous withdrawal is being executed.
- MUST revert if the target address is the OptimismPortal or the ETHLockbox. Note that the target
  address is permitted to be the address of *another* OptimismPortal contract but it cannot be the
  address if this contract.
- MUST revert if the withdrawal being finalized does not pass `checkWithdrawal` when using the specified proof submitter.
- MUST mark the withdrawal as finalized.
- MUST set the L2 Withdrawal Sender variable correctly.
- MUST unlock ETH from the ETHLockbox if the withdrawal includes an ETH value.
- MUST execute the withdrawal transaction by executing a contract call to the target address with
  the data and ETH value specified within the withdrawal using AT LEAST the minimum amount of gas
  specified by the withdrawal.
- MUST unset the L2 Withdrawal Sender after the withdrawal call.
- MUST emit a `WithdrawalFinalized` event with the withdrawal hash and success status.
- MUST lock any unused ETH back into the ETHLockbox if the call to the target address fails.

### numProofSubmitters

Returns the number of proof submitters for a given withdrawal hash.

- MUST return the length of the proofSubmitters array for the specified withdrawal hash.
- MUST NOT change state.

### depositTransaction

Accepts deposits of ETH and data, and emits a TransactionDeposited event for use in
deriving deposit transactions. Note that if a deposit is made by a contract, its
address will be aliased when retrieved using `tx.origin` or `msg.sender`. Consider
using the CrossDomainMessenger contracts for a simpler developer experience.

- MUST lock any ETH value (msg.value) in the ETHLockbox contract.
- MUST revert if the target address is not address(0) for contract creations.
- MUST revert if the gas limit provided is too low based on the calldata size.
- MUST revert if the calldata is too large (> 120,000 bytes).
- MUST transform the sender address to its alias if the caller is a contract.
- MUST emit a TransactionDeposited event with the from address, to address, deposit version, and opaque data.
