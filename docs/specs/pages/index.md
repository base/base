# Base Specification

This specification defines the Base Chain protocol: how nodes derive and execute blocks, how
transactions are propagated, and how state transitions are verified. It covers core protocol rules,
execution behavior, and proving.

## Design Goals

Our aim is to design a protocol specification that is:

- **Opinionated:** Simplicity through deliberate design choices. We identify the best solution and
  commit to it.
- **Maximally Simple:** By focusing on just what Base needs, we radically simplify the stack. The
  protocol spec and codebase should be understandable by a single developer.
- **Fast Cycles:** We ship upgrades frequently rather than batching risk into infrequent large ones.
  We target six smaller, tightly scoped hard forks per year on a regular cadence, with fortnightly
  releases.
- **Ethereum Aligned:** Base wins when Ethereum wins. We accelerate deployment of high-impact
  changes ahead of L1 to provide data that informs the Ethereum roadmap.

## Lineage

Base Chain inherits Ethereum's EVM semantics, transaction rules, and L1-anchored security. It was
originally built on the [OP Stack](https://specs.optimism.io). After the Jovian Hardfork, Base
Chain follows this specification.
