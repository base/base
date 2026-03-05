# `base-proof-contracts`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Shared onchain contract bindings for OP Stack dispute game management.

## Overview

Provides async client traits and concrete Alloy-backed implementations for:

- **`DisputeGameFactoryClient`**: Creating new dispute games and querying existing ones.
- **`AnchorStateRegistryClient`**: Reading the anchor state (latest finalized output root).
- **`AggregateVerifierClient`**: Querying individual dispute game instances and reading onchain configuration (`BLOCK_INTERVAL`, `INTERMEDIATE_BLOCK_INTERVAL`).

Also provides shared data types (`GameAtIndex`, `AnchorRoot`, `GameInfo`), pure encoding/decoding helpers
(`encode_extra_data`, `decode_extra_data`, `encode_create_calldata`), and a `ContractError` type for error handling.

These bindings are used by both [`base-proposer`](../proposer/) and the challenger.

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
