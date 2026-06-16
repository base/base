# base-proof-zk-host

Host-side ZK proving worker for the prover service.

This crate provides the worker machinery that lets a ZK proving host pull jobs
from the prover service over the JSON-RPC worker API and submit results back. It
mirrors the structure of the TEE host (`base-proof-tee-nitro-host`):

- [`JobDiscovery`] polls `getNextProof` for ZK jobs and dispatches claimed jobs
  with bounded concurrency.
- [`ProofGenerator`] drives proof generation for a claimed job, heartbeating the
  claim while the proof is produced, then hands the result to the submitter.
- [`ProofSubmitter`] delivers the proof result via `submitProof`, retrying
  retryable failures with exponential backoff.

The actual SP1 proof generation is abstracted behind the [`ZkProver`] trait. This
crate ships [`UnimplementedZkProver`], a stub that returns an error; a real SP1
"prove-to-completion" implementation that returns proof bytes is wired in
separately.

[`JobDiscovery`]: crate::JobDiscovery
[`ProofGenerator`]: crate::ProofGenerator
[`ProofSubmitter`]: crate::ProofSubmitter
[`ZkProver`]: crate::ZkProver
[`UnimplementedZkProver`]: crate::UnimplementedZkProver
