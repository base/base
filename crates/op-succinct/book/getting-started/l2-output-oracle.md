# Deploy OP Succinct L2 Output Oracle

The first step in deploying OP Succinct is to deploy the `OPSuccinctL2OutputOracle` smart contract that will verify SP1 proofs of the Optimism state transition function which verify the latest state root for the OP Stack rollup.

## Overview

The `OPSuccinctL2OutputOracle` contract is a modification of the `L2OutputOracle` contract that is used to verify the state roots of the OP Stack rollup.

### Modifications to `L2OutputOracle`

The original `L2OutputOracle` contract can be found [here](https://github.com/ethereum-optimism/optimism/blob/3e68cf018d8b9b474e918def32a56d1dbf028d83/packages/contracts-bedrock/src/L1/L2OutputOracle.sol#L199-L202).

The changes introduced in the `OPSuccinctL2OutputOracle` contract are:

1. The `submissionInterval` parameter is now the minimum interval in L2 blocks at which checkpoints must be submitted. An aggregation proof can be posted after this interval has passed.
2. The addition of the `aggregationVkey`, `rangeVkeyCommitment`, `verifierGateway`, `startingOutputRoot`, and `rollupConfigHash` parameters. `startingOutputRoot` is used for initalizing the contract from an empty state, because `op-succinct` requires a starting output root from which to prove the next state root. The other parameters are used for verifying the proofs posted to the contract.
3. The addition of `historicBlockHashes` to store the L1 block hashes which the `op-succinct` proofs are anchored to. Whenever a proof is posted, the merkle proof verification will use these L1 block hashes to verify the state of the L2 which is posted as blobs or calldata to the L1.
4. The new `checkpointBlockHash` function which checkpoints the L1 block hash at a given L1 block number using the `blockhash` function.
5. The `proposeL2Output` function now takes an additional `_proof` parameter, which is the proof that is posted to the contract, and removes the unnecessary `_l1BlockHash` parameter (which is redundant given the `historicBlockHashes` mapping). This function also verifies the proof using the `ISP1VerifierGateway` contract.

## Deployment

### 1) Clone `op-succinct` repo:

```bash
git clone https://github.com/succinctlabs/op-succinct.git
cd op-succinct
```

### 2) Set environment variables:

In the root directory, create a file called `.env` (mirroring `.env.example`) and set the following environment variables:

| Parameter | Description |
|-----------|-------------|
| `L1_RPC` | L1 Archive Node. |
| `L1_BEACON_RPC` | L1 Consensus (Beacon) Node. |
| `L2_RPC` | L2 Execution Node (`op-geth`). |
| `L2_NODE_RPC` | L2 Rollup Node (`op-node`). |
| `PROVER_NETWORK_RPC` | Default: `rpc.succinct.xyz`. |
| `SP1_PRIVATE_KEY` | Key for the Succinct Prover Network. Get access [here](https://docs.succinct.xyz/generating-proofs/prover-network). |
| `SP1_PROVER` | Default: `network`. Set to `network` to use the Succinct Prover Network. |
| `PRIVATE_KEY` | Private key for the account that will be deploying the contract and posting output roots to L1. |

### 3) Navigate to the contracts directory:

```bash
cd contracts
```

### 4) Set Deployment Parameters

Inside the `contracts` folder there is a file called `opsuccinctl2ooconfig.json` that contains the parameters for the deployment. The parameters are automatically set based on your RPC's and the owner of your contract is determined by the private key you set in the `.env` file.

#### Optional Advanced Parameters

Advanced users can set parameters manually in `opsuccinctl2ooconfig.json`, but the defaults are recommended. Skip this section if you want to use the defaults.

| Parameter | Description |
|-----------|-------------|
| `owner` | Ethereum address of the contract owner. Default: The address of the account associated with `PRIVATE_KEY`. |
| `proposer` | Ethereum address authorized to submit proofs. Default: The address of the account associated with `PRIVATE_KEY`. |
| `challenger` | Ethereum address authorized to dispute proofs. Default: `address(0)`, no one can dispute proofs. |
| `finalizationPeriod` | The time period (in seconds) after which a proposed output becomes finalized and withdrawals can be processed. Default: `0`. |

### 5) Deploy the `OPSuccinctL2OutputOracle` contract:

Run the following command to deploy the `OPSuccinctL2OutputOracle` contract to the L1 chain:

```bash
forge script script/OPSuccinctDeployer.s.sol:OPSuccinctDeployer \
    --rpc-url $L1_RPC \
    --private-key $PRIVATE_KEY \
    --ffi \
    --verify \
    --verifier etherscan \
    --etherscan-api-key $ETHERSCAN_API_KEY \
    --broadcast
```

If successful, you should see the following output:

```
Script ran successfully.

== Return ==
0: address 0x9b520F7d8031d45Eb8A1D9fE911038576931ab95

...

ONCHAIN EXECUTION COMPLETE & SUCCESSFUL.
##
Start verification for (2) contracts
Start verifying contract `0x9b520F7d8031d45Eb8A1D9fE911038576931ab95` deployed on sepolia

Submitting verification for [lib/optimism/packages/contracts-bedrock/src/universal/Proxy.sol:Proxy] 0x9b520F7d8031d45Eb8A1D9fE911038576931ab95.
```

In these deployment logs, `0x9b520F7d8031d45Eb8A1D9fE911038576931ab95` is the address of the Proxy contract for the `OPSuccinctL2OutputOracle`. This deployed Proxy contract will keep track of the state roots of the OP Stack chain.

#### Configure Environment

To use a configurable environment, pass the `ENV_FILE` flag with the path to your `.env` file. By default this is the `.env` in your root directory.

```bash
ENV_FILE=.env.new forge script script/OPSuccinctDeployer.s.sol:OPSuccinctDeployer ...
```

### 6) Add Proxy Address to `.env`

Add the address for the `OPSuccinctL2OutputOracle` proxy contract to the `.env` file in the root directory.

| Parameter | Description |
|-----------|-------------|
| `L2OO_ADDRESS` | The address of the Proxy contract for the `OPSuccinctL2OutputOracle`. |