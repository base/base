# SuperchainConfig

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Constants](#constants)
- [Interface](#interface)
- [Initialization](#initialization)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The `SuperchainConfig` contract is updated with a new role that has the ability
to issue deposit transactions from the identity of the `DEPOSITOR_ACCOUNT`
that call the L2 `ProxyAdmin`.

## Constants

| Name | Value | Definition |
| --------- | ------------------------- | -- |
| `UPGRADER_SLOT` | `bytes32(uint256(keccak256("superchainConfig.upgrader")) - 1)` | Account that can call the L2 `ProxyAdmin` |

## Interface

```solidity
function upgrader() public view returns (address)
```

## Initialization

The `upgrader` can only be set during initialization.
