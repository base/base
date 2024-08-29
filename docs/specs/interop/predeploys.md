# Predeploys

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [CrossL2Inbox](#crossl2inbox)
  - [Message execution arguments](#message-execution-arguments)
    - [`_msg`](#_msg)
    - [`_id`](#_id)
    - [`_target`](#_target)
  - [Interop Start Timestamp](#interop-start-timestamp)
  - [`ExecutingMessage` Event](#executingmessage-event)
  - [Reference implementation](#reference-implementation)
  - [Deposit Handling](#deposit-handling)
  - [`Identifier` Getters](#identifier-getters)
- [L2ToL2CrossDomainMessenger](#l2tol2crossdomainmessenger)
  - [`relayMessage` Invariants](#relaymessage-invariants)
  - [`sendExpire` Invariants](#sendexpire-invariants)
  - [`relayExpire` Invariants](#relayexpire-invariants)
  - [Message Versioning](#message-versioning)
  - [No Native Support for Cross Chain Ether Sends](#no-native-support-for-cross-chain-ether-sends)
  - [Interfaces](#interfaces)
    - [Sending Messages](#sending-messages)
    - [Relaying Messages](#relaying-messages)
    - [Sending Expired Message Hashes](#sending-expired-message-hashes)
    - [Relaying Expired Message Hashes](#relaying-expired-message-hashes)
- [OptimismSuperchainERC20Factory](#optimismsuperchainerc20factory)
  - [OptimismSuperchainERC20](#optimismsuperchainerc20)
  - [Overview](#overview)
    - [Proxy](#proxy)
    - [Beacon Pattern](#beacon-pattern)
    - [Deployment history](#deployment-history)
  - [Functions](#functions)
    - [`deploy`](#deploy)
  - [Events](#events)
    - [`OptimismSuperchainERC20Created`](#optimismsuperchainerc20created)
  - [Deployment Flow](#deployment-flow)
- [BeaconContract](#beaconcontract)
  - [Overview](#overview-1)
- [L1Block](#l1block)
  - [Static Configuration](#static-configuration)
  - [Dependency Set](#dependency-set)
  - [Deposit Context](#deposit-context)
  - [`isDeposit()`](#isdeposit)
    - [`depositsComplete()`](#depositscomplete)
- [OptimismMintableERC20Factory](#optimismmintableerc20factory)
  - [OptimismMintableERC20](#optimismmintableerc20)
  - [Updates](#updates)
  - [Functions](#functions-1)
    - [`createOptimismMintableERC20WithDecimals`](#createoptimismmintableerc20withdecimals)
    - [`createOptimismMintableERC20`](#createoptimismmintableerc20)
    - [`createStandardL2Token`](#createstandardl2token)
  - [Events](#events-1)
    - [`OptimismMintableERC20Created`](#optimismmintableerc20created)
    - [`StandardL2TokenCreated`](#standardl2tokencreated)
- [L2StandardBridge](#l2standardbridge)
  - [Updates](#updates-1)
    - [convert](#convert)
    - [`Converted`](#converted)
  - [Invariants](#invariants)
  - [Conversion Flow](#conversion-flow)
- [Security Considerations](#security-considerations)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

Four new system level predeploys are introduced for managing cross chain messaging and tokens, along with
an update to the `L1Block`, `OptimismMintableERC20Factory` and `L2StandardBridge` contracts with additional functionalities.

## CrossL2Inbox

| Constant | Value                                        |
| -------- | -------------------------------------------- |
| Address  | `0x4200000000000000000000000000000000000022` |

The `CrossL2Inbox` is responsible for executing a cross chain message on the destination chain.
It is permissionless to execute a cross chain message on behalf of any user.

To ensure safety of the protocol, the [Message Invariants](./messaging.md#messaging-invariants) must be enforced.

### Message execution arguments

The following fields are required for executing a cross chain message:

[message payload]: ./messaging.md#message-payload
[`Identifier`]: ./messaging.md#message-identifier

| Name      | Type         | Description                                             |
| --------- | ------------ | ------------------------------------------------------- |
| `_msg`    | `bytes`      | The [message payload], matching the initiating message. |
| `_id`     | `Identifier` | A [`Identifier`] pointing to the initiating message.    |
| `_target` | `address`    | Account that is called with `_msg`.                     |

#### `_msg`

The [message payload] of the executing message.

This must match the emitted payload of the initiating message identified by `_id`.

#### `_id`

A pointer to the `_msg` in a remote (or local) chain.

The message [`Identifier`] of the executing message.
This is required to enforce the message executes an existing and valid initiating message.

By including the [`Identifier`] in the calldata, it makes static analysis much easier for block builders.
It is impossible to check that the [`Identifier`] matches the cross chain message on chain. If the block
builder includes a message that does not correspond to the [`Identifier`], their block will be reorganized
by the derivation pipeline.

A possible upgrade path to this contract would involve adding a new function. If any fields in the [`Identifier`]
change, then a new 4byte selector will be generated by solc.

#### `_target`

Messages are broadcast, not directed. Upon execution the caller can specify which `address` to target:
there is no protocol enforcement on what this value is.

The `_target` is called with the `_msg` as input.
In practice, the `_target` will be a contract that needs to know the schema of the `_msg` so that it can be decoded.
It MAY call back to the `CrossL2Inbox` to authenticate
properties about the `_msg` using the information in the `Identifier`.

### Interop Start Timestamp

The Interop Start Timestamp represents the earliest timestamp which an initiating message (identifier) can have to be
considered valid. This is important because OP Mainnet migrated from a legacy system that is not provable. We cannot
allow for interop messages to come from unproven parts of the chain history, since interop is secured by fault proofs.

Interop Start Timestamp is stored in the storage of the CrossL2Inbox predeploy. During the Interop Network Upgrade,
each chain sets variable via a call to `setInteropStart()` by the `DEPOSITOR_ACCOUNT` which sets Interop Start Timestamp
to be the block.timestamp of the network upgrade block. Chains deployed after the network upgrade will have to enshrine
that timestamp into the pre-determined storage slot.

### `ExecutingMessage` Event

The `ExecutingMessage` event represents an executing message. It MUST be emitted on every call
to `executeMessage`.

```solidity
event ExecutingMessage(bytes32 indexed msgHash, Identifier identifier);
```

The data encoded in the event contains the keccak hash of the `msg` and the `Identifier`.
The following pseudocode shows the deserialization:

```solidity
bytes32 msgHash = log.topics[1];
Identifier identifier = abi.decode(log.data, (Identifier));
```

Emitting the hash of the message is more efficient than emitting the
message in its entirety. Equality with the initiating message can be handled off-chain through
hash comparison.

### Reference implementation

A simple implementation of the `executeMessage` function is included below.

```solidity
function executeMessage(Identifier calldata _id, address _target, bytes calldata _msg) public payable {
    require(_id.timestamp <= block.timestamp);
    require(L1Block.isInDependencySet(_id.chainid));
    require(_id.timestamp > interopStart());

    assembly {
      tstore(ORIGIN_SLOT, _id.origin)
      tstore(BLOCKNUMBER_SLOT, _id.blocknumber)
      tstore(LOG_INDEX_SLOT, _id.logIndex)
      tstore(TIMESTAMP_SLOT, _id.timestamp)
      tstore(CHAINID_SLOT, _id.chainid)
    }

    bool success = SafeCall.call({
      _target: _target,
      _value: msg.value,
      _calldata: _msg
    });

    require(success);

    emit ExecutingMessage(keccak256(_msg), _id);
}
```

Note that the `executeMessage` function is `payable` to enable relayers to earn in the gas paying asset.

### Deposit Handling

Any call to the `CrossL2Inbox` that would emit an `ExecutingMessage` event will reverts
if the call is made in a [deposit context](./derivation.md#deposit-context).
The deposit context status can be determined by callling `isDeposit` on the `L1Block` contract.

In the future, deposit handling will be modified to be more permissive.
It will revert only in specific cases where interop dependency resolution is not feasible.

### `Identifier` Getters

The `Identifier` MUST be exposed via `public` getters so that contracts can call back to authenticate
properties about the `_msg`.

## L2ToL2CrossDomainMessenger

| Constant          | Value                                        |
| ----------------- | -------------------------------------------- |
| Address           | `0x4200000000000000000000000000000000000023` |
| `MESSAGE_VERSION` | `uint256(0)`                                 |
| `EXPIRY_WINDOW`   | `uint256(7200)`                              |

The `L2ToL2CrossDomainMessenger` is a higher level abstraction on top of the `CrossL2Inbox` that
provides features necessary for secure transfers ERC20 tokens between L2 chains.
Messages sent through the `L2ToL2CrossDomainMessenger` on the source chain receive both replay protection
as well as domain binding, ie the executing transaction can only be valid on a single chain.

### `relayMessage` Invariants

- Only callable by the `CrossL2Inbox`
- The `Identifier.origin` MUST be `address(L2ToL2CrossDomainMessenger)`
- The `_destination` chain id MUST be equal to the local chain id
- The `CrossL2Inbox` cannot call itself

### `sendExpire` Invariants

- The message MUST have not been successfully relayed
- The `EXPIRY_WINDOW` MUST have elapsed since the message first failed to be relayed
- The expired message MUST not have been previously sent back to source
- The expired message MUST not be relayable after being sent back

### `relayExpire` Invariants

- Only callable by the `CrossL2Inbox`
- The message source MUST be `block.chainid`
- The `Identifier.origin` MUST be `address(L2ToL2CrossDomainMessenger)`
- The `expiredMessages` mapping MUST only contain messages that originated in this chain and failed to be relayed on destination.
- Already expired messages MUST NOT be relayed.

### Message Versioning

Versioning is handled in the most significant bits of the nonce, similarly to how it is handled by
the `CrossDomainMessenger`.

```solidity
function messageNonce() public view returns (uint256) {
    return Encoding.encodeVersionedNonce(nonce, MESSAGE_VERSION);
}
```

### No Native Support for Cross Chain Ether Sends

To enable interoperability between chains that use a custom gas token, there is no native support for
sending `ether` between chains. `ether` must first be wrapped into WETH before sending between chains.
See [SuperchainWETH](./superchain-weth.md) for more information.

### Interfaces

The `L2ToL2CrossDomainMessenger` uses a similar interface to the `L2CrossDomainMessenger` but
the `_minGasLimit` is removed to prevent complexity around EVM gas introspection and the `_destination`
chain is included instead.

#### Sending Messages

The following function is used for sending messages between domains:

```solidity
function sendMessage(uint256 _destination, address _target, bytes calldata _message) external;
```

It creates an initiating message that is represented by an anonymous event:

```solidity
assembly {
    log0(add(_data, 0x20), mload(_data))
}
```

The `_data` is an ABI encoded call to `relayMessage`. The event is done with Yul so that an extra layer
of ABI encoding as `bytes` is not wrapped around `relayMessage` call. The exact calldata meant to be passed
to the `L2ToL2CrossDomainMessenger` on the remote domain is included in the log.

An explicit `_destination` chain and `nonce` are used to ensure that the message can only be played on a single remote
chain a single time. The `_destination` is enforced to not be the local chain to avoid edge cases.

There is no need for address aliasing as the aliased address would need to commit to the source chain's chain id
to create a unique alias that commits to a particular sender on a particular domain and it is far more simple
to assert on both the address and the source chain's chain id rather than assert on an unaliased address.
In both cases, the source chain's chain id is required for security. Executing messages will never be able to
assume the identity of an account because `msg.sender` will never be the identity that initiated the message,
it will be the `L2ToL2CrossDomainMessenger` and users will need to callback to get the initiator of the message.

The `_destination` MUST NOT be the chainid of the local chain and a locally defined `nonce` MUST increment on
every call to `sendMessage`.

Note that `sendMessage` is not `payable`.

#### Relaying Messages

When relaying a message through the `L2ToL2CrossDomainMessenger`, it is important to require that
the `_destination` equal to `block.chainid` to ensure that the message is only valid on a single
chain. The hash of the message is used for replay protection.

It is important to ensure that the source chain is in the dependency set of the destination chain, otherwise
it is possible to send a message that is not playable.

When a message fails to be relayed, only the timestamp at which it
first failed along with its source chain id are stored. This is
needed for calculation of the failed message's expiry. The source chain id
is also required to simplify the function signature of `sendExpire`.

```solidity
function relayMessage(uint256 _destination, uint256 _source, uint256 _nonce, address _sender, address _target, bytes memory _message) external payable {
    require(msg.sender == address(CROSS_L2_INBOX));
    require(_destination == block.chainid);
    require(CROSS_L2_INBOX.origin() == address(this));

    bytes32 messageHash = keccak256(abi.encode(_destination, _source, _nonce, _sender, _target, _message));
    require(sentMessages[messageHash] == false);

    assembly {
      tstore(CROSS_DOMAIN_MESSAGE_SENDER_SLOT, _sender)
      tstore(CROSS_DOMAIN_MESSAGE_SOURCE_SLOT, _source)
    }

    sentMessages[messageHash] = true;

    bool success = SafeCall.call({
       _target: _target,
       _value: msg.value,
       _calldata: _message
    });

    if (!success) {
      emit FailedRelayedMessage(messageHash);

      if (failedMessages[messageHash].timestamp == 0) {
        failedMessages[messageHash] = FailedMessage({timestamp: block.timestamp, sourceChainId: _source});
      }

      return;
    }

    successfulMessages[messageHash] = true;
    delete failedMessages[messageHash];
    emit RelayedMessage(messageHash);
};
```

Note that the `relayMessage` function is `payable` to enable relayers to earn in the gas paying asset.

To enable cross chain authorization patterns, both the `_sender` and the `_source` MUST be exposed via `public`
getters.

#### Sending Expired Message Hashes

When expiring a message that failed to be relayed on the destination chain
to the source chain, it's crucial to ensure the message can only be sent back
to the `L2ToL2CrossDomainMessenger` contract in its source chain.

This function has no auth, which allows anyone to expire a given message hash.
The `EXPIRY_WINDOW` variable is added to give the users enough time to replay their
failed messages and to prevent malicious actors from performing a griefing attack
by expiring messages upon arrival.

Once the expired message is sent to the source chain, the message on the local chain is set
as successful in the `successfulMessages` mapping to ensure non-replayability and deleted
from `failedMessages`. An initiating message is then emitted to `relayExpire`

```solidity
function sendExpire(bytes32 _expiredHash) external nonReentrant {
    if (successfulMessages[_expiredHash]) revert MessageAlreadyRelayed();

    (uint256 messageTimestamp, uint256 messageSource) = failedMessages[_expiredHash];

    if (block.timestamp <  messageTimestamp + EXPIRY_WINDOW) revert ExpiryWindowHasNotEnsued();

    delete failedMessages[_expiredHash];
    successfulMessages[_expiredHash] = true;

    bytes memory data = abi.encodeCall(
        L2ToL2CrossDomainMessenger.expired,
        (_expiredHash, messageSource)
    );
    emit SentMessage(data);
}
```

#### Relaying Expired Message Hashes

When relaying an expired message, only message hashes
of actual failed messages should be stored, for this we must ensure the origin
of the log, and caller are all expected contracts.

It's also important to ensure only the hashes of messages that were initiated
in this chain are accepted.

If all checks have been successful, the message has is stored in the
`expiredMessages` mapping. This enables smart contracts to read from it and
check whether a message expired or not, and handle this case accordingly.

```solidity
function relayExpire(bytes32 _expiredHash, uint256 _messageSource) external {
    if (_messageSource != block.chainid) revert IncorrectMessageSource();
    if (expiredMessages[_expiredHash] != 0) revert ExpiredMessageAlreadyRelayed();
    if (msg.sender != Predeploys.CROSS_L2_INBOX) revert ExpiredMessageCallerNotCrossL2Inbox();

    if (CrossL2Inbox(Predeploys.CROSS_L2_INBOX).origin() != Predeploys.L2_TO_L2_CROSS_DOMAIN_MESSENGER) {
        revert CrossL2InboxOriginNotL2ToL2CrossDomainMessenger();
    }

    expiredMessages[_expiredHash] = block.timestamp;

    emit MessageHashExpired(_expiredHash);
}
```

## OptimismSuperchainERC20Factory

| Constant | Value                                        |
| -------- | -------------------------------------------- |
| Address  | `0x4200000000000000000000000000000000000026` |

### OptimismSuperchainERC20

The `OptimismSuperchainERC20Factory` creates ERC20 contracts that implement to the `SuperchainERC20` [standard](token-bridging.md)
and grant mint-burn rights to the `L2StandardBridge` (`OptimismSuperchainERC20`).
These ERC20s are called `OptimismSuperchainERC20` and can be converted back and forth with `OptimismMintableERC20` tokens.
The goal of the `OptimismSuperchainERC20` is to extend functionalities
of the `OptimismMintableERC20` so that they are interop compatible.

### Overview

Anyone can deploy `OptimismSuperchainERC20` contracts by using the `OptimismSuperchainERC20Factory`.

#### Proxy

The `OptimismSuperchainERC20Factory` MUST be a proxied predeploy.
It follows the
[`Proxy.sol` implementation](https://github.com/ethereum-optimism/optimism/blob/v1.1.4/packages/contracts-bedrock/src/universal/Proxy.sol)
and `delegatecall()` to the factory implementation address.

#### Beacon Pattern

It MUST deploy `OptimismSuperchainERC20` as
[BeaconProxies](https://github.com/OpenZeppelin/openzeppelin-contracts/blob/master/contracts/proxy/beacon/BeaconProxy.sol),
as this is the easiest way to upgrade multiple contracts simultaneously.
Each BeaconProxy delegatecalls to the implementation address provided by the Beacon Contract.

The implementation MUST include an `initialize` function that
receives `(address _remoteToken, string _name, string _symbol, uint8 _decimals)` and stores these in the BeaconProxy storage.

#### Deployment history

The `L2StandardBridge` includes a `convert()` function that allows anyone to convert
between any `OptimismMintableERC20` and its corresponding `OptimismSuperchainERC20`.
For this method to work, the `OptimismSuperchainERC20Factory` MUST include a deployment history.

### Functions

#### `deploy`

Creates an instance of the `OptimismSuperchainERC20` contract with a set of metadata defined by:

- `_remoteToken`: address of the underlying token in its native chain.
- `_name`: `OptimismSuperchainERC20` name
- `_symbol`: `OptimismSuperchainERC20` symbol
- `_decimals`: `OptimismSuperchainERC20` decimals

```solidity
deploy(address _remoteToken, string memory _name, string memory _symbol, uint8 _decimals) returns (address)
```

It returns the address of the deployed `OptimismSuperchainERC20`.

The function MUST use `CREATE3` to deploy its children.
This ensures the same address deployment across different chains,
which is necessary for the [standard](token-bridging.md) implementation.

The salt used for deployment MUST be computed by applying `keccak256` to the `abi.encode`
of the input parameters (`_remoteToken`, `_name`, `_symbol`, and `_decimals`).
This implies that the same L1 token can have multiple `OptimismSuperchainERC20` representations as long as the metadata changes.

The function MUST store the `_remoteToken` address for each deployed `OptimismSuperchainERC20` in a `deployments` mapping.

### Events

#### `OptimismSuperchainERC20Created`

It MUST trigger when `deploy` is called.

```solidity
event OptimismSuperchainERC20Created(address indexed superchainToken, address indexed remoteToken, address deployer);
```

where `superchainToken` is the address of the newly deployed `OptimismSuperchainERC20`,
`remoteToken` is the address of the corresponding token in L1,
and deployer`is the`msg.sender`.

### Deployment Flow

```mermaid
sequenceDiagram
  participant Alice
  participant FactoryProxy
  participant FactoryImpl
  participant BeaconProxy as OptimismSuperchainERC20 BeaconProxy
  participant Beacon Contract
  participant Implementation
  Alice->>FactoryProxy: deploy(remoteToken, name, symbol, decimals)
  FactoryProxy->>FactoryImpl: delegatecall()
  FactoryProxy->>BeaconProxy: deploy with CREATE3
  FactoryProxy-->FactoryProxy: deployments[superchainToken]=remoteToken
  FactoryProxy-->FactoryProxy: emit OptimismSuperchainERC20Created(superchainToken, remoteToken, Alice)
  BeaconProxy-->Beacon Contract: reads implementation()
  BeaconProxy->>Implementation: delegatecall()
  BeaconProxy->>Implementation: initialize()
```

## BeaconContract

| Constant | Value                                        |
| -------- | -------------------------------------------- |
| Address  | `0x4200000000000000000000000000000000000027` |

### Overview

The `BeaconContract` predeploy gets called by the `OptimismSuperchainERC20`
BeaconProxies deployed by the
[`SuperchainERC20Factory`](#optimismsuperchainerc20factory)

The Beacon Contract implements the interface defined
in [EIP-1967](https://eips.ethereum.org/EIPS/eip-1967).

The implementation address gets deduced similarly to the `GasPriceOracle` address in Ecotone and Fjord updates.

## L1Block

| Constant            | Value                                        |
| ------------------- | -------------------------------------------- |
| Address             | `0x4200000000000000000000000000000000000015` |
| `DEPOSITOR_ACCOUNT` | `0xDeaDDEaDDeAdDeAdDEAdDEaddeAddEAdDEAd0001` |

### Static Configuration

The `L1Block` contract MUST include method `setConfig(ConfigType, bytes)` for setting the system's static values, which
are defined as values that only change based on the chain operator's input. This function serves to reduce the size of
the L1 Attributes transaction, as well as to reduce the need to add specific one off functions. It can only be called by
`DEPOSITOR_ACCOUNT`.

The `ConfigType` enum is defined as follows:

```solidity
enum ConfigType {
    SET_GAS_PAYING_TOKEN,
    ADD_DEPENDENCY,
    REMOVE_DEPENDENCY
}
```

The second argument to `setConfig` is a `bytes` value that is ABI encoded with the necessary values for the `ConfigType`.

| ConfigType             | Value                                       |
| ---------------------- | ------------------------------------------- |
| `SET_GAS_PAYING_TOKEN` | `abi.encode(token, decimals, name, symbol)` |
| `ADD_DEPENDENCY`       | `abi.encode(chainId)`                       |
| `REMOVE_DEPENDENCY`    | `abi.encode(chainId)`                       |

where

- `token` is the gas paying token's address (type `address`)

- `decimals` is the gas paying token's decimals (type `uint8`)

- `name` is the gas paying token's name (type `bytes32`)

- `symbol` is the gas paying token's symbol (type `bytes32`)

- `chainId` is the chain id intended to be added or removed from the dependency set (type `uint256`)

Calls to `setConfig` MUST originate from `SystemConfig` and are forwarded to `L1Block` by `OptimismPortal`.

### Dependency Set

`L1Block` is updated to include the set of allowed chains. These chains are added and removed through `setConfig` calls
with `ADD_DEPENDENCY` or `REMOVE_DEPENDENCY`, respectively. The maximum size of the dependency set is `type(uint8).max`,
and adding a chain id when the dependency set size is at its maximum MUST revert. If a chain id already in the
dependency set, such as the chain's chain id, is attempted to be added, the call MUST revert. If a chain id that is not
in the dependency set is attempted to be removed, the call MUST revert. If the chain's chain id is attempted to be
removed, the call also MUST revert.

`L1Block` MUST provide a public getter to check if a particular chain is in the dependency set called
`isInDependencySet(uint256)`. This function MUST return true when a chain id in the dependency set, or the chain's chain
id, is passed in as an argument, and false otherwise. Additionally, `L1Block` MUST provide a public getter to return the
dependency set called `dependencySet()`. This function MUST return the array of chain ids that are in the dependency set.
`L1Block` MUST also provide a public getter to get the dependency set size called `dependencySetSize()`. This function
MUST return the length of the dependency set array.

### Deposit Context

New methods will be added on the `L1Block` contract to interact with [deposit contexts](./derivation.md#deposit-context).

```solidity
function isDeposit() public view returns (bool);
function depositsComplete() public;
```

### `isDeposit()`

Returns true if the current execution occurs in a [deposit context](./derivation.md#deposit-context).

Only the `CrossL2Inbox` is authorized to call `isDeposit`.
This is done to prevent apps from easily detecting and censoring deposits.

#### `depositsComplete()`

Called after processing the first L1 Attributes transaction and user deposits to destroy the deposit context.

Only the `DEPOSITOR_ACCOUNT` is authorized to call `depositsComplete()`.

## OptimismMintableERC20Factory

| Constant | Value                                        |
| -------- | -------------------------------------------- |
| Address  | `0x4200000000000000000000000000000000000012` |

### OptimismMintableERC20

The `OptimismMintableERC20Factory` creates ERC20 contracts on L2 that can be used to deposit
native L1 tokens into (`OptimismMintableERC20`). Anyone can deploy `OptimismMintableERC20` contracts.

Each `OptimismMintableERC20` contract created by the `OptimismMintableERC20Factory`
allows for the `L2StandardBridge` to mint
and burn tokens, depending on whether the user is
depositing from L1 to L2 or withdrawing from L2 to L1.

### Updates

The `OptimismMintableERC20Factory` is updated to include a `deployments` mapping
that stores the `remoteToken` address for each deployed `OptimismMintableERC20`.
This is essential for the liquidity migration process defined in the liquidity migration spec.

### Functions

#### `createOptimismMintableERC20WithDecimals`

Creates an instance of the `OptimismMintableERC20` contract with a set of metadata defined by:

- `_remoteToken`: address of the underlying token in its native chain.
- `_name`: `OptimismMintableERC20` name
- `_symbol`: `OptimismMintableERC20` symbol
- `_decimals`: `OptimismMintableERC20` decimals

```solidity
createOptimismMintableERC20WithDecimals(address _remoteToken, string memory _name, string memory _symbol, uint8 _decimals) returns (address)
```

**Invariants**

- The function MUST use `CREATE2` to deploy new contracts.
- The salt MUST be computed by applying `keccak256` to the `abi.encode`
  of the four input parameters (`_remoteToken`, `_name`, `_symbol`, and `_decimals`).
  This ensures a unique `OptimismMintableERC20` for each set of ERC20 metadata.
- The function MUST store the `_remoteToken` address for each deployed `OptimismMintableERC20` in a `deployments` mapping.

#### `createOptimismMintableERC20`

Creates an instance of the `OptimismMintableERC20` contract with a set of metadata defined
by `_remoteToken`, `_name` and `_symbol` and fixed `decimals` to the standard value 18.

```solidity
createOptimismMintableERC20(address _remoteToken, string memory _name, string memory _symbol) returns (address)
```

#### `createStandardL2Token`

Creates an instance of the `OptimismMintableERC20` contract with a set of metadata defined
by `_remoteToken`, `_name` and `_symbol` and fixed `decimals` to the standard value 18.

```solidity
createStandardL2Token(address _remoteToken, string memory _name, string memory _symbol) returns (address)
```

This function exists for backwards compatibility with the legacy version.

### Events

#### `OptimismMintableERC20Created`

It MUST trigger when `createOptimismMintableERC20WithDecimals`,
`createOptimismMintableERC20` or `createStandardL2Token` are called.

```solidity
event OptimismMintableERC20Created(address indexed localToken, address indexed remoteToken, address deployer);
```

#### `StandardL2TokenCreated`

It MUST trigger when `createOptimismMintableERC20WithDecimals`,
`createOptimismMintableERC20` or `createStandardL2Token` are called.
This event exists for backward compatibility with legacy version.

```solidity
event StandardL2TokenCreated(address indexed remoteToken, address indexed localToken);
```

## L2StandardBridge

| Constant | Value                                        |
| -------- | -------------------------------------------- |
| Address  | `0x4200000000000000000000000000000000000010` |

### Updates

The `OptimismMintableERC20` and `L2StandardToken` tokens (_legacy tokens_),
which correspond to locked liquidity in L1, are incompatible with interop.
Legacy token owners must convert into a `OptimismSuperchainERC20` representation that implements the [standard](token-bridging.md),
to move across the Superchain.

The conversion method uses the `L2StandardBridge` mint/burn rights
over the legacy tokens to allow easy migration to and from the
corresponding `OptimismSuperchainERC20`.

#### convert

The `L2StandardBridge` SHOULD add a `convert` public function that
converts `_amount` of `_from` token to `_amount` of `_to` token,
if and only if the token addresses are valid (as defined below).

```solidity
convert(address _from, address _to, uint256 _amount)
```

The function

1. Checks that `_from` and `_to` addresses are valid, paired and have the same amount of decimals.
2. Burns `_amount` of `_from` from `msg.sender`.
3. Mints `_amount` of `_to` to `msg.sender`.

#### `Converted`

The `L2StandardBridge` SHOULD include a `Converted` event
that MUST trigger when anyone converts tokens
with `convert`.

```solidity
event Converted(address indexed from, address indexed to, address indexed caller, uint256 amount);
```

where `from` is the address of the input token, `to` is the address of the output token,
`caller` is the `msg.sender` of the function call and `amount` is the converted amount.

### Invariants

The `convert` function conserves the following invariants:

- Conservation of amount:
  The burnt amount should match the minted amount.
- Revert for non valid or non paired: `convert` SHOULD revert when called with:
  - Tokens with different decimals.
  - Legacy tokens that are not in the `deployments` mapping from the `OptimismMintableERC20Factory`.
  - `OptimismSuperchainERC20` that are not in the `deployments` mapping from the `OptimismSuperchainERC20Factory`.
  - Legacy tokens and `OptimismSuperchainERC20s`s
    corresponding to different
    remote token addresses.
- Freedom of conversion for valid and paired tokens:
  anyone can convert between allowed legacy representations and
  valid `OptimismSuperchainERC20s` corresponding to the same remote token.

### Conversion Flow

```mermaid
sequenceDiagram
  participant Alice
  participant L2StandardBridge
  participant factory as OptimismMintableERC20Factory
  participant superfactory as OptimismSuperchainERC20Factory
  participant legacy as from Token
  participant SuperERC20 as to Token

  Alice->>L2StandardBridge: convert(from, to, amount)
  L2StandardBridge-->factory: check legacy token is allowed
  L2StandardBridge-->superfactory: check super token is allowed
  L2StandardBridge-->L2StandardBridge: checks matching remote and decimals
  L2StandardBridge->>legacy: IERC20(from).burn(Alice, amount)
  L2StandardBridge->>SuperERC20: IERC20(to).mint(Alice, amount)
  L2StandardBridge-->L2StandardBridge: emit Converted(from, to, Alice, amount)
```

## Security Considerations

TODO
