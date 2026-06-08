# base-proof-tee-registrar

Library crate for the prover registrar service.

Implements automated discovery and onchain registration of TEE prover signer
keys for the Base multi-proof system. The registrar polls AWS ALB target groups
to detect new Nitro enclave instances, fetches their attestation documents via
`enclave_signerAttestation`, generates ZK proofs via the Boundless Network
(RISC Zero / Automata SDK), and submits registration transactions to
`TEEProverRegistry` on L1.

## Modules

- **`config`** — [`RegistrarConfig`] runtime config struct, [`BoundlessConfig`],
  and [`SigningConfig`] for L1 transaction signing.
- **`error`** — [`RegistrarError`] enum covering all failure modes.
- **`prover`** — [`ProverClient`] JSON-RPC client for polling prover signer endpoints.
- **`registration_manager`** — [`RegistrationManager`] orchestration for signer proof requests.
- **`proof_handler`** — [`ProofHandler`] handling for completed proofs and registration txs.
- **`traits`** — [`InstanceDiscovery`] and [`AttestationProofProvider`] trait definitions.
- **`types`** — Core domain types: [`ProverInstance`], [`RegisteredSigner`].
