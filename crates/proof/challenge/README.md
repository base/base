# `base-challenger`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

ZK-proof dispute game challenger.

## Overview

- **Driver**: Main polling loop that ties together scanning, validation, proof requests, and dispute submission.
- **Scanner**: Scans the `DisputeGameFactory` for `IN_PROGRESS` dispute games and classifies each into a `GameCategory` (`InvalidTeeProposal`, `FraudulentZkChallenge`, or `InvalidZkProposal`) based on prover state.
- **Validator**: Verifies final and intermediate output roots for each `CandidateGame` by fetching L2 block headers and `L2ToL1MessagePasser` storage proofs, recomputing expected roots via `OutputRoot`, and comparing them against onchain claims.
- **Pending**: Tracks in-flight proof sessions through a `ProofPhase` lifecycle (`AwaitingProof` → `ReadyToSubmit` or `NeedsRetry`).
- **Submitter**: Submits dispute transactions onchain (`nullify()` / `challenge()`) for invalid games.
- **Bond**: Multi-phase bond credit claim lifecycle (`NeedsResolve` → `NeedsUnlock` → `AwaitingDelay` → `NeedsWithdraw`).
- **Tee**: `L1HeadProvider` trait for resolving L1 block numbers from hashes.
- **Verify**: Account proof verification against a state root using Merkle Patricia Trie proofs.
- **Error**: Challenge submission error types.
- **Service**: Full challenger service lifecycle (init, wiring, driver loop, shutdown).
- **Config**: Configuration types and validation.
- **CLI**: CLI argument definitions (`BASE_CHALLENGER_` env-var prefix).
- **Metrics**: Prometheus metric definitions.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-challenger = { workspace = true }
```

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
