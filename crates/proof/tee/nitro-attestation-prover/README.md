# base-proof-tee-nitro-attestation-prover

ZK attestation prover for AWS Nitro Enclave attestation documents.

Wraps [`base-proof-tee-nitro-verifier`] verification logic inside a RISC Zero
ZK guest program and provides two proving backends for generating on-chain
verifiable proofs:

- **`DirectProver`** — uses `risc0_zkvm::default_prover()` which routes to
  Bonsai remote proving (`BONSAI_API_KEY`), dev-mode (`RISC0_DEV_MODE=1`),
  or local CPU proving as a fallback.
- **`BoundlessProver`** — submits proof requests to the Boundless marketplace
  for decentralised proving.

The guest ELF is loaded at runtime (from disk or IPFS) rather than embedded at
compile time, so the risc0 toolchain is not required for building this crate.

## How attestation verification works

The ZK proof pipeline establishes a cryptographic chain of trust from AWS
Nitro hardware to on-chain signer registration:

### What the ZK proof does (offchain, inside the RISC Zero guest)

1. **Parses the `COSE_Sign1` envelope** — decodes the CBOR-encoded attestation
   document from the raw bytes returned by the Nitro Secure Module (NSM).
2. **Validates attestation content** — checks that the document is well-formed:
   non-empty `module_id`, non-zero `timestamp`, `digest == "SHA384"`, valid
   PCR indices and sizes, and optional field size limits (M-02 audit fix).
3. **Verifies the x509 certificate chain** — validates the full chain from
   the AWS Nitro root CA down to the leaf certificate, checking signatures,
   validity periods, and key usage constraints (M-01 audit fix).
4. **Verifies the COSE signature** — confirms the attestation document was
   signed by the leaf certificate's P384 key, proving it came from genuine
   AWS Nitro hardware and was not tampered with.
5. **Outputs the verified data as a `VerifierJournal`** — the journal
   contains all Platform Configuration Registers (PCRs, including
   PCR0 — the enclave image hash), public key (from the attestation
   document's optional field), timestamp, and certificate chain hashes.
   These are *proven outputs* — the ZK proof guarantees they were
   extracted from a genuine, unmodified attestation document.

### What the on-chain contract does (`TEEProverRegistry`)

The contract calls `NitroEnclaveVerifier.verify()` to check the ZK proof,
then applies policy checks on the proven journal:

- **PCR0 validation** — compares the attestation's PCR0 against
  `TEE_IMAGE_HASH` from the current `AggregateVerifier` (looked up via
  `DisputeGameFactory` and `gameType`). Reverts with `PCR0Mismatch` if
  the enclave image doesn't match.
- **Freshness check** — rejects attestations older than `MAX_AGE` (60 min).
- **Signer derivation** — derives the signer address from the attestation's
  public key (`keccak256` of the uncompressed ECDSA key, truncated to 20 bytes).

### Why this split matters

The ZK proof guarantees **authenticity** (the attestation came from real
hardware and wasn't modified). The on-chain contract enforces **policy**
(the enclave is running the expected image). This separation means the ZK
circuit never needs to change when the expected PCR0 rotates — only the
on-chain configuration is updated.

## Modules

- **`error`** — [`ProverError`] enum covering verification, risc0, and
  Boundless failures.
- **`types`** — [`AttestationProof`] output type and
  [`AttestationProofProvider`] trait.
- **`direct`** — [`DirectProver`] implementation using `default_prover()`.
- **`boundless`** — [`BoundlessProver`] implementation using the Boundless
  marketplace.
