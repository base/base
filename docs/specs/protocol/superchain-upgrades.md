# Superchain Upgrades

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Protocol Version](#protocol-version)
  - [Protocol Version Format](#protocol-version-format)
    - [Build identifier](#build-identifier)
    - [Major versions](#major-versions)
    - [Minor versions](#minor-versions)
    - [Patch versions](#patch-versions)
    - [Pre-releases](#pre-releases)
  - [Protocol Version Exposure](#protocol-version-exposure)
- [Superchain Target](#superchain-target)
  - [Superchain Version signaling](#superchain-version-signaling)
  - [`ProtocolVersions` L1 contract](#protocolversions-l1-contract)
- [Activation rules](#activation-rules)
  - [L2 Block-number based activation (deprecated)](#l2-block-number-based-activation-deprecated)
  - [L2 Block-timestamp based activation](#l2-block-timestamp-based-activation)
- [OP-Stack Protocol versions](#op-stack-protocol-versions)
- [Post-Bedrock Network upgrades](#post-bedrock-network-upgrades)
  - [Activation Timestamps](#activation-timestamps)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

Superchain upgrades, also known as forks or hardforks, implement consensus-breaking changes.

A Superchain upgrade requires the node software to support up to a given Protocol Version.
The version indicates support, the upgrade indicates the activation of new functionality.

This document lists the protocol versions of the OP-Stack, starting at the Bedrock upgrade,
as well as the default Superchain Targets.

Activation rule parameters of network upgrades are configured as part of the Superchain Target specification:
chains following the same Superchain Target upgrade synchronously.

## Protocol Version

The Protocol Version documents the progression of the total set of canonical OP-Stack specifications.
Components of the OP-Stack implement the subset of their respective protocol component domain,
up to a given Protocol Version of the OP-Stack.

OP-Stack mods, i.e. non-canonical extensions to the OP-Stack, are not included in the versioning of the Protocol.
Instead, mods must specify which upstream Protocol Version they are based on and where breaking changes are made.
This ensures tooling of the OP-Stack can be shared and collaborated on with OP-Stack mods.

The Protocol Version is NOT a hardfork identifier, but rather indicates software-support for a well-defined set
of features introduced in past and future hardforks, not the activation of said hardforks.

Changes that can be included in prospective Protocol Versions may be included in the specifications as proposals,
with explicit notice of the Protocol Version they are based on.
This enables an iterative integration process into the canonical set of specifications,
but does not guarantee the proposed specifications become canonical.

Note that the Protocol Version only applies to the Protocol specifications with the Superchain Targets specified within.
This versioning is independent of the [Semver] versioning used in OP Stack smart-contracts,
and the [Semver]-versioned reference software of the OP-Stack.

### Protocol Version Format

The Protocol Version is [Semver]-compatible.
It is encoded as a single 32 bytes long `<protocol version>`.
The version must be encoded as 32 bytes of `DATA` in JSON RPC usage.

The encoding is typed, to ensure future-compatibility.

```text
<protocol version> ::= <version-type><typed-payload>
<version-type> ::= <uint8>
<typed-payload> ::= <31 bytes>
```

version-type `0`:

```text
<reserved><build><major><minor><patch><pre-release>
<reserved> ::= <7 zeroed bytes>
<build> ::= <8 bytes>
<major> ::= <big-endian uint32>
<minor> ::= <big-endian uint32>
<patch> ::= <big-endian uint32>
<pre-release> ::= <big-endian uint32>
```

The `<reserved>` bytes of the Protocol Version are reserved for future extensions.

Protocol versions with a different `<version-type>` should not be compared directly.

[Semver]: https://semver.org/

#### Build identifier

The `<build>` identifier, as defined by [Semver], is ignored when determining version precedence.
The `<build>` must be non-zero to apply to the protocol version.

Modifications of the OP-Stack should define a `<build>` to distinguish from the canonical protocol feature-set.
Changes to the `<build>` may be encoded in the `<build>` itself to stay aligned with the upstream protocol.
The major/minor/patch versions should align with that of the upstream protocol that the modifications are based on.
Users of the protocol can choose to implement custom support for the alternative `<build>`,
but may work out of the box if the major features are consistent with that of the upstream protocol version.

The 8 byte `<build>` identifier may be presented as a string for human readability if the contents are alpha-numeric,
including `-` and `.`, as outlined in the [Semver] format specs. Trailing `0` bytes can be used for padding.
It may be presented as `0x`-prefixed hex string otherwise.

#### Major versions

Major version changes indicate support for new consensus-breaking functionality.
Major versions should retain support for functionality of previous major versions for
syncing/indexing of historical chain data.
Implementations may drop support for previous Major versions, when there are viable alternatives,
e.g. `l2geth` for pre-Bedrock data.

#### Minor versions

Minor version changes indicate support for backward compatible extensions,
including backward-compatible additions to the set of chains in a Superchain Target.
Backward-compatibility is defined by the requirement for existing end-users to upgrade nodes and tools or not.
Minor version changes may also include optional offchain functionality, such as additional syncing protocols.

#### Patch versions

Patch version changes indicate backward compatible bug fixes and improvements.

#### Pre-releases

Pre-releases of the protocol are proposals: these are not stable targets for production usage.
A pre-release might not satisfy the intended compatibility requirements as denoted by its associated normal version.
The `<pre-release>` must be non-zero to apply to the protocol version.
The `<pre-release>` `0`-value is reserved for non-prereleases, i.e. `v3.1.0` is higher than `v3.1.0-1`.

Node-software may support a pre-release, but must not activate any protocol changes without the user explicitly
opting in through the means of a feature-flag or configuration change.

A pre-release is not an official version and is meant for protocol developers to communicate an experimental changeset
before the changeset is reviewed by governance. Pre-releases are subject to change.

### Protocol Version Exposure

The Protocol Version is not exposed to the application-layer environment:
hardforks already expose the change of functionality upon activation as required,
and the Protocol Version is meant for offchain usage only.
The protocol version indicates support rather than activation of functionality.
There is one exception however: signaling by onchain components to offchain components.
More about this in [Superchain Version signaling](#superchain-version-signaling).

## Superchain Target

Changes to the L2 state-transition function are transitioned into deterministically across all nodes
through an **activation rule**.

Changes to L1 smart-contracts must be compatible with the latest activated L2 functionality,
and are executed through **L1 contract-upgrades**.

A Superchain Target defines a set of activation rules and L1 contract upgrades shared between OP-Stack chains,
to upgrade the chains collectively.

### Superchain Version signaling

Each Superchain Target tracks the protocol changes, and signals the `recommended` and `required`
Protocol Version ahead of activation of new breaking functionality.

- `recommended`: a signal in advance of a network upgrade, to alert users of the protocol change to be prepared for.
  Node software is recommended to signal the recommendation to users through logging and metrics.
- `required`: a signal shortly in advance of a breaking network upgrade, to alert users of breaking changes.
  Users may opt in to elevated alerts or preventive measures, to ensure consistency with the upgrade.

Signaling is done through a L1 smart-contract that is monitored by the OP-Stack software.
Not all components of the OP-Stack are required to directly monitor L1 however:
cross-component APIs like the Engine API may be used to forward the Protocol Version signals,
to keep components encapsulated from L1.
See [`engine_signalOPStackVersionV1`](exec-engine.md#enginesignalopstackversionv1).

### `ProtocolVersions` L1 contract

The `ProtocolVersions` contract on L1 enables L2 nodes to pick up on superchain protocol version signals.

The interface is:

- Required storage slot: `bytes32(uint256(keccak256("protocolversion.required")) - 1)`
- Recommended storage slot: `bytes32(uint256(keccak256("protocolversion.recommended")) - 1)`
- Required getter: `required()` returns `ProtocolVersion`
- Recommended getter `recommended()` returns `ProtocolVersion`
- Version updates also emit a typed event:
  `event ConfigUpdate(uint256 indexed version, UpdateType indexed updateType, bytes data)`

## Activation rules

The below L2-block based activation rules may be applied in two contexts:

- The rollup node, specified through the rollup configuration (known as `rollup.json`),
  referencing L2 blocks (or block input-attributes) that pass through the derivation pipeline.
- The execution engine, specified through the chain configuration (known as the `config` part of `genesis.json`),
  referencing blocks or input-attributes that are part of, or applied to, the L2 chain.

For both types of configurations, some activation parameters may apply to all chains within the superchain,
and are then retrieved from the superchain target configuration.

### L2 Block-number based activation (deprecated)

Activation rule: `upgradeNumber != null && block.number >= upgradeNumber`

Starting at, and including, the L2 `block` with `block.number >= upgradeNumber`, the upgrade rules apply.
If the upgrade block-number `upgradeNumber` is not specified in the configuration, the upgrade is ignored.

This block number based method has commonly been used in L1 up until the Bellatrix/Paris upgrade, a.k.a. The Merge,
which was upgraded through special rules.

This method is not superchain-compatible, as the activation-parameter is chain-specific
(different chains may have different block-heights at the same moment in time).

This applies to the L2 block number, not to the L1-origin block number.
This means that an L2 upgrade may be inactive, and then active, without changing the L1-origin.

### L2 Block-timestamp based activation

Activation rule: `upgradeTime != null && block.timestamp >= upgradeTime`

Starting at, and including, the L2 `block` with `block.timestamp >= upgradeTime`, the upgrade rules apply.
If the upgrade block-timestamp `upgradeTime` is not specified in the configuration, the upgrade is ignored.

This is the preferred superchain upgrade activation-parameter type:
it is synchronous between all L2 chains and compatible with post-Merge timestamp-based chain upgrades in L1.

This applies to the L2 block timestamp, not to the L1-origin block timestamp.
This means that an L2 upgrade may be inactive, and then active, without changing the L1-origin.

This timestamp based method has become the default on L1 after the Bellatrix/Paris upgrade, a.k.a. The Merge,
because it can be planned in accordance with beacon-chain epochs and slots.

Note that the L2 version is not limited to timestamps that match L1 beacon-chain slots or epochs.
A timestamp may be chosen to be synchronous with a specific slot or epoch on L1,
but the matching L1-origin information may not be present at the time of activation on L2.

## OP-Stack Protocol versions

- `v1.0.0`: 2021 Jan 16th - Mainnet Soft Launch, based on OVM.
  ([announcement](https://medium.com/ethereum-optimism/mainnet-soft-launch-7cacc0143cd5))
- `v1.1.0`: 2021 Aug 19th - Community launch.
  ([announcement](https://medium.com/ethereum-optimism/community-launch-7c9a2a9d3e84))
- `v2.0.0`: 2021 Nov 12th - the EVM-Equivalence update, also known as OVM 2.0 and chain regenesis.
  ([announcement](https://twitter.com/optimismfnd/status/1458953238867165192))
- `v2.1.0`: 2022 May 31st - Optimism Collective.
  ([announcement](https://optimism.mirror.xyz/gQWKlrDqHzdKPsB1iUnI-cVN3v0NvsWnazK7ajlt1fI)).
- `v3.0.0-1`: 2023 Jan 13th - Bedrock pre-release, deployed on OP-Goerli, and later Base-Goerli.
- `v3.0.0`: 2023 Jun 6th - Bedrock, including the Regolith hardfork improvements, first deployed on OP-Mainnet.
- `v4.0.0`: 2024 Jan 11th - Canyon network upgrade (Shapella).
  [Governance Proposal](https://gov.optimism.io/t/final-upgrade-proposal-2-canyon-network-upgrade/7088).
- `v5.0.0`: 2024 Feb 22nd - Delta network upgrade (Span Batches).
  [Governance Proposal](https://gov.optimism.io/t/final-upgrade-proposal-3-delta-network-upgrade/7310).
- `v6.0.0`: 2024 Mar 14th - Ecotone network upgrade (4844 Blob Batches + Cancun).
  [Governance Proposal](https://gov.optimism.io/t/upgrade-proposal-5-ecotone-network-upgrade/7669).
- `v7.0.0`: 2024 Jul 10th - Fjord network upgrade (RIP-7212 precompile + FastLZ cost fn + Brotli compression).
  [Governance Proposal](https://gov.optimism.io/t/upgrade-proposal-9-fjord-network-upgrade/8236).
- `v8.0.0`: 2024 Sep 11th - Granite network upgrade (Limit ecpairing input size + Reduce Channel Timeout).
  [Governance Proposal](https://gov.optimism.io/t/upgrade-proposal-10-granite-network-upgrade/8733).

## Post-Bedrock Network upgrades

### Activation Timestamps

Governance approves all network upgrades & the time at which the upgrade activates. The approved governance
proposal is the canonical document for the timestamp; however, the timestamps are replicated here for ease of use.

|Network Upgrade|Mainnet Upgrade Timestamp|Sepolia Upgrade Timestamp|Goerli Upgrade Timestamp|
|---------------|-------------------------|-------------------------|------------------------|
|Canyon         |               1704992401|               1699981200|              1699981200|
|Delta          |               1708560000|               1703203200|              1703116800|
|Ecotone        |               1710374401|               1708534800|              1707238800|
|Fjord          |               1720627201|               1716998400|                     n/a|
|Granite        |               1726070401|               1723478400|                     n/a|
