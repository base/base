# `base-proof-executor`

A `no_std` implementation of a stateless block executor for the Base rollup, backed by [`base-proof-mpt`](../mpt)'s `TrieDB`.

## Overview

Executes Base L2 blocks without maintaining persistent state, using Merkle proof witnesses
to reconstruct the necessary trie nodes on demand. `StatelessL2Builder` takes payload attributes
and a `TrieDB` instance, executes transactions via revm, and returns a `BlockBuildingOutcome`
containing the sealed header, receipts, and verified state root.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-proof-executor = { workspace = true }
```

```rust,ignore
use base_proof_executor::StatelessL2Builder;

// `provider` serves trie node preimages and `hinter` sends trie hints to the host.
let mut builder =
    StatelessL2Builder::new(&rollup_config, evm_factory, provider, hinter, parent_header);
let outcome = builder.build_block(attributes)?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
