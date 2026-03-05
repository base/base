# L1Block

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Depositor Account](#depositor-account)
  - [Sequence Number](#sequence-number)
  - [Epoch](#epoch)
- [Assumptions](#assumptions)
  - [a01-001: Depositor Account Provides Accurate L1 Data](#a01-001-depositor-account-provides-accurate-l1-data)
    - [Mitigations](#mitigations)
- [Invariants](#invariants)
  - [i01-001: Exclusive Depositor Write Access](#i01-001-exclusive-depositor-write-access)
    - [Impact](#impact)
  - [i01-002: L1 Context Consistency Within Epoch](#i01-002-l1-context-consistency-within-epoch)
    - [Impact](#impact-1)
- [Function Specification](#function-specification)
  - [DEPOSITOR_ACCOUNT](#depositor_account)
  - [gasPayingToken](#gaspayingtoken)
  - [gasPayingTokenName](#gaspayingtokenname)
  - [gasPayingTokenSymbol](#gaspayingtokensymbol)
  - [isCustomGasToken](#iscustomgastoken)
  - [setL1BlockValues](#setl1blockvalues)
  - [setL1BlockValuesEcotone](#setl1blockvaluesecotone)
  - [setL1BlockValuesIsthmus](#setl1blockvaluesisthmus)
  - [setL1BlockValuesJovian](#setl1blockvaluesjovian)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

Maintains L1 block context on L2 by storing the latest known L1 block attributes and system configuration
parameters. This contract serves as the authoritative source for L1 state information used in L2 transaction fee
calculations and protocol operations.

## Definitions

### Depositor Account

The special system address (0xDeaDDEaDDeAdDeAdDEAdDEaddeAddEAdDEAd0001) authorized to update L1 block values. This
account is controlled by the protocol and submits L1 attributes as the first transaction in each L2 block when
moving to a new epoch.

### Sequence Number

The count of L2 blocks produced since the start of the current epoch. This value resets to 0 when the L1 origin
block changes, indicating the beginning of a new epoch.

### Epoch

A period of L2 block production tied to a specific L1 block. Each epoch begins when the L1 origin block changes and
contains one or more L2 blocks that reference the same L1 block context.

## Assumptions

### a01-001: [Depositor Account](#depositor-account) Provides Accurate L1 Data

The [Depositor Account](#depositor-account) is trusted to provide accurate and timely L1 block attributes including
block numbers, timestamps, base fees, blob base fees, and batcher authentication hashes. The protocol relies on this
data for fee calculations and system configuration.

#### Mitigations

- The [Depositor Account](#depositor-account) is controlled by the protocol's derivation pipeline, which
  deterministically derives L1
  attributes from verifiable L1 block data
- L1 attributes can be independently verified by comparing against L1 block headers

## Invariants

### i01-001: Exclusive Depositor Write Access

Only the [Depositor Account](#depositor-account) can modify L1 block attributes and system configuration values
stored in this contract. All setter functions across different network upgrade versions enforce this restriction.

#### Impact

**Severity: Critical**

If unauthorized addresses could update L1 block values, attackers could manipulate fee calculations to avoid paying
L1 data availability costs, corrupt system configuration parameters like batcher authentication, or provide false L1
context that breaks protocol assumptions. This would compromise the economic security and operational integrity of
the entire L2 system.

### i01-002: L1 Context Consistency Within [Epoch](#epoch)

Within a single [Epoch](#epoch), the L1 block attributes (number, timestamp, basefee, hash, blobBaseFee, batcherHash)
remain constant while only the [Sequence Number](#sequence-number) increments. L1 context only changes when
transitioning to a new [Epoch](#epoch).

#### Impact

**Severity: High**

If L1 attributes could change arbitrarily within an [Epoch](#epoch), it would violate the protocol's derivation model
where
multiple L2 blocks are anchored to a single L1 block. This could cause inconsistent fee calculations across L2 blocks
in the same [Epoch](#epoch) and break the deterministic relationship between L1 and L2 block production.

## Function Specification

### DEPOSITOR_ACCOUNT

Returns the address of the special [Depositor Account](#depositor-account).

**Parameters:**
None

**Behavior:**
- MUST return the constant address 0xDeaDDEaDDeAdDeAdDEAdDEaddeAddEAdDEAd0001

### gasPayingToken

Returns the gas paying token address and decimals.

**Parameters:**
None

**Behavior:**
- MUST return address 0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE (representing Ether)
- MUST return decimals value of 18

### gasPayingTokenName

Returns the name of the gas paying token.

**Parameters:**
None

**Behavior:**
- MUST return the string "Ether"

### gasPayingTokenSymbol

Returns the symbol of the gas paying token.

**Parameters:**
None

**Behavior:**
- MUST return the string "ETH"

### isCustomGasToken

Indicates whether the network uses a custom gas token.

**Parameters:**
None

**Behavior:**
- MUST return false

### setL1BlockValues

Updates L1 block values using the legacy pre-Ecotone format.

**Parameters:**
- `_number`: L1 block number
- `_timestamp`: L1 block timestamp
- `_basefee`: L1 base fee
- `_hash`: L1 block hash
- `_sequenceNumber`: Number of L2 blocks since [Epoch](#epoch) start
- `_batcherHash`: Versioned hash to authenticate batcher
- `_l1FeeOverhead`: L1 fee overhead (legacy parameter)
- `_l1FeeScalar`: L1 fee scalar (legacy parameter)

**Behavior:**
- MUST revert if caller is not the [Depositor Account](#depositor-account)
- MUST update `number` to `_number`
- MUST update `timestamp` to `_timestamp`
- MUST update `basefee` to `_basefee`
- MUST update `hash` to `_hash`
- MUST update `sequenceNumber` to `_sequenceNumber`
- MUST update `batcherHash` to `_batcherHash`
- MUST update `l1FeeOverhead` to `_l1FeeOverhead`
- MUST update `l1FeeScalar` to `_l1FeeScalar`

### setL1BlockValuesEcotone

Updates L1 block values using the Ecotone upgrade format with packed calldata encoding.

**Parameters:**
Parameters are packed in calldata rather than ABI-encoded:
- Bytes 4-7: `baseFeeScalar` (uint32)
- Bytes 8-11: `blobBaseFeeScalar` (uint32)
- Bytes 12-19: `sequenceNumber` (uint64)
- Bytes 20-27: `timestamp` (uint64)
- Bytes 28-35: `number` (uint64)
- Bytes 36-67: `basefee` (uint256)
- Bytes 68-99: `blobBaseFee` (uint256)
- Bytes 100-131: `hash` (bytes32)
- Bytes 132-163: `batcherHash` (bytes32)

**Behavior:**
- MUST revert if caller is not the [Depositor Account](#depositor-account)
- MUST extract and store `baseFeeScalar`, `blobBaseFeeScalar`, and `sequenceNumber` from bytes 4-19
- MUST extract and store `number` and `timestamp` from bytes 20-35
- MUST extract and store `basefee` from bytes 36-67
- MUST extract and store `blobBaseFee` from bytes 68-99
- MUST extract and store `hash` from bytes 100-131
- MUST extract and store `batcherHash` from bytes 132-163

### setL1BlockValuesIsthmus

Updates L1 block values using the Isthmus upgrade format, extending Ecotone with operator fee parameters.

**Parameters:**
Extends Ecotone parameters with:
- Bytes 164-167: `operatorFeeScalar` (uint32)
- Bytes 168-175: `operatorFeeConstant` (uint64)

**Behavior:**
- MUST call `setL1BlockValuesEcotone()` to process bytes 4-163
- MUST extract and store `operatorFeeScalar` and `operatorFeeConstant` from bytes 164-175

### setL1BlockValuesJovian

Updates L1 block values using the Jovian upgrade format, extending Isthmus with DA footprint gas scalar.

**Parameters:**
Extends Isthmus parameters with:
- Bytes 176-177: `daFootprintGasScalar` (uint16)

**Behavior:**
- MUST call `setL1BlockValuesEcotone()` to process bytes 4-163
- MUST extract `operatorFeeScalar` and `operatorFeeConstant` from bytes 164-175
- MUST extract `daFootprintGasScalar` from bytes 176-177
- MUST store all three values (`daFootprintGasScalar`, `operatorFeeScalar`, `operatorFeeConstant`) in the
  `operatorFeeConstant` storage slot with proper bit packing
