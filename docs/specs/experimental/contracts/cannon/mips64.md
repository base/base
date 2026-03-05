# MIPS64

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Thread Stack](#thread-stack)
  - [Memory Reservation](#memory-reservation)
  - [Context Switch](#context-switch)
  - [State Version](#state-version)
- [Assumptions](#assumptions)
  - [a01-001: PreimageOracle provides valid data](#a01-001-preimageoracle-provides-valid-data)
    - [Mitigations](#mitigations)
  - [a01-002: Proof data is well-formed](#a01-002-proof-data-is-well-formed)
    - [Mitigations](#mitigations-1)
- [Invariants](#invariants)
  - [i01-001: Deterministic state transitions](#i01-001-deterministic-state-transitions)
    - [Impact](#impact)
  - [i01-002: MIPS64 semantics conformance](#i01-002-mips64-semantics-conformance)
    - [Impact](#impact-1)
- [Function Specification](#function-specification)
  - [step](#step)
  - [oracle](#oracle)
  - [stateVersion](#stateversion)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The MIPS64 contract is an on-chain implementation of a MIPS64 virtual machine that executes a single instruction step.
It serves as the state transition verification component in the Cannon fault proof system, enabling on-chain validation
of off-chain computation by proving correct execution of MIPS64 instructions within dispute games.

## Definitions

### Thread Stack

A hash onion structure representing a stack of thread states. Each thread is committed by computing
`Keccak256(previous_root ++ thread_hash)`, where the empty stack is represented by
`Keccak256(bytes32(0) ++ bytes32(0))`. This construction allows succinct commitment to all threads using a single
bytes32 value. See the [Cannon Fault Proof VM specification](../../fault-proof/cannon-fault-proof-vm.md#thread-stack-hashing)
for detailed hashing mechanics.

### Memory Reservation

A mechanism enabling atomic read-modify-write operations through Load Linked and Store Conditional instructions.
A reservation tracks a memory address, the owning thread, and the operation size (32-bit or 64-bit). Any memory write
to the reserved address clears the reservation, ensuring atomicity across concurrent threads.

### Context Switch

The process of preempting the currently active thread and activating another thread from the thread stacks.
Context switches occur voluntarily through specific syscalls or forcibly after executing a quantum of steps without
yielding.

### State Version

An identifier specifying the state transition rule set implemented by the contract. The identifier is returned by
stateVersion() and is treated as an opaque selector by callers.

## Assumptions

### a01-001: PreimageOracle provides valid data

The PreimageOracle contract returns valid preimage data when queried during read syscalls. If the oracle returns
incorrect or malicious data, the VM state transition will be incorrect, potentially causing invalid state roots to be
accepted in dispute games.

#### Mitigations

- The PreimageOracle contract is trusted and governance-controlled
- Preimage data is deterministically derived from the preimage key
- Dispute game participants can independently verify preimage correctness off-chain

### a01-002: Proof data is well-formed

Callers provide structurally valid proof data including memory proofs and thread witness data. While the contract
validates proof correctness against state commitments, it assumes the proof structure matches expected formats and
offsets.

#### Mitigations

- Proof validation logic verifies merkle proofs against state roots
- Invalid proofs cause reverts rather than incorrect state transitions
- Off-chain proof generation is deterministic and can be independently verified

## Invariants

### i01-001: Deterministic state transitions

For any given pre-state and input proofs, the step function produces a unique post-state. Independent verifiers
computing the same state transition will arrive at identical results. This determinism is essential for fault proofs,
where multiple parties must independently verify the same computation.

#### Impact

**Severity: High**

Non-deterministic state transitions would make it impossible to resolve disputes, as honest parties could produce
different valid state roots for the same input. This would completely break the fault proof system. The severity is High
rather than Critical due to the airgap protection in the dispute game system.

### i01-002: MIPS64 semantics conformance

Each step execution must match the effects of executing one MIPS64 instruction under the semantics defined in the
[Cannon Fault Proof VM specification](../../../fault-proof/cannon-fault-proof-vm.md). Any divergence from the MIPS64
specification is a bug. Divergences that affect instructions relied upon by the Go compiler are High severity bugs.
Divergences affecting unreachable or unimplemented opcodes not used by the compiler are Low severity bugs.

#### Impact

**Severity: High**

Incorrect MIPS64 instruction execution would cause the on-chain VM to produce different state transitions than
off-chain execution, breaking the fault proof system's ability to verify computation correctly. The severity is High
rather than Critical due to the airgap protection in the dispute game system. Divergences affecting opcodes not used by
the Go compiler would be Low severity.

## Function Specification

### step

Executes a single MIPS64 instruction step and returns the resulting state hash.

**Parameters:**

- `_stateData`: Encoded VM state (188 bytes packed)
- `_proof`: Encoded proof data containing thread witness and memory proofs
- `_localContext`: Local key context for preimage oracle queries

**Behavior:**

- MUST revert if the [State Version](#state-version) is unsupported
- MUST revert if the active [Thread Stack](#thread-stack) is empty when the VM has not exited
- MUST revert if proof data is insufficient or incorrectly formatted
- MUST revert if memory proofs are invalid for the provided state root
- MUST revert if thread witness does not match the current [Thread Stack](#thread-stack) root
- MUST return the post-state without modification if the VM has already exited
- MUST increment the step counter by 1
- MUST pop and remove exited threads from the active stack
- MUST preempt the current thread if stepsSinceLastContextSwitch reaches the scheduling quantum
- MUST increment stepsSinceLastContextSwitch by 1 for each instruction executed on the current thread
- MUST fetch and decode the instruction at the current program counter
- MUST handle syscalls by calling handleSyscall when opcode is 0 and function is 0xC
- MUST handle Load Linked and Store Conditional instructions through handleRMWOps
- MUST execute the instruction and update memory root, registers, and CPU scalars accordingly
- MUST update the current [Thread Stack](#thread-stack) root after modifying thread state
- MUST clear [Memory Reservation](#memory-reservation)s when writing to reserved addresses
- MUST compute and return the Keccak256 hash of the post-state with VM status in the high-order byte

### oracle

Returns the PreimageOracle contract address.

**Behavior:**

- MUST return the immutable ORACLE address set during construction

### stateVersion

Returns the [State Version](#state-version) identifier.

**Behavior:**

- MUST return the immutable STATE_VERSION value set during construction
