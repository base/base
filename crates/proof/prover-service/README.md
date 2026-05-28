# Base Prover Service

Shared protobuf definitions for the generic prover service API.

This crate owns the prover service proto that supports structured proof
requests, TEE proof payloads, and lease-based proof job RPCs. It is separate
from the existing ZK prover service so the current ZK API can remain intact
while the generic prover service evolves independently.
