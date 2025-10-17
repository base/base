# Quick Start

This guide will walk you through the steps to deploy OP Succinct for your OP Stack chain. By the end of this guide, you will have a deployed smart contract on L1 that is tracking the state of your OP Stack chain with mock SP1 proofs.

## Prerequisites

- Compatible RPCs. If you don't have these already, see [Node Setup](../advanced/node-setup.md) for more information.
  - L1 Archive Node (`L1_RPC`)
  - L2 Execution Node (`L2_RPC`)
  - L2 Rollup Node (`L2_NODE_RPC`)
- [Foundry](https://book.getfoundry.sh/getting-started/installation)
- [Docker](https://docs.docker.com/get-started/)
- [Rust](https://www.rust-lang.org/tools/install)
- [Just](https://github.com/casey/just?tab=readme-ov-file#installation)

``` admonish info
On Ubuntu, you'll need some system dependencies to run the service: `curl`, `clang`, `pkg-config`,
`libssl-dev`, `ca-certificates`, `git`, `libclang-dev`, and `jq`. You can see the [Dockerfile](https://github.com/succinctlabs/op-succinct/blob/main/validity/Dockerfile#L38) for more details.
```

## Step 1: Set environment variables.

In the root directory, create a file called `.env` and set the following environment variables:

| Parameter | Description |
|-----------|-------------|
| `L1_RPC` | L1 Archive Node. |
| `L2_RPC` | L2 Execution Node (`op-geth`). |
| `L2_NODE_RPC` | L2 Rollup Node (`op-node`). |
| `PRIVATE_KEY` | Private key for the account that will be deploying the contract. |
| `ETHERSCAN_API_KEY` | Etherscan API key for verifying the deployed contracts. |

```admonish info
If your integration requires access to consensus-layer data, set the `L1_BEACON_RPC` (L1 Beacon Node).  
This is optional and not required by default.
```

## Step 2: Deploy an `SP1MockVerifier` for verifying mock proofs

Deploy an `SP1MockVerifier` for verifying mock proofs by running the following command:

```shell
% just deploy-mock-verifier
[⠊] Compiling...
[⠑] Compiling 1 files with Solc 0.8.15
[⠘] Solc 0.8.15 finished in 615.84ms
Compiler run successful!
Script ran successfully.

== Return ==
0: address 0x4cb20fa9e6FdFE8FDb6CE0942c5f40d49c898646

....
```

In these deployment logs, `0x4cb20fa9e6FdFE8FDb6CE0942c5f40d49c898646` is the address of the `SP1MockVerifier` contract.

## Step 3: Deploy the `OPSuccinctL2OutputOracle` contract

This contract is a modification of Optimism's `L2OutputOracle` contract which verifies a proof along with the proposed state root.

First, add the address of the `SP1MockVerifier` contract from the previous step to the `.env` file in the root directory.

| Parameter | Description |
|-----------|-------------|
| `VERIFIER_ADDRESS` | The address of the `SP1MockVerifier` contract. |

Now, you should have the following in your `.env` file:

| Parameter | Description |
|-----------|-------------|
| `L1_RPC` | L1 Archive Node. |
| `L1_BEACON_RPC` | L1 Beacon Node. |
| `L2_RPC` | L2 Execution Node (`op-geth`). |
| `L2_NODE_RPC` | L2 Rollup Node (`op-node`). |
| `PRIVATE_KEY` | Private key for the account that will be deploying the contract. |
| `ETHERSCAN_API_KEY` | Etherscan API key for verifying the deployed contracts. |
| `VERIFIER_ADDRESS` | The address of the `SP1MockVerifier` contract. |

Then, deploy the `OPSuccinctL2OutputOracle` contract by running the following command:

```shell
% just deploy-oracle    
...

== Return ==
0: address 0xde4656D4FbeaC0c0863Ab428727e3414Fa251A4C
```

In these deployment logs, `0xde4656D4FbeaC0c0863Ab428727e3414Fa251A4C` is the address of the proxy for the `OPSuccinctL2OutputOracle` contract. This deployed proxy contract is used to track the verified state roots of the OP Stack chain on L1.

## Step 4: Set `op-succinct` service environment variables

To start the mock `op-succinct` service, add the following parameters to the `.env` file in the root directory:

| Parameter | Description |
|-----------|-------------|
| `L2OO_ADDRESS` | The address of the `OPSuccinctL2OutputOracle` contract from the previous step. |
| `OP_SUCCINCT_MOCK` | When set to `true`, the `op-succinct` service will generate mock proofs. For this quick start guide, set to `true`. |

Now, you should have the following in your `.env` file:

| Parameter | Description |
|-----------|-------------|
| `L1_RPC` | L1 Archive Node. |
| `L1_BEACON_RPC` | L1 Beacon Node. |
| `L2_RPC` | L2 Execution Node (`op-geth`). |
| `L2_NODE_RPC` | L2 Rollup Node (`op-node`). |
| `PRIVATE_KEY` | Private key for the account that will be deploying the contract and relaying proofs on-chain. |
| `ETHERSCAN_API_KEY` | Etherscan API key for verifying the deployed contracts. |
| `L2OO_ADDRESS` | The address of the `OPSuccinctL2OutputOracle` contract from the previous step. |
| `OP_SUCCINCT_MOCK` | When set to `true`, the `op-succinct` service will generate mock proofs. For this quick start guide, set to `true`. |

``` admonish info
When running just the proposer, you won't need the `ETHERSCAN_API_KEY` or `VERIFIER_ADDRESS` environment variables. These are only required for contract deployment.
```

## Step 5: Start the `op-succinct` service in mock mode.

We provide a Docker Compose file for running the `op-succinct` service.

#### Build the Docker Compose setup.

```shell
docker compose build
```

#### Run the Proposer

This command launches the [op-succinct](./proposer.md) in the background.

After a few minutes, you should see the OP Succinct proposer start to generate mock range proofs. Once enough range proofs have been generated, a mock aggregation proof will be created and submitted to the L1.

```shell
docker compose up
```

To see the logs of the `op-succinct` service, run:

```shell
docker compose logs -f
```

To stop the `op-succinct` service, run:

```shell
docker compose stop
```

