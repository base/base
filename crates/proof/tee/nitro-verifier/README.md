# base-proof-tee-nitro-verifier

Pure verification logic for AWS Nitro Enclave attestation documents.

Parses `COSE_Sign1` attestation envelopes, extracts and validates attestation
documents, and provides Solidity-aligned types used by the hinted registrar and
native verification flows.

## Modules

- **`attestation`** — `COSE_Sign1` parsing, attestation document extraction, and
  `AttestationReport` combining both.
- **`error`** — [`VerifierError`] enum covering parsing and validation failures.
- **`types`** — Solidity-aligned types generated from `INitroEnclaveVerifier.sol`:
  [`VerifierInput`], [`VerifierJournal`], [`Pcr`], [`VerificationResult`].
