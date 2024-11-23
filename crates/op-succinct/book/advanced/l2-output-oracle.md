# OP Succinct L2 Output Oracle

## Overview

The `OPSuccinctL2OutputOracle` contract is a modification of the `L2OutputOracle` contract that will verify SP1 proofs of the Optimism state transition function to get fully validity-proven state roots for the OP Stack rollup.

### Modifications to Original `L2OutputOracle`

The original `L2OutputOracle` contract can be found [here](https://github.com/ethereum-optimism/optimism/blob/develop/packages/contracts-bedrock/src/L1/L2OutputOracle.sol).

The changes introduced in the `OPSuccinctL2OutputOracle` contract are:

1. The `submissionInterval` parameter is now the minimum interval in L2 blocks at which checkpoints must be submitted. An aggregation proof can be posted after this interval has passed.
2. The addition of the `aggregationVkey`, `rangeVkeyCommitment`, `verifierGateway`, `startingOutputRoot`, and `rollupConfigHash` parameters. `startingOutputRoot` is used for initalizing the contract from an empty state, because `op-succinct` requires a starting output root from which to prove the next state root. The other parameters are used for verifying the proofs posted to the contract.
3. The addition of `historicBlockHashes` to store the L1 block hashes which the `op-succinct` proofs are anchored to. Whenever a proof is posted, the merkle proof verification will use these L1 block hashes to verify the state of the L2 which is posted as blobs or calldata to the L1.
4. The new `checkpointBlockHash` function which checkpoints the L1 block hash at a given L1 block number using the `blockhash` function.
5. The `proposeL2Output` function now takes an additional `_proof` parameter, which is the proof that is posted to the contract, and removes the unnecessary `_l1BlockHash` parameter (which is redundant given the `historicBlockHashes` mapping). This function also verifies the proof using the `ISP1VerifierGateway` contract.

## Configuration

Inside the `contracts` folder there is a file called `opsuccinctl2ooconfig.json` that contains the parameters for the deployment. When `just deploy-oracle` and `just upgrade-oracle` are invoked, the `opsuccinctl2ooconfig.json` is automatically updated based on the OP Stack chain you are using and the advanced parameters in your `.env` file.

### Required Parameters

When deploying the `OPSuccinctL2OutputOracle` contract, the following parameters are required to be set in your `.env` file:

| Parameter | Description |
|-----------|-------------|
| `L1_RPC` | L1 Archive Node. |
| `L1_BEACON_RPC` | L1 Consensus (Beacon) Node. |
| `L2_RPC` | L2 Execution Node (`op-geth`). |
| `L2_NODE_RPC` | L2 Rollup Node (`op-node`). |
| `PRIVATE_KEY` | Private key for the account that will be deploying the contract. |
| `ETHERSCAN_API_KEY` | Etherscan API key used for verifying the contract (optional). |

### Optional Advanced Parameters

You can configure additional parameters when deploying or upgrading the `OPSuccinctL2OutputOracle` contract in your `.env` file.

| Parameter | Description |
|-----------|-------------|
| `VERIFIER_ADDRESS` | Default: Succinct's official Groth16 VerifierGateway. Address of the `ISP1VerifierGateway` contract used to verify proofs. For mock proofs, this is the address of the `SP1MockVerifier` contract. |
| `STARTING_BLOCK_NUMBER` | Default: The finalized block number on L2. The block number to initialize the contract from. OP Succinct will start proving state roots from this block number. |
| `SUBMISSION_INTERVAL` | Default: `1000`. The minimum interval in L2 blocks at which checkpoints must be submitted. An aggregation proof can be posted for any range larger than this interval. |
| `FINALIZATION_PERIOD` | Default: `0`. The time period (in seconds) after which a proposed output becomes finalized and withdrawals can be processed. |
| `PROPOSER` | Default: The address of the account associated with `PRIVATE_KEY`. Ethereum address authorized to submit proofs. Set to `address(0)` to allow permissionless submissions. |
| `CHALLENGER` | Default: `address(0)`, no one can dispute proofs. Ethereum address authorized to dispute proofs. |

## Upgrading `OPSuccinctL2OutputOracle`

The last step is to update your OP Stack configuration to use the new `OPSuccinctL2OutputOracle` contract managed by the `op-succinct` service.

> ⚠️ **Caution**: When upgrading to the `OPSuccinctL2OutputOracle` contract, maintain the existing `finalizationPeriod` for a duration equal to at least one `finalizationPeriod`. Failure to do so will result in immediate finalization of all pending output roots upon upgrade, which is unsafe. Only after this waiting period has elapsed should you set the `finalizationPeriod` to 0.

### Upgrading with an EOA `ADMIN` key

To update the `L2OutputOracle` implementation with an EOA `ADMIN` key, run the following command in `/contracts`.

```bash
just upgrade-oracle
```

### Upgrading with a non-EOA `ADMIN` key

If the owner of the `L2OutputOracle` is not an EOA (e.g. multisig, contract), set `EXECUTE_UPGRADE_CALL` to `false` in your `.env` file. This will output the raw calldata for the upgrade call, which can be executed by the owner in a separate context.

| Parameter | Description |
|-----------|-------------|
| `EXECUTE_UPGRADE_CALL` | Set to `false` to output the raw calldata for the upgrade call. |

Then, run the following command in `/contracts`.

```bash
just upgrade-oracle
```

```shell
% just upgrade-oracle
warning: op-succinct-scripts@0.1.0: fault-proof built with release-client-lto profile
warning: op-succinct-scripts@0.1.0: range built with release-client-lto profile
warning: op-succinct-scripts@0.1.0: native_host_runner built with release profile
    Finished `release` profile [optimized] target(s) in 0.35s
     Running `target/release/fetch-rollup-config --env-file .env`
[⠊] Compiling...
Script ran successfully.

== Logs ==
  The calldata for upgrading the contract with the new initialization parameters is:
  0x4f1ef2860000000000000000000000007f5d6a5b55ee82090aedc859b40808103b30e46900000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000184c0e8e2a100000000000000000000000000000000000000000000000000000000000004b00000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000135161100000000000000000000000000000000000000000000000000000000674107ce000000000000000000000000ded0000e32f8f40414d3ab3a830f735a3553e18e000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001346d7ea10cc78c48e3da6f1890bb16cf27e962202f0f9b2c5c57f3cfb0c559ec1dca031e9fc246ec47246109ebb324a357302110de5d447af13a07f527620000000000000000000000004cb20fa9e6fdfe8fdb6ce0942c5f40d49c8986469ad6f24abc0df5e4cab37c1efae21643938ed5393389ce9e747524a59546a8785e32d5f9f902c6a46cb75cbdb083ea67b9475d7026542a009dc9d99072f4bdf100000000000000000000000000000000000000000000000000000000

## Setting up 1 EVM.
```
