# Prerequisites

## Requirements

You must have the following installed:

- [Foundry](https://book.getfoundry.sh/getting-started/installation)
- [Docker](https://docs.docker.com/get-started/)

You must have the following RPCs available:
- L1 Archive Node
- L1 Consensus (Beacon) Node
- L2 Archive Node
- L2 Rollup Node

If you do not have an L2 OP Geth node + rollup node running for your rollup, you can follow the [node setup instructions](../node-setup.md) to get started. 


## OP Stack Chain

The rest of this section will assume you have an existing OP Stack Chain running. If you do not have one, there are two ways you can get started:

- **Self-hosted.** If you want to run your own OP Stack Chain, please follow [Optimism's tutorial](https://docs.optimism.io/builders/chain-operators/tutorials/create-l2-rollup) first.
- **Rollup-as-a-service providers.** You can also use an existing RaaS provider, such as [Conduit](https://conduit.xyz/) or [Caldera](https://www.caldera.xyz/). But you will need access to the admin keys for your rollup.
