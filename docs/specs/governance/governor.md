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
    - [`quorumNumerator` (current)](#quorumnumerator-current)
    - [`state`](#state)
    - [`supportsInterface`](#supportsinterface)
    - [`token`](#token)
    - [`version`](#version)
    - [`votableSupply`](#votablesupply)
    - [`votableSupply` (specific block)](#votablesupply-specific-block)
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
- [Proposal Types](#proposal-types)
  - [Interface](#interface-1)
- [Modules](#modules)
  - [Interface](#interface-2)
- [Security Considerations](#security-considerations)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The `Governor` contract implements the core governance logic for creating, voting, and executing proposals.
This contract uses the `GovernanceToken` contract for voting power snapshots, and the `ProposalTypesConfigurator`
for proposal types.

## Constants

| Constant          | Value                           | Description |
| ----------------- | ------------------------------- | ----------- |
| `COUNTING_MODE` | `support=bravo&quorum=against,for,abstain&params=modules` | The configuration for supported values for `castVote` and how votes are counted, which is also consumed by UIs to show correct vote option and interpret results. |
| `PROPOSAL_TYPES_CONFIGURATOR` |  `0x67ecA7B65Baf0342CE7fBf0AA15921524414C09f` | The address of the proposal types configurator contract  |
| `VERSION` |  `2` | The version of the `Governor` contract  |
| `TOKEN` |  `0x4200000000000000000000000000000000000042` | The address of the `GovernanceToken` contract  |

## Interface

### Core Functions

#### `cancel`

Cancels a proposal. A proposal MUST only be cancellable by the manager and ONLY when it's not in a cancelled or executed
state.

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

Cast a vote on a proposal. This function MUST adhere to the configuration specified in `COUNTING_MODE`:

- `support`: MUST support governor-bravo-style voting options: `against` (0), `for` (1), and `abstain` (2).
- `quorum`: MUST be the sum of votes `against`, `for`, and `abstain`.
- `params`: MUST use a module if the proposal was created with one.

The voting weight of the voter MUST be determined by the number of votes the account had delegated to it at the time
the proposal state became active. The `_support` parameter MUST be one of the voting options, noted above.

```solidity
function castVote(uint256 _proposalId, uint8 _support) external returns (uint256)
```

This function MUST emit the `VoteCast` event and return the voting weight.

#### `castVoteBySig`

Cast a vote on a proposal using a signature. This function MUST calculate the voter address using the `v`, `r`, and `s`
parameters, and apply the same logic specified in [`castVote`](#castvote).

```solidity
function castVoteBySig(uint256 _proposalId, uint8 _support, uint8 _v, bytes32 _r, bytes32 _s) external returns (uint256)
```

This function MUST emit the `VoteCast` event and return the voting weight.

#### `castVoteWithReason`

Cast a vote on a proposal with a reason. This function MUST apply the same logic specified in [`castVote`](#castvote).

```solidity
function castVoteWithReason(uint256 _proposalId, uint8 _support, string memory _reason) external returns (uint256)
```

This function MUST emit the `VoteCast` event and return the voting weight.

#### `castVoteWithReasonAndParams`

Cast a vote on a proposal with a reason and additional encoded parameters. This function MUST apply the same logic
specified in [`castVote`](#castvote). Additionally, the function MUST call `countVote` function of the module with the
additional encoded parameters.

```solidity
function castVoteWithReasonAndParams(uint256 _proposalId, uint8 _support, string memory _reason, bytes memory _params) external returns (uint256)
```

This function MUST emit the `VoteCastWithParams` event if the length of the parameters is more than zero. Otherwise,
the function MUST emit the `VoteCast` event. The function MUST also return the voting weight.

#### `castVoteWithReasonAndParamsBySig`

Cast a vote on a proposal with a reason and additional encoded parameters using the voter address calculated from the
`v`, `r`, and `s` parameters. This function MUST apply the same logic specified in [`castVote`](#castvote).

```solidity
function castVoteWithReasonAndParams(uint256 _proposalId, uint8 _support, string memory _reason, bytes memory _params, uint8 _v, bytes32 _r, bytes32 _s) external returns (uint256)
```

This function MUST emit the `VoteCastWithParams` event if the length of the parameters is more than zero. Otherwise,
the function MUST emit the `VoteCast` event. The function MUST also return the voting weight.

#### `editProposalType`

Edit the type of a proposal that is still active. This function MUST only be callable by the manager. Additionally,
this function MUST revert if either the proposal or proposal type doesn't exist.

```solidity
function editProposalType(uint256 _proposalId, uint8 _proposalType) external
```

This function MUST emit the `ProposalTypeUpdated` event.

#### `execute`

Execute a successful proposal. This MUST only be possible when the proposal has reached quorum and is considered
successful in terms of votes.

```solidity
function execute(address[] memory _targets, uint256[] memory _values, bytes[] memory _calldatas, bytes32 _descriptionHash) external payable returns (uint256)
```

This function MUST emit the `ProposalExecuted` event and return the proposal ID.

#### `executeWithModule`

Similar to [`execute`](#execute), but for proposals that were created with a module. This MUST only be possible when the
proposal has reached quorum and is considered successful in terms of votes.

This function must call the `formatExecuteParams` function of the module passing the proposal ID and data to receive
the formatted `targets`, `values`, and `calldatas` for execution. This MUST adhere to the [module's interface](#interface-1).

```solidity
function executeWithModule(address _module, bytes memory _proposalData, bytes32 _descriptionHash) external payable returns (uint256)
```

This function MUST emit the `ProposalExecuted` event and return the proposal ID.

#### `propose`

Creates a new proposal. This function MUST check that the lengths of `targets` and `values` match, and that the length
of `targets` is greater than zero. Additionally, the sender's voting power MUST be greater or equal to the proposal threshold.
If the proposal uses a proposal type that is not configured, the function MUST revert. The function MUST also check
that the proposal does not already exist using the hash of the parameters.

The snapshot of the proposal MUST be the current block number plus the voting delay, and the deadline MUST be the snapshot
plus the voting period.

```solidity
function propose(address[] memory _targets, uint256[] memory _values, bytes[] memory _calldatas, string memory _description, uint8 _proposalType) external returns (uint256)
```

This function MUST emit the `ProposalCreated` event and return the ID of the proposal created.

#### `proposeWithModule`

Similar to [`propose`](#propose), but for proposals that were created with a module. This function MUST check that the
module is approved, and apply the same checks specified in [`propose`](#propose). This function MUST call the `propose`
function of the module, according to the [interface](#interface-1).

```solidity
function proposeWithModule(address _module, bytes memory _proposalData, bytes32 _descriptionHash, uint8 _proposalType) external returns (uint256)
```

This function MUST emit the `ProposalCreated` event and return the ID of the proposal created.

#### `relay`

Relays a transaction or function call to an arbitrary target. This function MUST only be callable as part of a
governance proposal.

In cases where the governance executor is some contract other than the `Governor` itself, like when using a timelock,
this function CAN be invoked in a governance proposal to recover tokens or Ether that was sent to the `Governor` contract
by mistake.

```solidity
function relay(address _target, uint256 _value, bytes calldata _data) external payable
```

#### `setModuleApproval`

Approve or reject a module. This function MUST only be callable by the manager.

```solidity
function setModuleApproval(address _module, bool _approved) external
```

#### `setProposalDeadline`

Updates the voting deadline of a proposal with a new block number. This function MUST only be callable by the manager.

```solidity
function setProposalDeadline(uint256 _proposalId, uint64 _deadline) external
```

This function MUST emit the `ProposalDeadlineUpdated` event.

#### `setProposalThreshold`

Updates the threshold of voting power needed to create proposals. This function MUST only be callable by the manager.

```solidity
function setProposalThreshold(uint256 _newProposalThreshold) external
```

This function MUST emit the `ProposalThresholdSet` event.

#### `setVotingDelay`

Updates the voting delay, which is the number of blocks before the voting for a proposal starts. This function MUST
only be callable by the manager.

```solidity
function setVotingDelay(uint256 _newVotingDelay) external
```

This function MUST emit the `VotingDelaySet` event.

#### `setVotingPeriod`

Updates the voting period, which is the number of blocks the voting for a proposal lasts. This function MUST only be
callable by the manager.

```solidity
function setVotingPeriod(uint256 _newVotingPeriod) external
```

This function MUST check that `_newVotingPeriod` is more than zero, and emit the `VotingPeriodSet` event.

#### `updateQuorumNumerator`

Updates the quorum numerator. This function MUST only be callable by the manager.

```solidity
function updateQuorumNumerator(uint256 _newQuorumNumerator) external
```

This function MUST check that the `_newQuorumNumerator` does not exceed the quorum denominator, and emit the
`QuorumNumeratorUpdated` event.

### Getters

#### `approvedModules`

Retrieves whether a module was approved or not to be used in proposals.

```solidity
function approvedModules(address _module) external view returns (bool)
```

#### `getProposalType`

Returns the type of a proposal.

```solidity
function getProposalType(uint256 _proposalId) external view returns (uint8);
```

#### `getVotes`

Returns the current voting power of an `_account`, which is the amount of `GovernanceToken` tokens delegated
to it.

```solidity
function getVotes(uint256 _account) external view returns (uint256);
```

#### `getVotesWithParams`

Returns the voting power of an account at a specific block nubmer given additional encoded parameters.

```solidity
function getVotesWithParams(address _account, uint256 _blockNumber) external view returns (uint256);
```

#### `hasVoted`

Returns whether an `_account` has cast a vote for a proposal with ID `_proposalId`.

```solidity
function hasVoted(uint256 _proposalId, address _account) external view returns (bool);
```

#### `hashProposal`

Hashing function used to build a proposal ID from proposal parameters.

```solidity
function hashProposal(address[] _targets, uint256[] _values, bytes[] _calldatas, bytes32 _descriptionHash) external view returns (uint256);
```

#### `hashProposalWithModule`

Hashing function used to build a proposal ID from proposal parameters and a module address.

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

Returns the block number for when voting on a proposal ends.

```solidity
function proposalDeadline(uint256 _proposalId) external view returns (uint256);
```

#### `proposalSnapshot`

Returns the block number for the snapshot of a proposal.

```solidity
function proposalSnapshot(uint256 _proposalId) external view returns (uint256);
```

#### `proposalThreshold`

Returns the minimum voting power required in order to create a proposal.

```solidity
function proposalThreshold() external view returns (uint256);
```

#### `proposalVotes`

Returns the distribution of votes for a proposal, ordered in `against`, `for`, and `abstain`.

```solidity
function proposalVotes(uint256 _proposalId) external view returns (uint256, uint256, uint256);
```

#### `quorum`

Returns the quorum of a proposal, which MUST be the sum of votes `against`, `for`, and `abstain`,
per the `quorum` configuration in `COUNTING_MODE`.

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

#### `quorumNumerator` (current)

Returns the quorum numerator at the current block number.

```solidity
function quorumNumerator() external view returns (uint256);
```

#### `state`

Returns the state of a proposal. The state MUST adhere to the following control flow:

- `Pending` (0): The proposal's start block is equal or greater than the current block number.
- `Active` (1): The proposal's end block is equal or greater the current block number.
- `Canceled` (2): The proposal has been cancelled.
- `Defeated` (3): The proposal has not reached quorum or has not been considered successful.
- `Succeeded` (4): The proposal has reached quorum and is considered successful.
- `Queued` (5): The proposal has been queued. This state SHOULD NOT be used in the current version.
- `Expired` (6): The grace period for the execution of the proposal has passed. This state SHOULD
NOT be used in the current version.
- `Executed` (7): The proposal has been executed.

This function MUST revert if the proposal does not exist.

```solidity
function state(uint256 _proposalId) external view returns (ProposalState);
```

#### `supportsInterface`

Returns whether the contract implements the interface defined by `_interfaceId`. This function MUST return `true`
for ID of the following interfaces: `ERC165`, `ERC721`, and `ERC1155`.

```solidity
function supportsInterface(bytes4 _interfaceId) external view returns (bool);
```

#### `token`

Returns the `TOKEN` constant, which MUST be the address of the `GovernanceToken` contract.

```solidity
function token() external view returns (address);
```

#### `version`

Returns the version of the `Governor` contract.

```solidity
function version() external view returns (string memory);
```

#### `votableSupply`

Returns the votable supply for the current block number. Votable supply is defined by the total amount
of `GovernanceToken` tokens that are currently delegated.

```solidity
function votableSupply() external view returns (uint256);
```

#### `votableSupply` (specific block)

Returns the votable supply for the given block number. Votable supply is defined by the total amount
of `GovernanceToken` tokens that are currently delegated.

```solidity
function votableSupply(uint256 _blockNumber) external view returns (uint256);
```

#### `votingDelay`

Returns the delay, in terms of block amounts, between proposal creation and voting start.

```solidity
function votingDelay() external view returns (uint256);
```

#### `votingPeriod`

Returns the period, in terms of block amounts, that voting for a proposal is enabled.

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

MUST trigger when a proposal is cancelled, per the [`cancel`](#cancel) and [`cancelWithModule`](#cancelwithmodule) functions.

```solidity
event ProposalCanceled(uint256 proposalId);
```

#### `VoteCast`

MUST trigger when proposal vote is cast, per the [`castVote`](#castvote), [`castVoteBySig`](#castvotebysig)
and [`castVoteWithReason`](#castvotewithreason) functions.

```solidity
event VoteCast(address indexed voter, uint256 proposalId, uint8 support, uint256 weight, string reason);
```

#### `VoteCastWithParams`

MUST trigger when proposal vote with parameters is cast, per the
[`castVoteWithReasonAndParams`](#castvotewithreasonandparams), and
[`castVoteWithReasonAndParamsBySig`](#castvotewithreasonandparamsbysig) functions.

```solidity
event VoteCastWithParams(address indexed voter, uint256 proposalId, uint8 support, uint256 weight, string reason, bytes params); 
```

#### `ProposalTypeUpdated`

MUST trigger when a proposal type is updated, per the [`editProposalType`](#editproposaltype) function.

```solidity
event ProposalTypeUpdated(uint256 indexed proposalId, uint8 proposalType);
```

#### `ProposalExecuted`

MUST trigger when a proposal is executed, per the [`execute`](#execute) and [`executeWithModule](#executewithmodule) functions.

```solidity
event ProposalExecuted(uint256 proposalId);
```

#### `ProposalCreated`

MUST trigger when a proposal is created, per the [`propose`](#propose) and [`proposeWithModule`](#proposewithmodule) functions.

```solidity
event ProposalCreated(uint256 indexed proposalId, address indexed proposer, address indexed module, address[] targets, uint256[] values, string[] signatures, bytes[] calldatas, uint256 startBlock, uint256 endBlock, string description, uint8 proposalType);
```

#### `ProposalDeadlineUpdated`

MUST trigger when the proposal voting deadline is updated, per the [`setProposalDeadline`](#setproposaldeadline) function.

```solidity
event ProposalDeadlineUpdated(uint256 proposalId, uint64 deadline);
```

#### `ProposalThresholdSet`

MUST trigger when the proposal threshold is updated, per the [`setProposalThreshold`](#setproposalthreshold).

```solidity
event ProposalThresholdSet(uint256 oldProposalThreshold, uint256 newProposalThreshold);
```

#### `VotingDelaySet`

MUST trigger when the proposal voting delay is updated, per the [`setVotingDelay`](#setvotingdelay).

```solidity
event VotingDelaySet(uint256 oldVotingDelay, uint256 newVotingDelay);
```

#### `VotingPeriodSet`

MUST trigger when the proposal voting period is updated, per the [`setVotingPeriod`](#setvotingperiod).

```solidity
event VotingPeriodSet(uint256 oldVotingPeriod, uint256 newVotingPeriod);
```

#### `QuorumNumeratorUpdated`

MUST trigger when the quorum numerator is updated, per the [`updateQuorumNumerator`](#updatequorumnumerator) function.

```solidity
event QuorumNumeratorUpdated(uint256 oldQuorumNumerator, uint256 newQuorumNumerator);
```

## Proposal Types

The `Governor` contract uses proposal types to define the quorum and approval threshold for a proposal to pass.
These proposal types are created and stored as part of an external contract called `ProposalTypesConfigurator`.
The `Governor` contract can only create or edit proposal types, and the amount of proposal types is limited to 255.

With the [`proposeWithModule`](#proposewithmodule) function, proposers can create proposals with any supported proposal
type.

### Interface

The `ProposalTypesConfigurator` contract MUST adhere to the following interface so that it can be used by the `Governor`
contract:

```solidity
interface IProposalTypesConfigurator {
  /// @notice The proposal type data structure.
  struct ProposalType {
    uint16 quorum; // The quorum percentage, denominated in basis points.
    uint16 approvalThreshold; // The approval threshold percentage, denominated in basis points.
    string name; // The name of the proposal type.
  }

  /// @notice Getter function to get the proposal type data for a given ID.
  function proposalTypes(uint256 _proposalTypeId) external view returns (ProposalType memory);

  /// @notice Creates or updates a proposal type.
  /// @param _proposalTypeId The ID of the proposal type.
  /// @param _quorum The quorum percentage.
  /// @param _approvalThreshold The approval threshold percentage.
  /// @param _name The proposal type name.
  function setProposalType(uint256 _proposalTypeId, uint16 _quorum, uint16 _approvalThreshold, string memory _name) external;
}
```

## Modules

Additionally, from proposal types, the `Governor` contract allows to use modules as part of the proposal lifecycle.
Modules are external contract that override the default proposing and voting logic of the `Governor` contract,
and can be used to implement custom voting mechanisms. The common modules are:

- `ApprovalVotingModule`: Allow voting for multiple options in a proposal and execute either the options that exceed
  a threshold or the top voted options.
- `OptimisticModule`: Combined with a optimistic proposal type, allows to pass a proposal without quorum and approval
  threshold. Utilized for signaling proposals.

With the [`setModuleApproval`](#setmoduleapproval) function, modules can be approved to be used as part of proposals.

### Interface

In order to be supported by the `Governor` contract, modules MUST implement the following interface:

```solidity
interface IGovernorModule {
    /// @notice Executes custom proposal creation logic.
    /// @param _proposalId The ID of the proposal.
    /// @param _proposalData The encoded data of the proposal.
    /// @param _descriptionHash The hash of the proposal description.
    function propose(uint256 _proposalId, bytes memory _proposalData, bytes32 _descriptionHash) external;

    /// @notice Executes custom proposal voting logic.
    /// @param _proposalId The ID of the proposal.
    /// @param _account The account that is voting.
    /// @param _support The vote option.
    /// @param _weight The voting weight.
    /// @param _params The encoded parameters.
    function countVote(uint256 _proposalId, address _account, uint8 _support, uint256 _weight, bytes memory _params) external;

    /// @notice Getter that returns the formatted targets, values, and calldatas for a proposal.
    /// @param _proposalId The ID of the proposal.
    /// @param _proposalData The encoded data of the proposal to extract the parameters from.
    function formatExecuteParams(uint256 _proposalId, bytes memory _proposalData)
        external
        view
        returns (address[] memory targets, uint256[] memory values, bytes[] memory calldatas);
      
    /// @notice Getter that returns if a proposal was successful in terms of votes.
    /// @param _proposalId The ID of the proposal.
    function voteSucceeded(uint256 proposalId) external view returns (bool);
}
```

## Security Considerations

As proposal types and modules allow for great flexibility of proposal logic, governance MUST be aware of the implications
of approving certain modules as they are external contracts that could be upgraded or modified after deployment.

As users are able to choose proposal types and modules when creating a proposal, the combination of proposal types and
modules MUST be carefully considered to avoid unexpected behaviors.

Due to the dependency on the `GovernanceToken`, all proposals are subject to the token's voting power logic.
Therefore, it MUST be ensured that the `GovernanceToken` provides accurate and up-to-date balance and delegation
checkpoints.
