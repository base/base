# `base-challenger`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

ZK-proof dispute game challenger.

## Overview

- **Scanner**: Reads the `DisputeGameFactory` for new dispute games, filtering by `IN_PROGRESS` status and unchallenged state (`zkProver == zero`) to produce `CandidateGame`s for downstream processing.
- **Validator**: Verifies final and intermediate output roots for each `CandidateGame` by fetching L2 block headers and `L2ToL1MessagePasser` storage proofs, recomputing expected roots via `OutputRoot`, and comparing them against onchain claims.
- **Service**: Lifecycle orchestration for the challenger (init, health, metrics, shutdown).
- **Config**: Validated runtime configuration with CLI argument parsing.
- **Health**: HTTP liveness (`/healthz`) and readiness (`/readyz`) probes.
- **Metrics**: Prometheus metric definitions and recording.
- **CLI**: Command-line argument parsing and configuration validation.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-challenger = { workspace = true }
```

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
