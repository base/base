# The Dependency Set

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Chain ID](#chain-id)
- [Updating the Dependency Set](#updating-the-dependency-set)
- [Future Considerations](#future-considerations)
  - [Layer 1 as Part of the Dependency Set](#layer-1-as-part-of-the-dependency-set)
- [Security Considerations](#security-considerations)
  - [Dependency Set Size](#dependency-set-size)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

The dependency set defines the set of chains that destination chains allow as source chains. Another way of
saying it is that the dependency set defines the set of initiating messages that are valid to be used
as part of an executing message. An executing message MUST have an initiating message that is created by a chain
in the dependency set.

The dependency set is defined by a set of chain ids. Since it is impossible to enforce uniqueness of chain ids,
social consensus MUST be used to determine the chain that represents the canonical chain id. This
particularly impacts the block builder as they SHOULD use the chain id to assist in validation
of executing messages.

The dependency set is configured on a per cluster basis. All chains that are in the dependency set
can accept initiating messages from any other chain in the dependency set, resulting in a mesh.

The chain id of the local chain MUST be considered as part of its own dependency set. This allows a chain
to consume logs that it has produced much more cheaply than providing a block hash proof.

## Chain ID

The concept of a chain id was introduced in [EIP-155](https://eips.ethereum.org/EIPS/eip-155) to prevent
replay attacks between chains. This EIP does not specify the max size of a chain id, although
[EIP-2294](https://eips.ethereum.org/EIPS/eip-2294) attempts to add a maximum size. Since this EIP is
stagnant, all representations of chain ids MUST be the `uint256` type.

In the future, OP Stack chains reserve the right to use up to 32 bytes to represent a chain id. The
configuration of the chain should deterministically map to a chain id and with careful architecture
changes, all possible OP Stack chains in the superchain will be able to exist counterfactually.

It is a known issue that not all software in the Ethereum ecosystem can handle 32 byte chain ids.

## Updating the Dependency Set

The dependency set is managed in the client software. Adding a chain to the dependency set is
considered an upgrade to the network. It is not possible to remove chains from the dependency set.

## Future Considerations

### Layer 1 as Part of the Dependency Set

The layer one MAY be part of the dependency set in the future. This means that any event
created on layer one is consumable on layer two.

## Security Considerations

### Dependency Set Size

It becomes increasingly expensive to fully validate the full cluster as the size of the dependency
set grows. The proof system requires validating all of the chains so the size of the dependency
set is limited by the performance of the proof.
