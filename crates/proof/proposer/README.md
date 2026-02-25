# `base-proposer`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

TEE-based output proposer for OP Stack chains.

## Overview

- **Contracts**: On-chain contract bindings for dispute game creation and verification.
- **RPC**: Async clients for L1, L2, and rollup node communication with caching.
- **Enclave**: TEE enclave client for stateless block validation and proof aggregation.
- **Prover**: Core prover for generating TEE-signed proposals.
- **Driver**: Coordination loop for proposal generation and submission.
- **Metrics**: Prometheus metric definitions and recording.
- **CLI**: Command-line argument parsing and configuration validation.

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
