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
    - [`castVoteWithReasonAndParamsBySig`](#castvotewithreasonandparamsbysig)
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
    - [`ProposalCanceled`](#proposalcanceled)
    - [`VoteCast`](#votecast)
    - [`VoteCastWithParams`](#votecastwithparams)
    - [`ProposalTypeUpdated`](#proposaltypeupdated)
    - [`ProposalExecuted`](#proposalexecuted)
    - [`ProposalCreated`](#proposalcreated)
    - [`ProposalDeadlineUpdated`](#proposaldeadlineupdated)
    - [`ProposalThresholdSet`](#proposalthresholdset)
    - [`VotingDelaySet`](#votingdelayset)
    - [`VotingPeriodSet`](#votingperiodset)
    - [`QuorumNumeratorUpdated`](#quorumnumeratorupdated)
- [Storage](#storage)
- [Types](#types)
  - [`ProposalVote`](#proposalvote)
  - [`ProposalCore`](#proposalcore)
- [Proposal Types](#proposal-types)
- [Modules](#modules)
- [Security Considerations](#security-considerations)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The `Governor` contract implements the core governance logic for creating, voting, queuing and executing proposals.
This contract uses the `GovernanceToken` contract for voting power snapshots.

## Constants

| Constant          | Value                           | Description |
| ----------------- | ------------------------------- | ----------- |
| `BALLOT_TYPEHASH` |  `0x150214d74d59b7d1e90c73fc22ef3d991dd0a76b046543d4d80ab92d2a50328f` | The EIP-712 typehash for the ballot struct  |
| `EXTENDED_BALLOT_TYPEHASH` |  `0x7f08F3095530B67CdF8466B7a923607944136Df0` | The EIP-712 typehash for the extended ballot struct  
| `COUNTING_MODE` | `support=bravo&quorum=against,for,abstain&params=modules` | The configuration for supported values for `castVote` and how votes are counted |
| `PERCENT_DIVISOR` |  `10000` | The maximum value of `quorum` and `approvalThreshold` for proposal types  |
| `PROPOSAL_TYPES_CONFIGURATOR` |  `0x67ecA7B65Baf0342CE7fBf0AA15921524414C09f` | The address of the proposal types configurator contract  |
| `VERSION` |  `2` | The version of the `Governor` contract  |
| `TOKEN` |  `0x4200000000000000000000000000000000000042` | The address of the `GovernanceToken` contract  |

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

This function MUST emit the `VoteCastWithParams` event if the length of the parameters is more than zero. Otherwise,
the function MUST emit the `VoteCast` event. The function MUST also return the voting weight.

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

This function MUST emit the `ProposalExecuted` event and return the proposal ID.

#### `executeWithModule`

Execute a successful proposal that was created with a module. This MUST only be possible when the quorum is reached,
the vote is successful, and the deadline is reached.

```solidity
function executeWithModule(address _module, bytes memory _proposalData, bytes32 _descriptionHash) external payable returns (uint256)
```

This function MUST emit the `ProposalExecuted` event and return the proposal ID.

#### `propose`

Creates a new proposal with a given proposal type. This function MUST check that the proposal type exists.

```solidity
function propose(address[] memory _targets, uint256[] memory _values, bytes[] memory _calldatas, string memory _description) external returns (uint256)
```

This function MUST emit the `ProposalCreated` event and return the ID of the proposal created.

#### `proposeWithModule`

Creates a new proposal with a given proposal type and module. This function MUST check that the proposal type exists,
and that the module is supported by the `Governor` contract.

```solidity
function proposeWithModule(address _module, bytes memory _proposalData, bytes32 _descriptionHash, uint8 _proposalType) external returns (uint256)
```

This function MUST emit the `ProposalCreated` event and return the ID of the proposal created.

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

Updates the proposal threshold. This function MUST only be callable by the manager.

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
function setVotingPeriod(uint256 _newVotingPeriod) external
```

This function MUST check that `_newVotingPeriod` is more than zero, and emit the `VotingPeriodSet` event.

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
function hashProposalWithModule(address _module, bytes _proposalData, bytes32 _descriptionHash) external view returns (uint256);
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

#### `ProposalCanceled`

MUST trigger when a proposal is cancelled.

```solidity
event ProposalCanceled(uint256 proposalId);
```

#### `VoteCast`

MUST trigger when proposal vote is casted.

```solidity
event VoteCast(address indexed voter, uint256 proposalId, uint8 support, uint256 weight, string reason);
```

#### `VoteCastWithParams`

MUST trigger when proposal vote with parameters is casted.

```solidity
event VoteCastWithParams(address indexed voter, uint256 proposalId, uint8 support, uint256 weight, string reason, bytes params); 
```

#### `ProposalTypeUpdated`

MUST trigger when a proposal type is updated.

```solidity
event ProposalTypeUpdated(uint256 indexed proposalId, uint8 proposalType);
```

#### `ProposalExecuted`

MUST trigger when a proposal is executed.

```solidity
event ProposalExecuted(uint256 proposalId);
```

#### `ProposalCreated`

MUST trigger when a proposal is created.

```solidity
event ProposalCreated(uint256 indexed proposalId, address indexed proposer, address indexed module, address[] targets, uint256[] values, string[] signatures, bytes[] calldatas, uint256 startBlock, uint256 endBlock, string description, uint8 proposalType);
```

#### `ProposalDeadlineUpdated`

MUST trigger when a proposal's deadline is updated.

```solidity
event ProposalDeadlineUpdated(uint256 proposalId, uint64 deadline);
```

#### `ProposalThresholdSet`

MUST trigger when the proposal threshold is updated.

```solidity
event ProposalThresholdSet(uint256 oldProposalThreshold, uint256 newProposalThreshold);
```

#### `VotingDelaySet`

MUST trigger when the voting delay is updated.

```solidity
event VotingDelaySet(uint256 oldVotingDelay, uint256 newVotingDelay);
```

#### `VotingPeriodSet`

MUST trigger when the voting period is updated.

```solidity
event VotingPeriodSet(uint256 oldVotingPeriod, uint256 newVotingPeriod);
```

#### `QuorumNumeratorUpdated`

MUST trigger when the quorum numerator is updated.

```solidity
event QuorumNumeratorUpdated(uint256 oldQuorumNumerator, uint256 newQuorumNumerator);
```

## Storage

The `Governor` contract MUST be able to store proposals and votes. The following storage variables MUST be
defined:

```solidity
// The address of the manager.
address public manager;

// The token to use for voting.
IVotesUpgradeable public token;

// Total number of `votes` that `account` has casted for `proposalId`.
mapping(uint256 proposalId => mapping(address account => uint256 votes)) public weightCast;

// Whether a `module` has been approved or not.
mapping(address module => bool approved) public approvedModules;

// The vote accounting for a proposal.
mapping(uint256 proposalId => ProposalVote votes) internal _proposalVotes;

// The proposals that have been created.
mapping(uint256 proposalId => ProposalCore proposal) internal _proposals;
```

## Types

The `Governor` contract MUST define the following types:

### `ProposalVote`

`ProposalVote` defines the vote accounting for a proposal, encapsulated in the following struct:

```solidity
struct ProposalVote {
  uint256 againstVotes;
  uint256 forVotes;
  uint256 abstainVotes;
  mapping(address => bool) hasVoted;
}
```

| Name                     | Type            | Description                                                             |
|--------------------------|-----------------|-------------------------------------------------------------------------|
| `againstVotes`          | `uint256` | Amount of votes against the proposal.                         |
| `forVotes`              | `uint256`       | Amount of votes in favor of the proposal.                |
| `abstainVotes`                 | `uint256`       | Amount of votes abstaining.   |
| `hasVoted`                 | `mapping`       | Whether an account has voted for this proposal or not.   |

### `ProposalCore`

`ProposalCore` defines the core information of a proposal, encapsulated in the following struct:

```solidity
struct ProposalCore {
  uint64 voteStart;
  uint64 voteEnd;
  bool executed;
  bool canceled;
  address votingModule;
  uint8 proposalType;
}
```

| Name                     | Type            | Description                                                             |
|--------------------------|-----------------|-------------------------------------------------------------------------|
| `voteStart`          | `uint64` | The block timestamp when voting started.                         |
| `voteEnd`              | `uint64`       | The block timestamp when voting ends.                |
| `executed`                 | `bool`       | Indicator for proposal execution.   |
| `canceled`                 | `bool`       | Indicator for proposal cancellation.   |
| `votingModule`                 | `address`       | The voting module of the proposal. Zero address if none.   |
| `proposalType`                 | `uint8`       | The type of proposal.   |

## Proposal Types

The `Governor` contract uses proposal types to define the quorum and approval threshold for a proposal to pass.
These proposal types are created and stored as part of an external contract called `PROPOSAL_TYPES_CONFIGURATOR`.
The `Governor` contract can only create or edit proposal types, and the amount of proposal types is limited to 255.

With the [`proposeWithModule`](#proposewithmodule) function, proposers can create proposals with any supported proposal
type.

## Modules

Additionally, from proposal types, the `Governor` contract allows to use modules as part of the proposal logic.
Modules are external contract that override the default proposing and voting logic of the `Governor` contract,
and can be used to implement custom voting mechanisms. The common modules are:

- `ApprovalVotingModule`: Allow voting for multiple options in a proposal and execute either the options that exceed
  a threshold or the top voted options.
- `OptimisticModule`: Combined with a optimistic proposal type, allows to pass a proposal without quorum and approval
  threshold. Utilized for signaling proposals.

With the [`setModuleApproval`](#setmoduleapproval) function, modules can be approved to be used as part of proposals.

## Security Considerations

As proposal types and modules allow for great flexibility of proposal logic, governance MUST be aware of the implications
of approving certain modules as they are external contracts that could be upgraded or modified after deployment.

As users are able to choose proposal types and modules when creating a proposal, the combination of proposal types and
modules MUST be carefully considered to avoid unexpected behaviors.

Due to the dependency on the `GovernanceToken`, all proposals are subject to the token's voting power logic.
Therefore, it MUST be ensured that the `GovernanceToken` provides accurate and up-to-date balance and delegation
checkpoints.