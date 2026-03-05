# LivenessModule

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Liveness Interval](#liveness-interval)
  - [Minimum Owners](#minimum-owners)
  - [Threshold Percentage](#threshold-percentage)
  - [Fallback Owner](#fallback-owner)
  - [Shutdown](#shutdown)
- [Assumptions](#assumptions)
  - [a01-001: LivenessGuard provides accurate liveness data](#a01-001-livenessguard-provides-accurate-liveness-data)
    - [Mitigations](#mitigations)
  - [a02-002: Fallback owner is trusted](#a02-002-fallback-owner-is-trusted)
    - [Mitigations](#mitigations-1)
- [Invariants](#invariants)
  - [i01-001: Threshold is maintained](#i01-001-threshold-is-maintained)
    - [Impact](#impact)
  - [i02-002: Live owners are not removed](#i02-002-live-owners-are-not-removed)
    - [Impact](#impact-1)
  - [i03-003: Non-live owners are correctly removed](#i03-003-non-live-owners-are-correctly-removed)
    - [Impact](#impact-2)
  - [i04-004: Fallback is properly triggered when owner count is unsafe](#i04-004-fallback-is-properly-triggered-when-owner-count-is-unsafe)
    - [Impact](#impact-3)
- [Function Specification](#function-specification)
  - [constructor](#constructor)
  - [safe](#safe)
  - [livenessGuard](#livenessguard)
  - [livenessInterval](#livenessinterval)
  - [minOwners](#minowners)
  - [thresholdPercentage](#thresholdpercentage)
  - [fallbackOwner](#fallbackowner)
  - [getRequiredThreshold](#getrequiredthreshold)
  - [canRemove](#canremove)
  - [removeOwners](#removeowners)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The LivenessModule enables permissionless removal of inactive Safe owners based on liveness tracking data from the
LivenessGuard. When the owner count falls below the minimum threshold, the module transfers sole ownership to a
designated [Fallback Owner](#fallback-owner) and deactivates itself.

## Definitions

### Liveness Interval

The time period (in seconds) during which an owner must demonstrate activity to avoid being considered inactive. An
owner is eligible for removal if their last recorded activity timestamp plus the [Liveness Interval] is less than the
current block timestamp.

### Minimum Owners

The minimum number of owners that must be maintained in the Safe before triggering [Shutdown]. If owner removals would
reduce the count below this threshold, all remaining owners must be removed and ownership transferred to the [Fallback
Owner].

### Threshold Percentage

The percentage (1-100) used to calculate the Safe's signing threshold based on the current number of owners. The
threshold is computed as the smallest integer greater than or equal to `(numOwners * ThresholdPercentage) / 100`.

### Fallback Owner

The address that receives sole ownership of the Safe during [Shutdown]. This address is expected to be controlled by
governance and serves as the recovery mechanism when the owner set becomes too small.

### Shutdown

The process triggered when owner removals would reduce the owner count below [Minimum Owners]. During shutdown, all
remaining owners are removed, the [Fallback Owner] becomes the sole owner with a threshold of 1, and the module
permanently deactivates itself by setting `ownershipTransferredToFallback` to true.

## Assumptions

### a01-001: LivenessGuard provides accurate liveness data

The LivenessGuard contract correctly tracks and reports the last activity timestamp for each Safe owner via its
`lastLive` mapping. Inaccurate liveness data would cause the module to incorrectly identify owners as inactive or
active.

#### Mitigations

- The LivenessGuard specification defines invariants ensuring liveness tracking accuracy
- The module verifies the LivenessGuard has not been changed during the removal process
- The LivenessGuard address is immutable in the module, preventing substitution

### a02-002: Fallback owner is trusted

The [Fallback Owner] address is trusted to act in the best interest of the Safe's stakeholders. A malicious fallback
owner could gain complete control of the Safe during [Shutdown].

#### Mitigations

- The [Fallback Owner](#fallback-owner) is set at deployment and cannot be changed without deploying a new module
- [Shutdown](#shutdown) only occurs when the owner count falls below [Minimum Owners], which requires multiple owner
  removals
- The Safe's existing owners can remove the module before [Shutdown](#shutdown) occurs if they maintain sufficient
  liveness

## Invariants

### i01-001: Threshold is maintained

The Safe's signing threshold is correctly maintained throughout all owner removal operations, ensuring the Safe
remains operational and secure.

#### Impact

**Severity: Critical**

If the threshold is incorrectly maintained, the Safe could become unusable (threshold too high for remaining owners) or
insecure (threshold too low for the number of owners). This could result in loss of access to Safe assets or
unauthorized transaction execution.

### i02-002: Live owners are not removed

Owners who have demonstrated liveness within the [Liveness Interval] cannot be removed through the `removeOwners`
function, except during [Shutdown] when the owner count has already fallen below [Minimum Owners]. This protects active
participants from being incorrectly removed.

#### Impact

**Severity: Critical**

If live owners could be removed, active participants could lose access to the Safe despite maintaining their
responsibilities. This could lead to unauthorized changes to the Safe's owner set and potential loss of control over
Safe assets.

### i03-003: Non-live owners are correctly removed

Owners who have not demonstrated liveness within the [Liveness Interval] can be successfully removed through the
`removeOwners` function when the owner count is at or above [Minimum Owners]. This ensures the liveness enforcement
mechanism functions as intended.

#### Impact

**Severity: High**

If non-live owners cannot be removed, the liveness enforcement mechanism fails its core purpose. Inactive owners would
accumulate over time, potentially leading to a Safe where the threshold cannot be met because too many owners have lost
access to their keys.

### i04-004: Fallback is properly triggered when owner count is unsafe

When owner removals reduce the count below [Minimum Owners], [Shutdown] is triggered: all remaining owners are removed,
the [Fallback Owner] becomes the sole owner with threshold 1, and the module deactivates itself. This ensures the Safe
never remains in an unsafe configuration with too few owners.

#### Impact

**Severity: Critical**

If [Shutdown](#shutdown) fails to trigger properly, the Safe could be left with too few owners to meet the threshold,
causing
permanent loss of access to Safe assets. Alternatively, if [Shutdown](#shutdown) triggers incorrectly, legitimate owners
could lose
control to the [Fallback Owner](#fallback-owner) prematurely.

## Function Specification

### constructor

Initializes the LivenessModule with configuration parameters and validates the initial Safe state.

**Parameters:**

- `_safe`: The Safe contract instance this module will manage
- `_livenessGuard`: The LivenessGuard contract instance providing liveness data
- `_livenessInterval`: The [Liveness Interval] in seconds
- `_minOwners`: The [Minimum Owners] threshold
- `_thresholdPercentage`: The [Threshold Percentage] (1-100)
- `_fallbackOwner`: The [Fallback Owner] address

**Behavior:**

- MUST set all immutable parameters to the provided values
- MUST revert if `_minOwners` exceeds the current number of Safe owners
- MUST revert if `_thresholdPercentage` is zero
- MUST revert if `_thresholdPercentage` exceeds 100

### safe

Returns the Safe contract instance managed by this module.

**Behavior:**

- MUST return the immutable `SAFE` reference

### livenessGuard

Returns the LivenessGuard contract instance used for liveness data.

**Behavior:**

- MUST return the immutable `LIVENESS_GUARD` reference

### livenessInterval

Returns the [Liveness Interval] in seconds.

**Behavior:**

- MUST return the immutable `LIVENESS_INTERVAL` value

### minOwners

Returns the [Minimum Owners] threshold.

**Behavior:**

- MUST return the immutable `MIN_OWNERS` value

### thresholdPercentage

Returns the [Threshold Percentage].

**Behavior:**

- MUST return the immutable `THRESHOLD_PERCENTAGE` value

### fallbackOwner

Returns the [Fallback Owner] address.

**Behavior:**

- MUST return the immutable `FALLBACK_OWNER` address

### getRequiredThreshold

Calculates the required Safe threshold for a given number of owners based on the [Threshold Percentage].

**Parameters:**

- `_numOwners`: The number of owners to calculate the threshold for

**Behavior:**

- MUST return `(_numOwners * THRESHOLD_PERCENTAGE + 99) / 100`
- MUST return 1 when `_numOwners` is 1

### canRemove

Checks whether a specific owner is eligible for removal based on liveness data.

**Parameters:**

- `_owner`: The owner address to check

**Behavior:**

- MUST revert if `_owner` is not a current owner of the Safe
- MUST return true if `LIVENESS_GUARD.lastLive(_owner) + LIVENESS_INTERVAL < block.timestamp`
- MUST return false otherwise

### removeOwners

Removes a set of inactive owners from the Safe, updating the threshold after each removal. If the owner count falls
below [Minimum Owners], triggers [Shutdown].

**Parameters:**

- `_previousOwners`: Array of previous owners in the Safe's linked list (one per owner to remove)
- `_ownersToRemove`: Array of owner addresses to remove

**Behavior:**

- MUST revert if the arrays have different lengths
- MUST revert if `ownershipTransferredToFallback` is true
- MUST process each owner removal in array order:
  - If current owner count is at least [Minimum Owners], MUST revert if `canRemove(_ownersToRemove[i])` returns false
  - If current owner count is below [Minimum Owners], MUST remove the owner regardless of liveness
  - MUST call `OwnerManager.removeOwner` with the new threshold via `execTransactionFromModuleReturnData`
  - MUST emit `RemovedOwner` event for each removed owner
- MUST trigger [Shutdown] when removing the last non-[Fallback Owner](#fallback-owner):
  - MUST call `OwnerManager.swapOwner` to replace the last owner with [Fallback Owner]
  - MUST set `ownershipTransferredToFallback` to true
  - MUST emit `OwnershipTransferredToFallback` event
- MUST verify the final Safe state after all removals:
  - If one owner remains, MUST be the [Fallback Owner]
  - If multiple owners remain, MUST be at least [Minimum Owners]
  - MUST have threshold equal to `getRequiredThreshold(numOwners)`
  - MUST have the same LivenessGuard address in the guard storage slot
- MUST revert with `OwnerRemovalFailed` if any Safe call fails
