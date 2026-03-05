# DeployerWhitelist

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Whitelist Disabled State](#whitelist-disabled-state)
- [Assumptions](#assumptions)
- [Invariants](#invariants)
  - [i01-001: Whitelist Irreversibility](#i01-001-whitelist-irreversibility)
    - [Impact](#impact)
- [Function Specification](#function-specification)
  - [setWhitelistedDeployer](#setwhitelisteddeployer)
  - [setOwner](#setowner)
  - [enableArbitraryContractDeployment](#enablearbitrarycontractdeployment)
  - [isDeployerAllowed](#isdeployerallowed)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

DeployerWhitelist is a legacy predeploy contract that was originally used to restrict contract deployment to
whitelisted addresses during the initial phases of Optimism. The contract has been deprecated since the Bedrock
upgrade and is no longer used by the Optimism system. It is maintained in state only for backwards compatibility.

## Definitions

### Whitelist Disabled State

The state where the whitelist is permanently disabled, indicated by the owner being set to `address(0)`. In this
state, all addresses are allowed to deploy contracts according to the contract's logic, and the whitelist cannot be
re-enabled.

## Assumptions

N/A

## Invariants

### i01-001: Whitelist Irreversibility

Once the whitelist enters the [Whitelist Disabled State](#whitelist-disabled-state) (owner is `address(0)`), it
cannot be re-enabled. The owner cannot be changed from `address(0)` to any other address.

#### Impact

**Severity: Low**

If this invariant were violated, a previously disabled whitelist could be re-enabled, potentially restricting
contract deployment after it had been made permissionless. However, since this contract is deprecated and no longer
used by the Optimism system, the practical impact is minimal.

## Function Specification

### setWhitelistedDeployer

Updates the whitelist status of a deployer address.

**Parameters:**

- `_deployer`: The address whose whitelist status is being updated
- `_isWhitelisted`: Boolean indicating whether the address should be whitelisted

**Behavior:**

- MUST revert if caller is not the owner
- MUST set `whitelist[_deployer]` to `_isWhitelisted`
- MUST emit `WhitelistStatusChanged` event with `_deployer` and `_isWhitelisted`

### setOwner

Transfers ownership of the contract to a new address.

**Parameters:**

- `_owner`: The address of the new owner

**Behavior:**

- MUST revert if caller is not the current owner
- MUST revert if `_owner` is `address(0)`
- MUST emit `OwnerChanged` event with the old owner and new owner addresses
- MUST set `owner` to `_owner`

### enableArbitraryContractDeployment

Permanently disables the whitelist by setting the owner to `address(0)`, entering the
[Whitelist Disabled State](#whitelist-disabled-state).

**Behavior:**

- MUST revert if caller is not the owner
- MUST emit `WhitelistDisabled` event with the current owner address
- MUST set `owner` to `address(0)`

### isDeployerAllowed

Checks whether an address is allowed to deploy contracts according to the whitelist logic.

**Parameters:**

- `_deployer`: The address to check

**Returns:**

- `bool`: `true` if the whitelist is in [Whitelist Disabled State](#whitelist-disabled-state) OR if `_deployer` is
  whitelisted; `false` otherwise
