# Governance Token

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Composition](#composition)
  - [Inherited contracts](#inherited-contracts)
    - [Overrides](#overrides)
  - [Constructor](#constructor)
  - [New functions](#new-functions)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

| Constant          | Value                                        |
|-------------------|----------------------------------------------|
| Address           | `0x4200000000000000000000000000000000000042` |

The `GovernanceToken` is a contract on L2 for the OP token, which is used to govern the OP-Stack protocol.

## Composition

### Inherited contracts

`GovernanceToken` inherits from `ERC20Burnable`, `ERC20Votes`, and `Ownable`.

#### Overrides

The contract overrides the following functions in order to resolve collisions between `ERC20` and `ERC20Votes`:

- `_afterTokenTransfer`: point to `ERC20Votes._afterTokenTransfer`
- `_mint`: point to `ERC20Votes._mint`
- `_burn`: point to `ERC20Votes._burn`

### Constructor

The constructor of `GovernanceToken` is as follows:

```solidity
    constructor() ERC20("Optimism", "OP") ERC20Permit("Optimism") {}
```

### New functions

The contract introduces the following functions:

```solidity
    function mint(address _account, uint256 _amount) public onlyOwner {
        _mint(_account, _amount);
    }
```

which allows the owner to mint tokens.
