# L1BlockNumber

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
- [Assumptions](#assumptions)
- [Invariants](#invariants)
- [Function Specification](#function-specification)
  - [getL1BlockNumber](#getl1blocknumber)
  - [receive](#receive)
  - [fallback](#fallback)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

Provides backwards-compatible access to the L1 block number for legacy applications. This contract is deprecated
and exists solely for compatibility with the pre-Bedrock Optimism system.

## Definitions

N/A

## Assumptions

N/A

## Invariants

N/A

## Function Specification

### getL1BlockNumber

Returns the latest L1 block number by querying the L1Block predeploy contract.

**Parameters:**
None

**Behavior:**
- MUST call `IL1Block(Predeploys.L1_BLOCK_ATTRIBUTES).number()` to retrieve the L1 block number
- MUST return the L1 block number as a uint256

### receive

Returns the L1 block number when ETH is sent to the contract without calldata.

**Parameters:**
None (payable function)

**Behavior:**
- MUST retrieve the L1 block number via `getL1BlockNumber()`
- MUST store the result in memory at position 0
- MUST return 32 bytes containing the L1 block number

### fallback

Returns the L1 block number for any function call that does not match an existing function signature.

**Parameters:**
None (payable function)

**Behavior:**
- MUST retrieve the L1 block number via `getL1BlockNumber()`
- MUST store the result in memory at position 0
- MUST return 32 bytes containing the L1 block number
