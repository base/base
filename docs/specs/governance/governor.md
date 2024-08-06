# Governor

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Constants](#constants)
- [Interface](#interface)
  - [Core Functions](#core-functions)
    - [`cancel`](#cancel)
    - [`cancelWithModule`](#cancelwithmodule)
    - [`castVote`](#castvote)
    - [`castVoteBySig`](#castvotebysig)
    - [`castVoteWithReason`](#castvotewithreason)
    - [`castVoteWithReasonAndParams`](#castvotewithreasonandparams)
    - [`castVoteWithReasonAndParamsBySug`](#castvotewithreasonandparamsbysug)
    - [`editProposalType`](#editproposaltype)
    - [`execute`](#execute)
    - [`executeWithModule`](#executewithmodule)
    - [`propose`](#propose)
    - [`proposeWithModule`](#proposewithmodule)
    - [`relay`](#relay)
    - [`setModuleApproval`](#setmoduleapproval)
    - [`setProposalDeadline`](#setproposaldeadline)
    - [`setProposalThreshold`](#setproposalthreshold)
    - [`setVotingDelay`](#setvotingdelay)
    - [`setVotingPeriod`](#setvotingperiod)
    - [`updateQuorumNumerator`](#updatequorumnumerator)
  - [Getters](#getters)
    - [`approvedModules`](#approvedmodules)
    - [`getProposalType`](#getproposaltype)
    - [`getVotes`](#getvotes)
    - [`getVotesWithParams`](#getvoteswithparams)
    - [`hasVoted`](#hasvoted)
    - [`hashProposal`](#hashproposal)
    - [`hashProposalWithModule`](#hashproposalwithmodule)
    - [`manager`](#manager)
    - [`name`](#name)
    - [`proposalDeadline`](#proposaldeadline)
    - [`proposalSnapshot`](#proposalsnapshot)
    - [`proposalThreshold`](#proposalthreshold)
    - [`proposalVotes`](#proposalvotes)
    - [`quorum`](#quorum)
    - [`quorumDenominator`](#quorumdenominator)
    - [`quorumNumerator`](#quorumnumerator)
    - [`quorumNumerator` (block)](#quorumnumerator-block)
    - [`state`](#state)
    - [`supportsInterface`](#supportsinterface)
    - [`token`](#token)
    - [`version`](#version)
    - [`votableSupply`](#votablesupply)
    - [`votableSupply` (block)](#votablesupply-block)
    - [`votingDelay`](#votingdelay)
    - [`votingPeriod`](#votingperiod)
    - [`weightCast`](#weightcast)
  - [Events](#events)
- [Storage](#storage)
- [Types](#types)
- [User Flow](#user-flow)
- [Security Considerations](#security-considerations)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The `Governor` contract implements the core governance logic for creating, voting, queuing and executing proposals.
This contract uses the `GovernanceToken` contract for voting power snapshots.

## Constants

| Constant          | Value                           | Description |
| ----------------- | ------------------------------- | ----------- |
| `BALLOT_TYPEHASH` |  `0x150214d74d59b7d1e90c73fc22ef3d991dd0a76b046543d4d80ab92d2a50328f` | The EIP-712 typehash for the ballot struct  |
| `COUNTING_MODE` | `support=bravo&quorum=against,for,abstain&params=modules` | The configuration for supported values for `castVote` and how votes are counted |
| `EXTENDED_BALLOT_TYPEHASH` |  `0x7f08F3095530B67CdF8466B7a923607944136Df0` | The EIP-712 typehash for the extended ballot struct  |
| `PERCENT_DIVISOR` |  `10000` | The maximum value of `quorum` and `approvalThreshold` for proposal types  |
| `PROPOSAL_TYPES_CONFIGURATOR` |  `0x67ecA7B65Baf0342CE7fBf0AA15921524414C09f` | The address of the proposal types configurator contract  |
| `VERSION` |  `2` | The version of the `Governor` contract  |
| `VOTABLE_SUPPLY_ORACLE` |  `0x1b7CA7437748375302bAA8954A2447fC3FBE44CC` | The address of the `GovernanceToken` contract  |

## Interface

### Core Functions

#### `cancel`

Cancels a proposal. A proposal MUST only be cancellable by the manager and ONLY while it's in an active state,
i.e. not executed or cancelled already.

```solidity
function cancel(address[] memory _targets, uint256[] memory _values, bytes[] memory _calldatas, bytes32 _descriptionHash) external returns (uint256)
```

This function MUST emit the `ProposalCanceled` event and return the ID of the proposal that was cancelled.

#### `cancelWithModule`

Similar to `cancel`, but for proposals that were created with a module.

```solidity
function cancelWithModule(address _module, bytes memory _proposalData, bytes32 _descriptionHash) external returns (uint256)
```

This function MUST emit the `ProposalCanceled` event and return the ID of the proposal that was cancelled.

#### `castVote`

Cast a vote on a proposal.

```solidity
function castVote(uint256 _proposalId, uint8 _support) external returns (uint256)
```

This function MUST emit the `VoteCast` event and return the voting weight.

#### `castVoteBySig`

Cast a vote on a proposal using a signature.

```solidity
function castVoteBySig(uint256 _proposalId, uint8 _support, uint8 _v, bytes32 _r, bytes32 _s) external returns (uint256)
```

This function MUST emit the `VoteCast` event and return the voting weight.

#### `castVoteWithReason`

Cast a vote on a proposal with a reason.

```solidity
function castVoteWithReason(uint256 _proposalId, uint8 _support, string memory _reason) external returns (uint256)
```

This function MUST emit the `VoteCast` event and return the voting weight.

#### `castVoteWithReasonAndParams`

Cast a vote on a proposal with a reason and additional encoded parameters.

```solidity
function castVoteWithReasonAndParams(uint256 _proposalId, uint8 _support, string memory _reason, bytes memory _params) external returns (uint256)
```

This function MUST emit the `VoteCastWithParams` event if the length of the parameters is more than zero. Otherwise, the function
MUST emit the `VoteCast` event. The function MUST also return the voting weight.

#### `castVoteWithReasonAndParamsBySig`

Cast a vote on a proposal with a reason and additional encoded parameters using a signature.

```solidity
function castVoteWithReasonAndParams(uint256 _proposalId, uint8 _support, string memory _reason, bytes memory _params, uint8 _v, bytes32 _r, bytes32 _s) external returns (uint256)
```

This function MUST emit the `VoteCastWithParams` event if the length of the parameters is more than zero. Otherwise,
the function MUST emit the `VoteCast` event. The function MUST also return the voting weight.

#### `editProposalType`

Edit the type of a proposal that is still active. This function MUST only be callable by the manager.

```solidity
function editProposalType(uint256 _proposalId, uint8 _proposalType) external
```

This function MUST revert if the proposal type is not already configured. Additionally, this function MUST emit
the `ProposalTypeUpdated` event.

#### `execute`

Execute a successful proposal. This MUST only be possible when the quorum is reached, the vote is successful, and the
deadline is reached.

```solidity
function execute(address[] memory _targets, uint256[] memory _values, bytes[] memory _calldatas, bytes32 _descriptionHash) external payable returns (uint256)
```

This function MUST emit TODO event and return TODO.

#### `executeWithModule`

Execute a successful proposal that was created with a module. This MUST only be possible when the quorum is reached,
the vote is successful, and the deadline is reached.

```solidity
function executeWithModule(address _module, bytes memory _proposalData, bytes32 _descriptionHash) external payable returns (uint256)
```

This function MUST emit TODO event and return TODO.

#### `propose`

Creates a new proposal with a given proposal type. This function MUST check that the proposal type exists.

```solidity
function propose(address[] memory _targets, uint256[] memory _values, bytes[] memory _calldatas, string memory _description) external returns (uint256)
```

This function MUST emit the TODO event and return the ID of the proposal created.

#### `proposeWithModule`

Creates a new proposal with a given proposal type and module. This function MUST check that the proposal type exists,
and that the module is supported by the `Governor` contract.

```solidity
function proposeWithModule(address _module, bytes memory _proposalData, bytes32 _descriptionHash, uint8 _proposalType) external returns (uint256)
```

This function MUST emit the TODO event and return the ID of the proposal created.

#### `relay`

Relays a transaction or function call to an arbitrary target. This function MUST only be callable as part of a
governance proposal.

```solidity
function relay(address _target, uint256 _value, bytes calldata _data) external payable
```

#### `setModuleApproval`

Approve or reject a module. This function MUST only be callable by the manager.

```solidity
function setModuleApproval(address _module, bool _approved) external
```

#### `setProposalDeadline`

Update the deadline timestamp of an active proposal. This function MUST only be callable by the manager.

```solidity
function setProposalDeadline(uint256 _proposalId, uint64 _deadline) external
```

This function MUST emit the `ProposalDeadlineUpdated` event.

#### `setProposalThreshold`

Updates the proposal treshold. This function MUST only be callable by the manager.

```solidity
function setProposalThreshold(uint256 _newProposalThreshold) external
```

This function MUST emit the `ProposalThresholdSet` event.

#### `setVotingDelay`

Updates the voting delay. This function MUST only be callable by the manager.

```solidity
function setVotingDelay(uint256 _newVotingDelay) external
```

This function MUST emit the `VotingDelaySet` event.

#### `setVotingPeriod`

Updates the voting period. This function MUST only be callable by the manager.

```solidity
function setVotingPeriod(uint256 _newVotingDelay) external
```

This function MUST check that `_newVotingDelay` is more than zero, and emit the `VotingPeriodSet` event.

#### `updateQuorumNumerator`

Updates the quorum numerator. This function MUST only be callable by the manager.

```solidity
function updateQuorumNumerator(uint256 _newQuorumNumerator) external
```

This function MUST check that the quorum numerator does not exceed `_newQuorumNumerator`,
and emit the `QuorumNumeratorUpdated` event.

### Getters

#### `approvedModules`

Retrieves the status if a module was approved to be used in proposals.

```solidity
function approvedModules(address _module) external view returns (bool)
```

#### `getProposalType`

Returns the type of a proposal.

```solidity
function getProposalType(uint256 _proposalId) external view returns (uint8);
```

#### `getVotes`

Returns the current amount of votes that `_account` has, which should be based on the
`GovernanceToken` contract.

```solidity
function getVotes(uint256 _account) external view returns (uint256);
```

#### `getVotesWithParams`

Returns the voting power of an account at a specific timepoint given additional encoded parameters.

```solidity
function getVotesWithParams(address _account, uint256 _timepoint, bytes _params) external view returns (uint256);
```

#### `hasVoted`

Returns whether `_account` has cast a vote on `_proposalId`.

```solidity
function hasVoted(uint256 _proposalId, address _account) external view returns (bool);
```

#### `hashProposal`

Hashing function used to build a proposal id from proposal details.

```solidity
function hashProposal(address[] _targets, uint256[] _values, bytes[] _calldatas, bytes32 _descriptionHash) external view returns (uint256);
```

#### `hashProposalWithModule`

Hashing function used to build a proposal id from proposal details and module address.

```solidity
function hashProposal(address _module, bytes _proposalData, bytes32 _descriptionHash) external view returns (uint256);
```

#### `manager`

Returns the manager address of the `Governor` contract.

```solidity
function manager() external view returns (address);
```

#### `name`

Returns the name of the `Governor` contract.

```solidity
function name() external view returns (string memory);
```

#### `proposalDeadline`

Returns the timestamp for when voting on a proposal ends.

```solidity
function proposalDeadline(uint256 _proposalId) external view returns (uint256);
```

#### `proposalSnapshot`

Returns the timestamp when the proposal snapshot was taken.

```solidity
function proposalSnapshot(uint256 _proposalId) external view returns (uint256);
```

#### `proposalThreshold`

Returns the number of votes required in order for a voter to become a proposer.

```solidity
function proposalThreshold() external view returns (uint256);
```

#### `proposalVotes`

Returns the distribution of votes for a proposal, ordered in `against`, `for`, and `abstain`.

```solidity
function proposalVotes(uint256 _proposalId) external view returns (uint256, uint256, uint256);
```

#### `quorum`

Returns the quorum for a proposal, in terms of number of votes: `supply * numerator / denominator`.

```solidity
function quorum(uint256 _proposalId) external view returns (uint256);
```

#### `quorumDenominator`

Returns the denominator for the quorum calculation.

```solidity
function quorumDenominator() external view returns (uint256);
```

#### `quorumNumerator`

Returns the quorum numerator at a specific block number.

```solidity
function quorumNumerator(uint256 _blockNumber) external view returns (uint256);
```

#### `quorumNumerator` (block)

Returns the quorum numerator at the current block number.

```solidity
function quorumNumerator() external view returns (uint256);
```

#### `state`

Returns the state of a proposal.

```solidity
function state(uint256 _proposalId) external view returns (uint8);
```

#### `supportsInterface`

Returns whether if the contract implements the interface defined by `_interfaceId`.

```solidity
function supportsInterface(bytes4 _interfaceId) external view returns (bool);
```

#### `token`

Returns the address of the `GovernanceToken` contract.

```solidity
function token() external view returns (address);
```

#### `version`

Returns the version of the `Governor` contract.

```solidity
function version() external view returns (string memory);
```

#### `votableSupply`

Returns the votable supply for the current block number.

```solidity
function votableSupply() external view returns (uint256);
```

#### `votableSupply` (block)

Returns the votable supply for `_blockNumber`.

```solidity
function votableSupply(uint256 _blockNumber) external view returns (uint256);
```

#### `votingDelay`

Returns the delay since a proposal is submitted until voting power is fixed and voting starts.

```solidity
function votingDelay() external view returns (uint256);
```

#### `votingPeriod`

Returns the delay since a proposal starts until voting ends.

```solidity
function votingPeriod() external view returns (uint256);
```

#### `weightCast`

Returns the total number of votes that `_account` has cast for `_proposalId`.

```solidity
function weightCast(uint256 _proposalId, uint256 _account) external view returns (uint256);
```

### Events

TODO

## Storage

TOOD

## Types

The `Governor` contract MUST define the following types:

TODO

## User Flow

TODO

## Security Considerations

TODO
