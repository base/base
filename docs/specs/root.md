# OP Stack Specs

This directory contains the plain english specs for Optimism, a minimal optimistic rollup protocol
that maintains 1:1 compatibility with Ethereum.

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Specification Contents](#specification-contents)
  - [Experimental](#experimental)
- [Design Goals](#design-goals)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Specification Contents

- [Introduction](introduction.md)
- [Overview](protocol/overview.md)
- [Deposits](protocol/deposits.md)
- [Withdrawals](protocol/withdrawals.md)
- [Execution Engine](protocol/exec-engine.md)
- [L2 Output Root Proposals](protocol/proposals.md)
- [Rollup Node](protocol/rollup-node.md)
- [Rollup Node P2p](protocol/rollup-node-p2p.md)
- [L2 Chain Derivation](protocol/derivation.md)
- [Superchain Upgrades](protocol/superchain-upgrades.md)
- [System Config](protocol/system_config.md)
- [Batch Submitter](protocol/batcher.md)
- [Guaranteed Gas Market](protocol/guaranteed-gas-market.md)
- [Messengers](protocol/messengers.md)
- [Bridges](protocol/bridges.md)
- [Predeploys](protocol/predeploys.md)
- [Preinstalls](protocol/preinstalls.md)
- [Glossary](glossary.md)

### Experimental

Specifications of new features in active development.

- [Fault Proof](./experimental/fault-proof/index.md)
  - [Stage One Decentralization]()
    - [Dispute Game Interface](./experimental/fault-proof/stage-one/dispute-game-interface.md)
    - [Fault Dispute Game](./experimental/fault-proof/stage-one/fault-dispute-game.md)
      - [Bond Incentives](./experimental/fault-proof/stage-one/bond-incentives.md)
      - [Honest Challenger Behavior](./experimental/fault-proof/stage-one/honest-challenger-fdg.md)
  - [Cannon VM](./experimental/fault-proof/cannon-fault-proof-vm.md)
- [Plasma](./experimental/plasma.md)
- [Interoperability](./interop/overview.md)

## Design Goals

Our aim is to design a protocol specification that is:

- **Fast:** When users send transactions, they get reliable confirmations with low-latency.
  For example when swapping on Uniswap you should see that your transaction succeeds in less than 2
  seconds.
- **Scalable:** It should be possible to handle an enormous number of transactions
  per second which will enable the system to charge low fees.
  V1.0 will enable Optimism to scale up to and even past the gas limit on L1.
  Later iterations should scale much further.
- **Modular:** Our designs will use modularity to reduce complexity and enable parallel
  contributions. Coming up with good conceptual frameworks & composable atoms of software enables us
  to build extremely complex software even when any one person cannot hold that much in their brain.
- **Minimal:** Rollups should be minimal to best take advantage of the battle-tested infrastructure
  (like Geth) that already runs Ethereum. An ideal optimistic rollup design should be representable
  as a _diff_ against Ethereum client software.
- **Developer Driven:** Our designs will be developer driven to ensure we are actually building
  something that people want to use. We must constantly engage with the developers who will be using
  our software to avoid creating a system no one wants to use.
- **Clear and Readable:** The specs we write are written to be read. So tight feedback loop with the
  systems team consuming the spec is also key!
- **Secure:** This is self-evident.
  User’s assets are at stake. Every component of the system must be incredibly secure.
- **Decentralizable:** Optimism must be designed to avail itself of the security and
  censorship-resistant guarantees achieved by a decentralized system.
  Currently centralized components of the system should have a clear path towards decentralization.
  Already decentralized components of the system should be protected and preserved.
