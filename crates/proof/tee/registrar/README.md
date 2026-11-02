# base-proof-tee-registrar

Library crate for the prover registrar service.

Implements automated discovery and onchain registration of TEE prover signer
keys for the Base multi-proof system. The registrar polls AWS ALB target groups
to detect new Nitro enclave instances, probes each instance's `readyz` endpoint
(independent of registration-gated `healthz`), fetches their attestation
documents via `enclave_signerAttestation`, validates and plans them locally,
generates P-384 inverse hints, caches certificate chains through `CertManager`,
and submits hinted registration transactions to `TEEProverRegistry` on L1.

Domain terms (active, registerable, orphan, and protected signers; unresolved instances; registration tasks) are defined in [GLOSSARY.md](<GLOSSARY.md>).

## Discovery Cache TTL

When an instance disappears from otherwise successful discovery output or is
reported unhealthy, the registrar preserves its last-known active signers for
`instance_cache_ttl_cycles` cycles. Shorter TTLs can speed up cleanup for
genuinely removed instances but increase exposure to transient AWS/ALB flakes;
longer TTLs protect against flakes but delay real cleanup.

`SignerReconciler` owns this in-process cache. Discovery errors and cancelled
cycles do not age it. From the caller's observations and pending tasks,
`SignerReconciler::step` builds `ProtectedSigners` only when no instance is
unresolved. Orphan cleanup protects active signers plus pending registrations,
including cancelled tasks until they are reaped.

## Modules

- **`service`** — [`RegistrarConfig`] runtime config and lifecycle runner.
- **`error`** — [`RegistrarError`] enum covering all failure modes.
- **`planner`** — [`AttestationPlanner`] for CertManager-oriented registration plans.
- **`hints`** — [`P384Hints`] Agora / `nitro-validator` inverse-transcript generator.
- **`prover`** — [`ProverClient`] JSON-RPC client for polling prover readiness and signer endpoints.
- **`reconciler`** — [`SignerReconciler`] unavailable-instance grace cache and [`ProtectedSigners`] set for orphan cleanup.
- **`signer_manager`** — [`SignerManager`] registration task reconciliation, transaction execution, and orphan cleanup.
- **`registration_tasks`** — [`RegistrationTaskSet`] bookkeeping and completion handling for registration tasks.
- **`traits`** — [`InstanceDiscovery`] and [`EnclaveEndpointClient`] abstractions.
- **`types`** — Core domain types: [`ProverInstance`], [`RegistrationPlan`], [`RegistrationHints`].
