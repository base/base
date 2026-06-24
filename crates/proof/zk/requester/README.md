# ZK Proof Requester

Utilities for submitting higher-level ZK proof request flows to the prover service.

The crate wraps the raw prover-service requester client with ZK-specific request
composition. It does not execute proofs locally, configure workers, or replace
`base-prover-service-client`.
