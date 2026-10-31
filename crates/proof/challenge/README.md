# `base-challenger`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

ZK-proof dispute game challenger.

See the [challenger glossary](GLOSSARY.md) for proof and dispute lifecycle terms.

## Overview

- **Driver**: Main polling loop that ties together scanning, validation, proof requests, and dispute submission.
- **Scanner**: Scans the `DisputeGameFactory` for `IN_PROGRESS` dispute games and classifies each into a `GameCategory` (`InvalidTeeProposal`, `FraudulentZkChallenge`, `InvalidZkProposal`, or `InvalidDualProposal`) based on prover state.
- **Validator**: Verifies final and intermediate output roots for each `CandidateGame` by fetching L2 block headers and `L2ToL1MessagePasser` storage proofs, recomputing expected roots via `OutputRoot`, and comparing them against onchain claims.
- **Dispute attempt**: Models one game's proof and dispute lifecycle across TEE and ZK proof sources, including ZK fallback, without I/O. Not yet wired into the driver.
- **Pending**: Tracks in-flight proof sessions through a `ProofPhase` lifecycle (`AwaitingProof` → `ReadyToSubmit` or `NeedsRetry`).
- **Submitter**: Submits dispute transactions onchain (`nullify()` / `challenge()`) for invalid games.
- **Bond**: Resolves dispute games, unlocks bond credits, and withdraws them once the withdrawal delay has elapsed.
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
