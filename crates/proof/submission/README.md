# `base-proof-submission`

Shared proof submission helpers for Base dispute games.

## Overview

This crate contains the reusable pieces needed to attach proof bytes to an
existing `AggregateVerifier` dispute game:

- Submitting `AggregateVerifier.verifyProposalProof(bytes)` through the shared transaction manager.
- Classifying known non-retryable revert selectors into structured errors.

Proof byte encoding lives in `base-proof-primitives::ProofEncoder` so callers can
prepare either TEE or ZK proof bytes before using this crate's submission path.

It intentionally does not own proposer or challenger policy. Callers remain
responsible for deciding which game to target, whether a proof should be
attached, and how classified errors affect their higher-level workflow.

## License

[MIT License](https://github.com/base/base/blob/main/LICENSE)
