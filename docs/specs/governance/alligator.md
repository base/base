# Alligator

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
  - [Integration with `GovernanceToken`](#integration-with-governancetoken)
  - [Getters](#getters)
  - [Delegation](#delegation)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The `Alligator` contract implements advanced delegation features for the `GovernanceToken` contract.

### Integration with `GovernanceToken`

The `Alligator` contract integrates with the `GovernanceToken` contract through a hook-based approach. The
`GovernanceToken` contract calls the `Alligator` contract's `afterTokenTransfer` function after a token transfer. This
allows the `Alligator` contract to consume the hooks and update its delegation and checkpoint mappings accordingly.
If either of the addresses passed as arguments to `afterTokenTransfer`, `from` and `to`, have not been migrated,
`Alligator` copies the address's delegation and checkpoint data from the `GovernanceToken` contract to its own state.

### Getters

`Alligator` implements the getters `delegates`, `checkpoints`, and `numCheckpoints`, which receive as argument an
account.  If this account has already been migrated, the contract reads the account's data from its state. Otherwise,
it reads the data from the `GovernanceToken` contract.

### Delegation

The `Alligator` contract implements the delegation logic for the `GovernanceToken` contract, specifically the function
`delegate` and `delegateBySig`.
