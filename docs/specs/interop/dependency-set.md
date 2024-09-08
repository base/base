# The Dependency Set

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Chain ID](#chain-id)
- [Updating the Dependency Set](#updating-the-dependency-set)
- [Security Considerations](#security-considerations)
  - [Layer 1 as Part of the Dependency Set](#layer-1-as-part-of-the-dependency-set)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

The dependency set defines the set of chains that destination chains allow as source chains. Another way of
saying it is that the dependency set defines the set of initiating messages that are valid for an executing
message to be included. An executing message MUST have an initiating message that is included in a chain
in the dependency set.

The dependency set is defined by a set of chain ids. Since it is impossible to enforce uniqueness of chain ids,
social consensus MUST be used to determine the chain that represents the canonical chain id. This
particularly impacts the block builder as they SHOULD use the chain id to assist in validation
of executing messages.

The dependency set is configured on a per chain basis and is a unidirectional relationship. This means
that it is possible to depend on a chain without it depending back. This means that it is possible
to send assets to a chain where they cannot be sent back.

The chain id of the local chain MUST be considered as part of its own dependency set. This allows a chain
to consume logs that it has produced much more cheaply than providing a block hash proof.

While the dependency set explicitly defines the set of chains that are depended on for incoming messages,
the full set of transitive dependencies must be known to allow for the progression of safety.
This means that the `op-node` needs to be aware of all transitive dependencies.

## Chain ID

The concept of a chain id was introduced in [EIP-155](https://eips.ethereum.org/EIPS/eip-155) to prevent
replay attacks between chains. This EIP does not specify the max size of a chain id, although
[EIP-2294](https://eips.ethereum.org/EIPS/eip-2294) attempts to add a maximum size. Since this EIP is
stagnant, all representations of chain ids MUST be the `uint256` type.

In the future, OP Stack chains reserve the right to use up to 32 bytes to represent a chain id. The
configuration of the chain should deterministically map to a chain id and with careful architecture
changes, all possible OP Stack chains in the superchain will be able to exist counterfactually.

It is a known issue that not all software in the Ethereum can handle 32 byte chain ids.

## Updating the Dependency Set

The `SystemConfig` is updated to manage a new role, `dependencyManager`.
It can only updated by the `ProxyAdmin` during an contract upgrade.
The sole holder of this role is the only address
permissioned to update (remove/add to) the dependency set of that chain.

The `SystemConfig` is also updated to manage the dependency set.
The address with the `dependency manager` role can add or remove
chains from the dependency set through the `SystemConfig`.

The `SystemConfig` MUST enforce that the maximum size of the dependency set is `type(uint8).max` or 255.

## Security Considerations

### Layer 1 as Part of the Dependency Set

The layer one MAY be part of the dependency set if the fault proof implementation is set up
to support it. It is known that it is possible but it is not known if this is going to be
a feature of the first release. This section should be clarified when the decision is made.

If layer one is part of the dependency set, then it means that any event on L1 can be pulled
into any L2. This is a very powerful abstraction as a minimal amount of execution can happen
on L1 which triggers additional execution across all L2s in the OP Stack.
