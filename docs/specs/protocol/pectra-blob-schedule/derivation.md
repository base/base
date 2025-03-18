# Pectra Blob Schedule Derivation

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [If enabled](#if-enabled)
- [If disabled (default)](#if-disabled-default)
- [Motivation and Rationale](#motivation-and%C2%A0rationale)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## If enabled

If this hardfork is enabled (i.e. if there is a non nil hardfork activation timestamp set), the following rules apply:

When setting the [L1 Attributes Deposited Transaction](../../glossary.md#l1-attributes-deposited-transaction),
the adoption of the Pectra blob base fee update fraction
(see [EIP-7691](https://github.com/ethereum/EIPs/blob/master/EIPS/eip-7691.md))
occurs for L2 blocks with an L1 origin equal to or greater than the hard fork timestamp.
For L2 blocks with an L1 origin less than the hard fork timestamp, the Cancun blob base fee update fraction is used
(see [EIP-4844](https://github.com/ethereum/EIPs/blob/master/EIPS/eip-4844.md)).

## If disabled (default)

If the hardfork activation timestamp is nil, the blob base fee update rules which are active
at any given L1 block will apply to the L1 Attributes Deposited Transaction.

## Motivation and Rationale

Due to a consensus layer bug, OPStack chains on Holesky and Sepolia running officially released op-node software
did not update their blob base fee update fraction (for L1 Attributes Deposited Transaction)
in tandem with the Prague upgrade on L1.

These chains, or any OPStack chain with a sequencer running
the buggy consensus code[^1] when Holesky/Sepolia activated Pectra,
will have an inaccurate blob base fee in the [L1Block](../../protocol/predeploys.md#l1block) contract.
This optional fork is a mechanism to bring those chains back in line.
It is unnecessary for chains using Ethereum mainnet for L1 and running op-node
[v1.12.0](https://github.com/ethereum-optimism/optimism/releases/tag/op-node%2Fv1.12.0)
or later before Pectra activates on L1.

Activating by L1 origin preserves the invariant that the L1BlockInfo is constant for blocks with the same epoch.

[^1]: This is any commit _before_ the code was fixed in [aabf3fe054c5979d6a0008f26fe1a73fdf3aad9f](https://github.com/ethereum-optimism/optimism/commit/aabf3fe054c5979d6a0008f26fe1a73fdf3aad9f)
