# `base-challenger`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

ZK-proof dispute game challenger.

## Overview

- **Scanner**: Reads the `DisputeGameFactory` for new dispute games, filtering by `IN_PROGRESS` status and unchallenged state (`zkProver == zero`) to produce `CandidateGame`s for downstream processing.
- **Validator**: Verifies `CandidateGame` output roots against L2 state. Validates both the final `rootClaim` and intermediate checkpoint roots using the OP Stack v0 output root formula (`keccak256(version ‖ state_root ‖ storage_root ‖ block_hash)`).
- **Service**: Lifecycle orchestration for the challenger (init, health, metrics, shutdown).
- **Config**: Validated runtime configuration with CLI argument parsing.
- **Health**: HTTP liveness (`/healthz`) and readiness (`/readyz`) probes.
- **Metrics**: Prometheus metric definitions and recording.
- **CLI**: Command-line argument parsing and configuration validation.

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
