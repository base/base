# Mock OP Succinct

Running OP Succinct in mock mode is useful for testing your configuration is correct before generating proofs.

## Overview

### 1) Set environment variables.

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

### 2) Deploy an `SP1MockVerifier` for verifying mock proofs

```bash
just deploy-mock-verifier
```

If successful, you should see the following output:

```shell
% just deploy-mock-verifier
[⠊] Compiling...
[⠑] Compiling 1 files with Solc 0.8.15
[⠘] Solc 0.8.15 finished in 615.84ms
Compiler run successful!
Script ran successfully.

== Return ==
0: address 0x4cb20fa9e6FdFE8FDb6CE0942c5f40d49c898646

## Setting up 1 EVM.

==========================

Chain 11155111

Estimated gas price: 3.851705636 gwei

Estimated total gas used for script: 171869

Estimated amount required: 0.000661988795953684 ETH

==========================
....
```

In these deployment logs, `0x4cb20fa9e6FdFE8FDb6CE0942c5f40d49c898646` is the address of the `SP1MockVerifier` contract.


#### Custom Environment

If you have multiple environments, you can pass the environment file to the `deploy-mock-verifier` command.

```bash
just deploy-mock-verifier <env_file>
```

### 3) Deploy the `OPSuccinctL2OutputOracle` contract.

This contract is a modification of the `L2OutputOracle` contract which verifies a proof along with the proposed state root.

First, add the address of the `SP1MockVerifier` contract from the previous step to the `.env` file in the root directory.

| Parameter | Description |
|-----------|-------------|
| `VERIFIER_ADDRESS` | The address of the `SP1MockVerifier` contract. |

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

### 4) Set `op-succinct` service environment variables

To start the mock `op-succinct` service, add the following parameters to the `.env` file in the root directory:

| Parameter | Description |
|-----------|-------------|
| `L2OO_ADDRESS` | The address of the `OPSuccinctL2OutputOracle` contract from the previous step. |
| `OP_SUCCINCT_MOCK` | Set to `true` for mock mode. |

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
| `OP_SUCCINCT_MOCK` | Set to `true` for mock mode. |

### 5) Start the `op-succinct` service in mock mode.

We provide a Docker Compose file for running the `op-succinct` service.

#### Build the Docker Compose setup.

```shell
docker compose build
```

#### Run the Proposer

This command launches the [op-succinct service](../advanced/proposer.md) in the background. It launches two containers: one container that manages proof generation and another container that is a small fork of the original op-proposer service.

After a few minutes, you should see the op-succinct-proposer service start to generate mock range proofs. Once enough range proofs have been generated, a mock aggregate proof will be created and submitted to the L1.

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

