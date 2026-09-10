# `base-proof-execution`

Fault-proof program execution for Base: stateless block execution, derivation driving, witness-oracle access, and verification of the claimed output. Trie witnesses come from [`base-proof-witness-mpt`](../witness/mpt).

The default profile supports `no_std` proof guests. `std` enables host runtime and KZG support; `evm-std` enables the EVM's host functionality without selecting Tokio. `test-utils` adds the database and provider fixtures used by execution tests.

## Overview

Executes Base L2 blocks without maintaining persistent state, using Merkle proof witnesses
to reconstruct the necessary trie nodes on demand. `StatelessL2Builder` takes payload attributes
and a `TrieDB` instance, executes transactions via revm, and returns a `BlockBuildingOutcome`
containing the sealed header, receipts, and verified state root.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-proof-execution = { workspace = true }
```

```rust,ignore
use base_proof_execution::{StatelessL2Builder, TrieDB};

let builder = StatelessL2Builder::new(trie_db, chain_spec);
let outcome = builder.execute(attributes)?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
