# Verify the OP Succinct binaries

When deploying OP Succinct in production, the verification process relies on deterministic builds, so it's important to ensure that the SP1 programs used when generating proofs are reproducible.

Recall there are two programs used in OP Succinct:

-   `range`
    -   Proves the correctness of an OP Stack derivation + STF for a range of blocks.
-   `aggregation`
    -   Aggregates multiple range proofs into a single proof. This is the proof that lands on-chain. The aggregation proof ensures that all `range` proofs in a given block range are linked and use the `rangeVkeyCommitment` from the `L2OutputOracleProxy` as the verification key.

Anything that changes these programs requires reproducing the binaries. This includes changes to the program logic or bumping program dependencies.

## Prerequisites

Ensure you have the [`cargo prove`](https://docs.succinct.xyz/docs/sp1/getting-started/install#option-1-prebuilt-binaries-recommended) CLI tool installed and have the latest version of the toolchain by running:

```bash
sp1up
```

and

```bash
cargo prove --version
```

Also, make sure Docker is running:

```bash
docker ps
```

## Reproduce the program binaries

To build the binaries, run:

```bash
# Build the range elfs
cd programs/range/ethereum
cargo prove build --output-directory ../../../elf --elf-name range-elf-bump --docker --tag v5.1.0
cargo prove build --output-directory ../../../elf --elf-name range-elf-embedded --docker --tag v5.1.0 --features embedded

cd ../celestia
cargo prove build --output-directory ../../../elf --elf-name celestia-range-elf-embedded --docker --tag v5.1.0 --features embedded

# Build the aggregation-elf
cd ../../aggregation
cargo prove build --output-directory ../../elf --elf-name aggregation-elf --docker --tag v5.1.0
```

The updated binaries will be saved in the [`/elf`](https://github.com/succinctlabs/op-succinct/tree/main/elf) directory.

## Verify the program binaries

To verify the binaries, run:

```bash
cargo run --bin config --release
```

This will log the rollup config hash, aggregation verification key, and range verification key commitment based on the latest ELFs in the [`/elf`](https://github.com/succinctlabs/op-succinct/tree/main/elf) directory.

## Update the contract

After reproducing the binaries, you must either [deploy](/validity/contracts/deploy.md) a new OPSuccinctL2OutputOracle contract or perform a [rolling update](/validity/contracts/update-parameters.md) on an existing contract.
