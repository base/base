# Quick Start

This guide will walk you through the steps to deploy OP Succinct for your OP Stack chain. By the end of this guide, you will have a deployed smart contract on L1 that is tracking the state of your OP Stack chain with mock SP1 proofs and Ethereum as the data availability layer.

```admonish note
For OP Stack chain with alternative DA layers, please refer to the [Experimental Features](./experimental/experimental.md) section.
```

## Prerequisites

- Compatible RPCs. If you don't have these already, see [Node Setup](../advanced/node-setup.md) for more information.
  - L1 Archive Node (`L1_RPC`)
  - L2 Execution Node (`L2_RPC`)
  - L2 Rollup Node (`L2_NODE_RPC`)
- [Foundry](https://book.getfoundry.sh/getting-started/installation)
- [Docker](https://docs.docker.com/get-started/)
- [Rust](https://www.rust-lang.org/tools/install)
- [Just](https://github.com/casey/just?tab=readme-ov-file#installation)

```` admonish info
On Ubuntu, you'll need some system dependencies to run the service:

```bash
sudo apt update && sudo apt install -y \
  curl clang pkg-config libssl-dev ca-certificates git libclang-dev jq build-essential
```
````

## Step 1: Clone and build contracts

Clone the repository and build the contracts:

```bash
git clone https://github.com/succinctlabs/op-succinct.git
cd op-succinct/contracts
forge build
cd ..
```

## Step 2: Configure environment

In the root directory, create a file called `.env` and set the following environment variables:

```env
# Required
L1_RPC=<YOUR_L1_RPC_URL>                   # L1 Archive Node
L2_RPC=<YOUR_L2_RPC_URL>                   # L2 Execution Node (op-geth)
L2_NODE_RPC=<YOUR_L2_NODE_RPC_URL>         # L2 Rollup Node (op-node)
PRIVATE_KEY=<YOUR_PRIVATE_KEY>             # Private key for deploying contracts
ETHERSCAN_API_KEY=<YOUR_ETHERSCAN_API_KEY> # For verifying deployed contracts

# Optional
L1_BEACON_RPC=<YOUR_L1_BEACON_RPC_URL>     # L1 Beacon Node (for integrations that access consensus-layer data)
```

```admonish info
Obtaining a Test Private Key

- Anvil (local devnet): Run `anvil` and use one of the pre-funded accounts printed on startup. Copy the Private Key value for any account. Only use these on your local Anvil network.

- Foundry (generate a fresh key): Run `cast wallet new` to generate a human-readable output. Save the private key and fund it on your test network.

⚠️ **Caution:** Never use test keys on mainnet or with real assets.
```

## Step 3: Deploy contracts

### Deploy an `SP1MockVerifier` for verifying mock proofs

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

Add the address to your `.env` file:

```env
VERIFIER_ADDRESS=<SP1_MOCK_VERIFIER_ADDRESS>  # Address of the SP1MockVerifier contract
```

### Deploy the `OPSuccinctL2OutputOracle` contract

This contract is a modification of Optimism's `L2OutputOracle` contract which verifies a proof along with the proposed state root.

Deploy the contract by running the following command. This command automatically fetches and stores the rollup configuration, loads the required environment variables, and executes the Foundry deployment script, optionally verifying the contract on Etherscan:

```shell
% just deploy-oracle
...

== Return ==
0: address 0xde4656D4FbeaC0c0863Ab428727e3414Fa251A4C
```

In these deployment logs, `0xde4656D4FbeaC0c0863Ab428727e3414Fa251A4C` is the address of the proxy for the `OPSuccinctL2OutputOracle` contract. This deployed proxy contract is used to track the verified state roots of the OP Stack chain on L1.

## Step 4: Run the service

Add the following parameters to the `.env` file:

```env
L2OO_ADDRESS=<OPSUCCINCT_L2_OUTPUT_ORACLE_ADDRESS>  # Address of the OPSuccinctL2OutputOracle proxy
OP_SUCCINCT_MOCK=true                               # Set to true to generate mock proofs
```

``` admonish info
When running just the proposer, you won't need the `ETHERSCAN_API_KEY` or `VERIFIER_ADDRESS` environment variables. These are only required for contract deployment.
```

### Build the Docker Compose setup

```shell
docker compose build
```

### Run the Proposer

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
