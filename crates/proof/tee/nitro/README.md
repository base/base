# `base-proof-tee-nitro`

AWS Nitro Enclave TEE proof generation and verification.

## Overview

Implements the full TEE proving pipeline for AWS Nitro Enclaves. The crate is split into two
halves — **enclave** (runs inside the enclave) and **host** (runs on the parent instance,
behind the `host` feature flag) — connected by a length-prefixed bincode protocol over vsock.

In production the host forwards preimages over vsock to the enclave, which re-executes the
block, signs the result with an ephemeral ECDSA key, and returns a `ProofResult`. In local
development mode the same pipeline runs in-process without vsock or NSM hardware.

## Architecture

```text
┌─────────────────────────────────────────────────────────────┐
│  Host  (feature = "host")                                   │
│                                                             │
│  NitroProverServer                                          │
│  ├─ ProverService<NitroBackend>   (JSON-RPC: prover_*)      │
│  │   └─ NitroBackend::prove()                               │
│  │       ├─ Host::build_witness() → Oracle with preimages   │
│  │       └─ NitroTransport::prove(preimages)                │
│  └─ NitroTransport               (JSON-RPC: enclave_*)      │
│      ├─ Vsock(VsockTransport)     production, Linux-only    │
│      └─ Local(Arc<Server>)        local dev, in-process     │
│                                                             │
│         ┌───── vsock ──────┐                                │
│         │  Frame protocol  │                                │
│         │  4B len + bincode│                                │
└─────────┴──────────────────┴────────────────────────────────┘
                  │
┌─────────────────┴───────────────────────────────────────────┐
│  Enclave  (always compiled)                                 │
│                                                             │
│  NitroEnclave::run()          vsock listener loop            │
│  └─ handle_connection()       dispatches EnclaveRequest      │
│      └─ Server::prove(preimages)                            │
│          ├─ Oracle::new(preimages)                           │
│          ├─ BootInfo + Prologue → stateless execution       │
│          ├─ Proposal validation + ECDSA signing             │
│          ├─ ProofEncoder::encode_proof_bytes()              │
│          └─ Returns ProofResult::Tee                        │
│                                                             │
│  Server                                                     │
│  ├─ NsmSession       NSM device for PCR0 + attestation      │
│  ├─ Ecdsa / Signing  ephemeral secp256k1 key (NSM RNG)      │
│  └─ Attestation      COSE_Sign1 verification + AWS CA chain │
│                                                             │
│  Oracle              HashMap<PreimageKey, Vec<u8>>           │
│  ├─ PreimageOracleClient  read preimages during execution   │
│  ├─ WitnessOracle         capture preimages during witness  │
│  ├─ HintWriterClient      no-op (TEE has no hint routing)   │
│  └─ FlushableCache        no-op (no external cache)         │
└─────────────────────────────────────────────────────────────┘
```

## Modules

| Module | Description |
|---|---|
| `enclave` | Enclave runtime, vsock listener, request/response protocol |
| `enclave::server` | Proving pipeline: oracle → execute → validate → sign → aggregate |
| `enclave::crypto` | `Ecdsa` key generation and `Signing` (secp256k1 with keccak256) |
| `enclave::nsm` | `NsmSession` (NSM device access) and `NsmRng` (hardware RNG) |
| `enclave::attestation` | `COSE_Sign1` verification, AWS Nitro CA root certificate chain |
| `oracle` | `Oracle` — `HashMap`-backed preimage store for stateless execution |
| `transport` | `Frame` — length-prefixed bincode codec over `AsyncRead`/`AsyncWrite` |
| `error` | `NitroError` umbrella + `NsmError`, `AttestationError`, `CryptoError`, `ProposalError` |
| `host` | *(feature `host`)* `NitroBackend`, `NitroProverServer`, `NitroTransport` |
| `host::vsock` | *(Linux-only)* `VsockTransport` — frame-based vsock communication with timeouts |

## Usage

```toml
[dependencies]
# Enclave-side (default):
base-proof-tee-nitro = { workspace = true }

# Host-side (includes ProverService integration):
base-proof-tee-nitro = { workspace = true, features = ["host"] }
```

## Features

| Feature | Effect |
|---|---|
| `host` | Enables `NitroBackend`, `NitroProverServer`, `NitroTransport`, and JSON-RPC server support |

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
