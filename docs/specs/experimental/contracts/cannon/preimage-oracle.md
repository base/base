# PreimageOracle

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Preimage Key](#preimage-key)
  - [Large Preimage Proposal](#large-preimage-proposal)
  - [Challenge Period](#challenge-period)
  - [Preimage Part](#preimage-part)
  - [Keccak Block](#keccak-block)
  - [Bond](#bond)
- [Assumptions](#assumptions)
  - [a01-001: Challengers Monitor Proposals](#a01-001-challengers-monitor-proposals)
    - [Mitigations](#mitigations)
  - [a01-002: Sufficient Gas for Precompile Calls](#a01-002-sufficient-gas-for-precompile-calls)
    - [Mitigations](#mitigations-1)
- [Invariants](#invariants)
  - [i01-001: Preimage Data Integrity](#i01-001-preimage-data-integrity)
    - [Impact](#impact)
  - [i01-002: Large Preimage Proposal Finality](#i01-002-large-preimage-proposal-finality)
    - [Impact](#impact-1)
  - [i01-003: Challenge Correctness](#i01-003-challenge-correctness)
    - [Impact](#impact-2)
  - [i01-004: Bond Conservation](#i01-004-bond-conservation)
    - [Impact](#impact-3)
  - [i01-005: Local Data Isolation](#i01-005-local-data-isolation)
    - [Impact](#impact-4)
- [Function Specification](#function-specification)
  - [readPreimage](#readpreimage)
  - [loadLocalData](#loadlocaldata)
  - [loadKeccak256PreimagePart](#loadkeccak256preimagepart)
  - [loadSha256PreimagePart](#loadsha256preimagepart)
  - [loadBlobPreimagePart](#loadblobpreimagepart)
  - [loadPrecompilePreimagePart](#loadprecompilepreimagepart)
  - [initLPP](#initlpp)
  - [addLeavesLPP](#addleaveslpp)
  - [challengeLPP](#challengelpp)
  - [challengeFirstLPP](#challengefirstlpp)
  - [squeezeLPP](#squeezelpp)
  - [getTreeRootLPP](#gettreerootlpp)
  - [proposalCount](#proposalcount)
  - [proposalBlocksLen](#proposalblockslen)
  - [challengePeriod](#challengeperiod)
  - [minProposalSize](#minproposalsize)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The PreimageOracle provides data availability for fault proof execution by storing preimage data indexed by
cryptographic keys. It supports multiple preimage types including local data, hash-based preimages, blob data, and
precompile results. For large preimages that cannot be submitted in a single transaction, it implements a Large
Preimage Proposal system with a challenge mechanism to ensure data integrity.

## Definitions

### Preimage Key

A 32-byte identifier for preimage data where the first byte indicates the preimage type and the remaining 31 bytes
contain type-specific data. Preimage types include: local (0x01), keccak256 (0x02), sha256 (0x04), blob (0x05), and
precompile (0x06).

### Large Preimage Proposal

A mechanism for submitting preimages larger than the minimum proposal size through an incremental merkleization
process. Proposals include keccak256 state commitments for each 136-byte block and are subject to a challenge period
before finalization.

### Challenge Period

The time duration after a Large Preimage Proposal is finalized during which anyone can challenge the correctness of
the keccak256 computation. If no successful challenge occurs during this period, the proposal can be squeezed to make
the preimage data available.

### Preimage Part

A 32-byte segment of preimage data at a specific offset. Preimages are stored with an 8-byte big-endian length prefix
followed by the actual data, and can be read in 32-byte parts at any valid offset.

### Keccak Block

A 136-byte chunk of data absorbed during keccak256 computation. Large Preimage Proposals process data in keccak blocks
and commit to the internal state after each block is absorbed and permuted.

### Bond

ETH locked when creating a Large Preimage Proposal. The bond is returned to the claimant if the proposal passes the
challenge period unchallenged, or paid to a challenger if the proposal is successfully challenged.

## Assumptions

### a01-001: Challengers Monitor Proposals

Honest challengers monitor [Large Preimage Proposal](#large-preimage-proposal)s and challenge any proposals with
incorrect keccak256 computations
during the [Challenge Period](#challenge-period).

#### Mitigations

- [Challenge Period](#challenge-period) provides sufficient time for monitoring
- [Bond](#bond) incentivizes challenges by rewarding successful challengers
- Proposals are publicly observable through events and storage

### a01-002: Sufficient Gas for Precompile Calls

Callers of loadPrecompilePreimagePart provide sufficient gas to execute the precompile call as specified by the
requiredGas parameter.

#### Mitigations

- Contract enforces minimum gas check before calling precompile
- Reserved gas buffer accounts for call setup overhead

## Invariants

### i01-001: Preimage Data Integrity

Once a [Preimage Part](#preimage-part) is marked as available (preimagePartOk is true), the stored data at that key and
offset never
changes and always corresponds to the correct preimage for that key.

#### Impact

**Severity: Critical**

If preimage data could be modified after being stored, fault proofs could be invalidated or manipulated, allowing
invalid state transitions to be proven valid or preventing valid proofs from being generated.

### i01-002: [Large Preimage Proposal](#large-preimage-proposal) Finality

Once a [Large Preimage Proposal](#large-preimage-proposal) passes the [Challenge Period](#challenge-period) without
being countered, the preimage data it represents
becomes permanently available and cannot be modified or removed.

#### Impact

**Severity: Critical**

If finalized proposals could be modified or removed, fault proofs depending on that data would fail, breaking the
security guarantees of the fault proof system.

### i01-003: Challenge Correctness

A [Large Preimage Proposal](#large-preimage-proposal) can only be successfully challenged if it contains an incorrect
keccak256 state transition,
and all proposals with incorrect state transitions can be successfully challenged.

#### Impact

**Severity: Critical**

If incorrect proposals could pass unchallenged or correct proposals could be falsely challenged, the integrity of the
preimage data would be compromised, allowing invalid fault proofs or preventing valid ones.

### i01-004: [Bond](#bond) Conservation

The total ETH held in proposal [Bond](#bond)s equals the sum of all active proposal [Bond](#bond)s, and [Bond](#bond)s
are only transferred to
the claimant (on successful squeeze) or challenger (on successful challenge).

#### Impact

**Severity: High**

If [Bond](#bond)s could be lost or stolen, the economic security of the [Large Preimage
Proposal](#large-preimage-proposal) system would be compromised,
reducing incentives for honest behavior.

### i01-005: Local Data Isolation

Preimage data loaded via loadLocalData for a given caller and local context can only be written by that specific
caller, ensuring isolation between different callers' local data.

#### Impact

**Severity: High**

If local data could be overwritten by other callers, fault proofs could be manipulated by providing incorrect local
context data, compromising the security of the dispute resolution process.

## Function Specification

### readPreimage

Reads a [Preimage Part](#preimage-part) from storage.

**Parameters:**
- `_key`: The [Preimage Key](#preimage-key) identifying the preimage
- `_offset`: The byte offset within the preimage (including 8-byte length prefix)

**Behavior:**
- MUST revert if the [Preimage Part](#preimage-part) at the specified key and offset has not been loaded (preimagePartOk
  is false)
- MUST return up to 32 bytes of preimage data starting at the offset
- MUST return the actual length of data available, which may be less than 32 bytes if the offset is near the end of
the preimage
- MUST include the 8-byte big-endian length prefix in offset calculations (offset 0 starts at the length prefix)

### loadLocalData

Loads caller-specific local data into the preimage oracle.

**Parameters:**
- `_ident`: The local data identifier (1=L1 Head Hash, 2=Output Root, 3=Root Claim, 4=L2 Block Number, 5=Chain ID)
- `_localContext`: Context value for key localization
- `_word`: The data word to store
- `_size`: Number of bytes in the word to load (maximum 32)
- `_partOffset`: Offset within the preimage to write

**Behavior:**
- MUST compute the localized key using the identifier, caller address, and local context
- MUST revert if partOffset is greater than or equal to size + 8
- MUST revert if size is greater than 32
- MUST store the [Preimage Part](#preimage-part) at the computed key and offset
- MUST mark the [Preimage Part](#preimage-part) as available
- MUST store the preimage length as the provided size
- MUST return the computed localized key

### loadKeccak256PreimagePart

Prepares a keccak256 [Preimage Part](#preimage-part) for reading.

**Parameters:**
- `_partOffset`: Offset within the preimage to prepare
- `_preimage`: The complete preimage data

**Behavior:**
- MUST revert if partOffset is greater than or equal to the preimage size + 8
- MUST compute the [Preimage Key](#preimage-key) as keccak256(preimage) with the first byte set to 0x02
- MUST store the 32-byte [Preimage Part](#preimage-part) starting at the specified offset (including 8-byte length
  prefix)
- MUST mark the [Preimage Part](#preimage-part) as available
- MUST store the preimage length

### loadSha256PreimagePart

Prepares a sha256 [Preimage Part](#preimage-part) for reading.

**Parameters:**
- `_partOffset`: Offset within the preimage to prepare
- `_preimage`: The complete preimage data

**Behavior:**
- MUST revert if partOffset is greater than or equal to the preimage size + 8
- MUST compute the [Preimage Key](#preimage-key) using the SHA-256 precompile with the first byte set to 0x04
- MUST revert if the SHA-256 precompile call fails
- MUST store the 32-byte [Preimage Part](#preimage-part) starting at the specified offset (including 8-byte length
  prefix)
- MUST mark the [Preimage Part](#preimage-part) as available
- MUST store the preimage length

### loadBlobPreimagePart

Verifies and loads a blob [Preimage Part](#preimage-part) using KZG proof verification.

**Parameters:**
- `_z`: The evaluation point (big-endian)
- `_y`: The evaluation result (big-endian), which is the preimage data
- `_commitment`: The KZG commitment (48 bytes)
- `_proof`: The KZG proof (48 bytes)
- `_partOffset`: Offset within the preimage to prepare

**Behavior:**
- MUST verify the KZG proof using the point evaluation precompile (address 0x0A)
- MUST revert if the KZG proof verification fails
- MUST revert if partOffset is greater than or equal to 40 (32 bytes data + 8 bytes prefix)
- MUST compute the [Preimage Key](#preimage-key) as keccak256(commitment || z) with the first byte set to 0x05
- MUST store the 32-byte [Preimage Part](#preimage-part) (the value y) at the specified offset
- MUST mark the [Preimage Part](#preimage-part) as available
- MUST store the preimage length as 32

### loadPrecompilePreimagePart

Executes a precompile and stores the result as a preimage.

**Parameters:**
- `_partOffset`: Offset within the preimage result to prepare
- `_precompile`: Address of the precompile to call
- `_requiredGas`: Minimum gas required for the precompile execution
- `_input`: Input data for the precompile call

**Behavior:**
- MUST verify sufficient gas is available (gas >= (requiredGas * 64 / 63) + PRECOMPILE_CALL_RESERVED_GAS)
- MUST revert if insufficient gas is available
- MUST call the precompile with the provided input
- MUST compute the [Preimage Key](#preimage-key) as keccak256(precompile || requiredGas || input) with the first byte
  set to 0x06
- MUST store the result as a 1-byte status (success/failure) followed by the return data
- MUST revert if partOffset is greater than or equal to the result size + 8
- MUST store the [Preimage Part](#preimage-part) at the specified offset
- MUST mark the [Preimage Part](#preimage-part) as available
- MUST store the preimage length as 1 + returndatasize

### initLPP

Initializes a new [Large Preimage Proposal](#large-preimage-proposal).

**Parameters:**
- `_uuid`: Unique identifier for the proposal
- `_partOffset`: Offset of the [Preimage Part](#preimage-part) to extract
- `_claimedSize`: Total size of the preimage being proposed

**Behavior:**
- MUST accept ETH payment as the proposal [Bond](#bond)
- MUST revert if the [Bond](#bond) is less than MIN_[Bond](#bond)_SIZE (0.25 ether)
- MUST revert if the caller is not an EOA (legacy check, can be bypassed with EIP-7702)
- MUST revert if partOffset is greater than or equal to claimedSize + 8
- MUST revert if claimedSize is less than MIN_LPP_SIZE_BYTES
- MUST revert if a proposal with the same claimant and uuid already exists
- MUST initialize proposal metadata with the part offset and claimed size
- MUST add the proposal to the proposals array
- MUST store the [Bond](#bond) amount

### addLeavesLPP

Adds keccak256 state commitments to a [Large Preimage Proposal](#large-preimage-proposal) merkle tree.

**Parameters:**
- `_uuid`: Unique identifier for the proposal
- `_inputStartBlock`: Expected starting block number for this input
- `_input`: Input data to process (must be multiple of 136 bytes)
- `_stateCommitments`: Array of keccak256 state commitments after each block
- `_finalize`: Whether to finalize the proposal after adding these leaves

**Behavior:**
- MUST pad the input if finalizing, otherwise use input as-is
- MUST revert if the caller is not an EOA (legacy check, can be bypassed with EIP-7702)
- MUST revert if the proposal has not been initialized
- MUST revert if the proposal has already been finalized
- MUST revert if inputStartBlock does not match the number of blocks already processed
- MUST revert if input length is not a multiple of 136 bytes
- MUST revert if the number of state commitments does not equal input length / 136
- MUST extract and store the [Preimage Part](#preimage-part) if the part offset falls within the current input
- MUST add each [Keccak Block](#keccak-block) to the merkle tree with its corresponding state commitment
- MUST revert if the total blocks processed exceeds MAX_LEAF_COUNT (2^16 - 1)
- MUST update metadata with blocks processed and bytes processed
- MUST set the timestamp to current block timestamp if finalizing
- MUST revert if finalizing and bytes processed does not equal claimed size
- MUST store the block number where leaves were added
- MUST emit a log0 event with caller address and all calldata

### challengeLPP

Challenges an incorrect keccak256 state transition in a [Large Preimage Proposal](#large-preimage-proposal).

**Parameters:**
- `_claimant`: Address of the proposal claimant
- `_uuid`: Unique identifier for the proposal
- `_stateMatrix`: The keccak256 state matrix before the postState block
- `_preState`: The leaf containing the pre-state commitment
- `_preStateProof`: Merkle proof for the pre-state leaf
- `_postState`: The leaf containing the post-state commitment
- `_postStateProof`: Merkle proof for the post-state leaf

**Behavior:**
- MUST verify both leaves are present in the proposal's merkle tree
- MUST revert if either merkle proof is invalid
- MUST verify the provided state matrix matches the preState commitment
- MUST revert if the state matrix does not match
- MUST verify preState and postState are contiguous (postState.index = preState.index + 1)
- MUST revert if the states are not contiguous
- MUST absorb the postState input into the state matrix and permute
- MUST verify the resulting state does NOT match the postState commitment
- MUST revert if the resulting state matches (challenge is invalid)
- MUST mark the proposal as countered
- MUST transfer the [Bond](#bond) to the challenger

### challengeFirstLPP

Challenges the first keccak256 block in a [Large Preimage Proposal](#large-preimage-proposal).

**Parameters:**
- `_claimant`: Address of the proposal claimant
- `_uuid`: Unique identifier for the proposal
- `_postState`: The first leaf in the proposal
- `_postStateProof`: Merkle proof for the first leaf

**Behavior:**
- MUST verify the leaf is present in the proposal's merkle tree
- MUST revert if the merkle proof is invalid
- MUST verify the postState index is 0
- MUST revert if the index is not 0
- MUST absorb the postState input into a fresh state matrix and permute
- MUST verify the resulting state does NOT match the postState commitment
- MUST revert if the resulting state matches (challenge is invalid)
- MUST mark the proposal as countered
- MUST transfer the [Bond](#bond) to the challenger

### squeezeLPP

Finalizes a [Large Preimage Proposal](#large-preimage-proposal) after the [Challenge Period](#challenge-period), making
the preimage data available.

**Parameters:**
- `_claimant`: Address of the proposal claimant
- `_uuid`: Unique identifier for the proposal
- `_stateMatrix`: The keccak256 state matrix before the final block
- `_preState`: The second-to-last leaf in the proposal
- `_preStateProof`: Merkle proof for the pre-state leaf
- `_postState`: The final leaf in the proposal
- `_postStateProof`: Merkle proof for the post-state leaf

**Behavior:**
- MUST revert if the proposal has been countered
- MUST revert if the proposal has not been finalized (timestamp is 0)
- MUST revert if the [Challenge Period](#challenge-period) has not elapsed
- MUST verify both leaves are present in the proposal's merkle tree
- MUST revert if either merkle proof is invalid
- MUST verify the provided state matrix matches the preState commitment
- MUST revert if the state matrix does not match
- MUST verify preState and postState are contiguous and postState is the final block
- MUST revert if the states are not contiguous or postState is not final
- MUST absorb the postState input, permute, and squeeze to get the final keccak256 digest
- MUST set the first byte of the digest to 0x02 (keccak256 preimage type)
- MUST store the [Preimage Part](#preimage-part) at the specified offset with the computed key
- MUST mark the [Preimage Part](#preimage-part) as available
- MUST store the preimage length
- MUST transfer the [Bond](#bond) to the claimant

### getTreeRootLPP

Computes the merkle root of a [Large Preimage Proposal](#large-preimage-proposal).

**Parameters:**
- `_owner`: Address of the proposal claimant
- `_uuid`: Unique identifier for the proposal

**Behavior:**
- MUST compute the merkle root from the proposal's branch and the number of blocks processed
- MUST use zero hashes for empty branches
- MUST return the computed root

### proposalCount

Returns the total number of [Large Preimage Proposal](#large-preimage-proposal)s created.

**Behavior:**
- MUST return the length of the proposals array

### proposalBlocksLen

Returns the number of addLeavesLPP calls made for a proposal.

**Parameters:**
- `_claimant`: Address of the proposal claimant
- `_uuid`: Unique identifier for the proposal

**Behavior:**
- MUST return the length of the proposalBlocks array for the specified proposal

### challengePeriod

Returns the [Challenge Period](#challenge-period) duration.

**Behavior:**
- MUST return the CHALLENGE_PERIOD immutable value

### minProposalSize

Returns the minimum size for [Large Preimage Proposal](#large-preimage-proposal)s.

**Behavior:**
- MUST return the MIN_LPP_SIZE_BYTES immutable value
