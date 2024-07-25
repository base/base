# OP Stack Specs

This directory contains the plain english specs for Optimism, a minimal optimistic rollup protocol
that maintains 1:1 compatibility with Ethereum.

Chat with us on the [discussion board!](https://github.com/ethereum-optimism/specs/discussions)

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Specification Contents](#specification-contents)
  - [Experimental](#experimental)
- [Design Goals](#design-goals)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Specification Contents

- [Background](background.md)
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
- [Fault Proof](fault-proof/index.md)
  - [Stage One Decentralization](fault-proof/stage-one/index.md)
    - [Dispute Game Interface](fault-proof/stage-one/dispute-game-interface.md)
    - [Fault Dispute Game](fault-proof/stage-one/fault-dispute-game.md)
      - [Bond Incentives](fault-proof/stage-one/bond-incentives.md)
      - [Honest Challenger Behavior](fault-proof/stage-one/honest-challenger-fdg.md)
  - [Cannon VM](fault-proof/cannon-fault-proof-vm.md)
- [Glossary](glossary.md)

### Experimental

Specifications of new features in active development.

- [Custom Gas Token](./experimental/custom-gas-token.md)
- [Alt-DA](./experimental/alt-da.md)
- [Interoperability](./interop/overview.md)
- [Security Council Safe](./experimental/security-council-safe.md)
- [OP Stack Manager](./experimental/op-stack-manager.md)

## Design Goals

The aim is to design a protocol specification that is:

- **Fast:** When sending transactions, rollup clients return reliable confirmations with
  low-latency. For example when swapping on Uniswap you should see that your transaction
  succeeds in less than 2 seconds.
- **Scalable:** It should be possible to handle an enormous number of transactions
  per second which will enable the system to charge low fees. The gas limit should
  scale up to and even past the gas limit on L1.
- **Modular:** Components are modular, reducing complexity and enable parallel contributions.
  Robust conceptual frameworks and composable atoms of software enables complex protocol design
  that exceeds the knowledge base of any single contributor.
- **Minimal:** Rollups should be minimal by using Ethereum’s battle-tested infrastructure. An ideal
  optimistic rollup design should be representable as a _diff_ against Ethereum client software.
- **Developer Driven:** Designs are driven by technical contributors so developers are
  stakeholders. There should be a tight feedback loop between protocol design and developer use.
- **Clear and Readable:** The specs are articulated well. Iterative technical feedback is key!
- **Secure:** Every component of the system is incredibly secure and highly redundant. Assets
  are at stake, so the design must be robust to mitigate risk.
- **Decentralizable:** Designed to avail the protocol of the security and censorship-resistant
  guarantees achieved by a decentralized system.
  Currently centralized components of the system should have a clear path towards decentralization.
  Already decentralized components of the system should be protected and preserved.
