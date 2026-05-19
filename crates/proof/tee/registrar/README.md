# base-proof-tee-registrar

Library crate for the prover registrar service.

Implements automated discovery and on-chain registration of TEE prover signer
keys for the Base multi-proof system. The registrar polls AWS ALB target groups
to detect new Nitro enclave instances, fetches their attestation documents via
`enclave_signerAttestation`, generates ZK proofs via the Boundless Network
(RISC Zero / Automata SDK), and submits registration transactions to
`TEEProverRegistry` on L1.

## Modules

- **`config`** — [`RegistrarConfig`] runtime config struct and
  [`BoundlessConfig`]. L1 transaction signing is delegated to the
  `base-tx-manager` crate (`TxManagerConfig` + `SignerConfig`).
- **`error`** — [`RegistrarError`] enum covering all failure modes.
- **`prover`** — [`ProverClient`] JSON-RPC client for polling prover signer endpoints.
- **`traits`** — [`InstanceDiscovery`] trait definition. Attestation-proof
  generation is provided by the `base-proof-tee-nitro-attestation-prover` crate
  (`AttestationProofProvider` trait).
- **`types`** — Core domain types: [`ProverInstance`], [`RegisteredSigner`].
