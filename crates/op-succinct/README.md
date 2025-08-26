# op-succinct

OP Succinct is the production-grade proving engine for the OP Stack, powered by SP1.

With support for both validity proofs, with OP Succinct, and ZK fault proofs, with OP Succinct Lite, OP Succinct enables seamless upgrades for OP Stack rollups to a type-1 zkEVM rollup.

**[Docs](https://succinctlabs.github.io/op-succinct)**

## Repository Overview

> [!CAUTION]
> `main` is the development branch and may contain unstable code.
> For production use, please use the [latest release](https://github.com/succinctlabs/op-succinct/releases).

The repository is organized into the following directories:

- `book`: The documentation for OP Succinct users and developers.
- `contracts`: The solidity contracts for posting state roots to L1.
- `programs`: The programs for proving the execution and derivation of the L2 state transitions and proof aggregation.
- `validity`: The implementation of the `op-succinct/op-succinct` service.
- `fault-proof`: The implementation of the `op-succinct/fault-proof` service.
- `scripts`: Scripts for testing and deploying OP Succinct.
- `utils`: Shared utilities for the host, client, and proposer.

## Development

### Book

Make sure you install the following on your machine:

```bash
cargo install mdbook
cargo install mdbook-mermaid
cargo install mdbook-admonish
```

Then run the server:

```sh
mdbook serve --open
```

### OP Succinct

To configure or change the OP Succinct codebase, please refer to the [OP Succinct Book](https://succinctlabs.github.io/op-succinct).

## Acknowledgments

This repo would not exist without:
* [OP Stack](https://docs.optimism.io/stack/getting-started): Modular software components for building L2 blockchains.
* [Kona](https://github.com/anton-rs/kona/tree/main): A portable implementation of the OP Stack rollup state transition, namely the derivation pipeline and the block execution logic.
* [SP1](https://github.com/succinctlabs/sp1): The fastest, most feature-complete zkVM for developers.
