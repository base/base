# Prerequisites

## Requirements

You must have the following installed:

- [Foundry](https://book.getfoundry.sh/getting-started/installation)
- [Docker](https://docs.docker.com/get-started/)

You must have the following RPCs available:
- L1 Archive Node
- L1 Consensus (Beacon) Node
- L2 Execution Node (`op-geth`)
- L2 Rollup Node (`op-node`)

The following RPC endpoints must be accessible:

- L1 Archive Node.
  - `debug_getRawHeader`, `debug_getRawReceipts`, `debug_getRawBlock`
- L2 Execution Node (`op-geth`): Archive node with hash state scheme.
  - `debug_getRawHeader`, `debug_getRawTransaction`, `debug_getRawBlock`, `debug_getExecutionWitness`, `debug_dbGet`
- L2 Optimism Node (`op-node`)
  - `optimism_outputAtBlock`, `optimism_rollupConfig`, `optimism_syncStatus`, `optimism_safeHeadAtL1Block`.

If you do not have access to an L2 OP Geth node + rollup node for your OP Stack chain, you can follow the [L2 node setup instructions](../advanced/node-setup.md) to spin them up.

## OP Stack Chain

The rest of this section will assume you have an existing OP Stack Chain running. If you do not have one, there are two ways you can get started:

- **Self-hosted.** If you want to run your own OP Stack Chain, please follow [Optimism's tutorial](https://docs.optimism.io/builders/chain-operators/tutorials/create-l2-rollup) first.
- **Rollup-as-a-service providers.** You can also use an existing RaaS provider, such as [Conduit](https://conduit.xyz/), [Caldera](https://www.caldera.xyz/), [Alchemy](https://www.alchemy.com/contact-sales-rollups), [AltLayer](https://www.altlayer.io/raas) or [Gelato](https://www.gelato.network/raas). Contact them to upgrade your rollup to use OP Succinct.
