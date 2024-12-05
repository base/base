# Full OP Succinct

Running OP Succinct in full mode will generate proofs of valid OP Stack L2 outputs and submit them to the L1.

## Prerequisites

You will need a whitelisted key on the Succinct Prover Network. Follow the instructions [here](https://docs.succinct.xyz/generating-proofs/prover-network) to get your key whitelisted.

To get access to the Succinct Prover Network for OP Succinct, fill out this [form](https://docs.google.com/forms/d/e/1FAIpQLSd2Yil8TrU54cIuohH1WvDvbxTusyqh5rsDmMAtGC85-Arshg/viewform?ref=https://succinctlabs.github.io/op-succinct/). The Succinct team will reach out to you with an RPC endpoint you can use.

## Overview

### 1) Contract deployment environment variables

In the root directory, create a file called `.env` and set the following environment variables:

| Parameter | Description |
|-----------|-------------|
| `L1_RPC` | L1 Archive Node. |
| `L1_BEACON_RPC` | L1 Beacon Node. |
| `L2_RPC` | L2 Execution Node (`op-geth`). |
| `L2_NODE_RPC` | L2 Rollup Node (`op-node`). |
| `PRIVATE_KEY` | Private key for the account that will be deploying the contract. |
| `ETHERSCAN_API_KEY` | Etherscan API key for verifying the deployed contracts. |

There are additional optional parameters that you can set in the `.env` file. See the [Advanced Parameters](../advanced/l2-output-oracle.md#optional-advanced-parameters) section for more information.

### 2) Deploy the `OPSuccinctL2OutputOracle` contract.

This contract is a modification of the `L2OutputOracle` contract which verifies a proof along with the proposed state root.

Then, deploy the `OPSuccinctL2OutputOracle` contract.

```shell
just deploy-oracle
```

If successful, you should see the following output:

```shell
% just deploy-oracle    
warning: op-succinct-scripts@0.1.0: fault-proof built with release-client-lto profile
warning: op-succinct-scripts@0.1.0: range built with release-client-lto profile
warning: op-succinct-scripts@0.1.0: native_host_runner built with release profile
    Finished `release` profile [optimized] target(s) in 9.00s
     Running `target/release/fetch-rollup-config --env-file .env`
[⠊] Compiling...
[⠘] Compiling 2 files with Solc 0.8.15
[⠃] Solc 0.8.15 finished in 1.72s
Compiler run successful!
Script ran successfully.

== Return ==
0: address 0xde4656D4FbeaC0c0863Ab428727e3414Fa251A4C

## Setting up 1 EVM.

==========================

Chain 11155111

Estimated gas price: 1.57226658 gwei

Estimated total gas used for script: 2746913

Estimated amount required: 0.00431887950806754 ETH

==========================
```

In these deployment logs, `0xde4656D4FbeaC0c0863Ab428727e3414Fa251A4C` is the address of the proxy for the `OPSuccinctL2OutputOracle` contract. This deployed proxy contract is used to track the verified state roots of the OP Stack chain on L1.

### 3) `op-succinct` service environment variables

To start the `op-succinct` service, add the following parameters to the `.env` file in the root directory:

| Parameter | Description |
|-----------|-------------|
| `L2OO_ADDRESS` | The address of the `OPSuccinctL2OutputOracle` contract from the previous step. |
| `SP1_PRIVATE_KEY` | The private key for the account that will be submitting proofs to the L1. |
| `PROVER_NETWORK_RPC` | The RPC endpoint for the Succinct Prover Network. The default endpoint (`https://rpc.succinct.xyz`) is not suitable for use in OP Succinct. Reach out to the Succinct team to get access with OP Succinct. |

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
| `SP1_PRIVATE_KEY` | The private key for the account that will be submitting proofs to the L1. |
| `PROVER_NETWORK_RPC` | Reach out to the Succinct team to get access [here](https://docs.google.com/forms/d/e/1FAIpQLSd2Yil8TrU54cIuohH1WvDvbxTusyqh5rsDmMAtGC85-Arshg/viewform?ref=https://succinctlabs.github.io/op-succinct/). The default endpoint (`https://rpc.succinct.xyz`) is not suitable for use in OP Succinct. |

### 4) Start the `op-succinct` service.

We provide a Docker Compose file for running the `op-succinct` service.

#### Build the Docker Compose setup.

```shell
docker compose build
```

#### Run the Proposer

This command launches the [op-succinct service](../advanced/proposer.md) in the background. It launches two containers: one container that manages proof generation and another container that is a small fork of the original op-proposer service.

After a few minutes, you should see the op-succinct-proposer service start to request proofs from the Succinct Prover Network. Once enough proofs have been generated, an aggregate proof will be requested and submitted to the L1.

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

