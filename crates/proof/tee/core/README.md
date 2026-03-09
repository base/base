# `base-enclave`

Core types for the enclave.

## Overview

Provides Rust equivalents of the Go types used in the TEE enclave, with JSON serialization that
matches the Go `encoding/json` output exactly. Includes `ExecutionResult` (state root and receipt
hash), `ExecutionWitness` (block headers, code, and state trie nodes), `TransformedWitness`
(integrity-checked witness), and stateless execution helpers (`execute_stateless`,
`execute_block`). Also provides output root computation (`output_root_v0`), rollup/chain
configuration types, and proof encoding.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-enclave = { workspace = true }
```

```rust,ignore
use base_enclave::{execute_stateless, ExecutionWitness, output_root_v0};

let result = execute_stateless(&rollup_config, witness, l1_origin)?;
let output_root = output_root_v0(&header, storage_root);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
