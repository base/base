# `base-proof-contracts`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Shared onchain contract bindings for Base dispute game management.

## Overview

Provides async client traits and concrete Alloy-backed implementations for:

- **`DisputeGameFactoryClient`**: Creating new dispute games and querying existing ones.
- **`AnchorStateRegistryClient`**: Reading the anchor state (latest finalized output root).
- **`AggregateVerifierClient`**: Querying individual dispute game instances (status, ZK/TEE prover addresses, output roots), reading onchain configuration (`BLOCK_INTERVAL`, `INTERMEDIATE_BLOCK_INTERVAL`), and constructing state-changing calls such as `nullify` via [`encode_nullify_calldata`].

Also provides shared data types (`GameAtIndex`, `AnchorRoot`, `GameInfo`), pure encoding helpers
(`encode_extra_data`, `encode_create_calldata`, `encode_nullify_calldata`), and a `ContractError` type for error handling.

These bindings are used by both [`base-proposer`](../proposer/) and the challenger.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-proof-contracts = { workspace = true }
```

Use the Alloy-backed clients to interact with onchain contracts:

```rust,ignore
use base_proof_contracts::DisputeGameFactoryClient;

let factory = DisputeGameFactoryClient::new(provider, factory_address);
let games = factory.get_all_games(game_type, 0, count).await?;
```

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
