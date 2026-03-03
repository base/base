# `base-proof-common`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Shared on-chain contract bindings for OP Stack dispute game management.

Provides async client traits and concrete Alloy-backed implementations for:

- **`DisputeGameFactory`**: Creating new dispute games and querying existing ones.
- **`AnchorStateRegistry`**: Reading the anchor state (latest finalized output root).
- **`AggregateVerifier`**: Querying individual dispute game instances.

These bindings are used by both the proposer and challenger.

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
