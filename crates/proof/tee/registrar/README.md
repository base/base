# base-proof-tee-registrar

Library crate for the prover registrar service.

Implements automated discovery and onchain registration of TEE prover signer
keys for the Base multi-proof system. The registrar polls AWS ALB target groups
to detect new Nitro enclave instances, probes each instance's `readyz` endpoint
(independent of registration-gated `healthz`), fetches their attestation
documents via `enclave_signerAttestation`, generates ZK proofs via the Boundless
Network (RISC Zero / Automata SDK), and submits registration transactions to
`TEEProverRegistry` on L1.

## Discovery Cache TTL

When an instance disappears from otherwise successful discovery output or is
reported unhealthy, the registrar preserves its last-known active signers for
`instance_cache_ttl_cycles` cycles. Shorter TTLs can speed up cleanup for
genuinely removed instances but increase exposure to transient AWS/ALB flakes;
longer TTLs protect against flakes but delay real cleanup.

## Modules

- **`service`** — [`RegistrarConfig`] runtime config and lifecycle runner.
- **`error`** — [`RegistrarError`] enum covering all failure modes.
- **`planner`** — [`AttestationPlanner`] for CertManager-oriented registration plans.
- **`hints`** — [`P384Hints`] Agora / `nitro-validator` inverse-transcript generator (unused by the Boundless path until hinted orchestration).
- **`prover`** — [`ProverClient`] JSON-RPC client for polling prover readiness and signer endpoints.
- **`signer_manager`** — [`SignerManager`] lifecycle management for signer proof tasks and registration execution.
- **`traits`** — [`InstanceDiscovery`] and attestation proof provider trait usage.
- **`types`** — Core domain types: [`ProverInstance`], [`RegistrationPlan`], [`RegistrationHints`].
