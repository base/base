# Prover Registrar: Automated TEE Signer Registration System

## Overview

A standalone Rust service that automatically detects new TEE prover instances (rotated
by an AWS Auto Scaling Group) and registers their signers on-chain via the
`SystemConfigGlobal` contract. Registration requires generating a ZK proof of the
Nitro attestation certificate via the Boundless Network (RISC Zero), then submitting
it to L1.

The service also deregisters signers whose backing instances have been terminated,
using the AWS target group as the source of truth for active instances.

### Key Design Decisions

- **Polling via AWS API, not ALB**: The registrar discovers prover instances by
  querying the ALB target group directly (via AWS SDK), so it can see instances in
  the `initial` health-check state before the ALB routes traffic to them.
- **Pre-registration during ALB warm-up**: The ALB health check is configured with a
  ~5-minute warm-up period. The registrar detects and registers new provers during
  this window, so they are registered before receiving proposal traffic.
- **Signer sidecar**: Transaction signing uses an HTTP sidecar (Coinbase keychain)
  in production. A direct private-key signer is available for local testing only.
- **Automata SDK as git dependency**: Used as-is for RISC Zero/Boundless ZK proof
  generation. Heavy compile-time deps are acceptable since proving happens remotely.
- **Trait-based modularity**: Core abstractions are behind traits so future TEE types
  (e.g., TDX) and ZK backends (e.g., SP1) can be swapped in.
- **No proposer modifications**: The proposer is unaware of the registrar. The ALB
  health-check delay ensures provers are registered before receiving traffic.

---

## Architecture

```
                        ┌─────────────────────────────┐
                        │     Prover Registrar         │
                        │     (this service)           │
                        │                              │
                        │  ┌────────┐  ┌────────────┐  │
                        │  │AWS SDK │  │ Boundless   │  │
                        │  │(disc.) │  │ (ZK proofs) │  │
                        │  └───┬────┘  └─────┬──────┘  │
                        └──────┼─────────────┼─────────┘
                               │             │
           ┌───────────────────┼─────┐       │
           │                   │     │       │
           ▼                   ▼     │       ▼
    ┌─────────────┐   ┌──────────┐  │  ┌──────────────┐
    │ ALB Target  │   │ Prover   │  │  │ Boundless    │
    │ Group API   │   │ Instance │  │  │ Network      │
    │ (instance   │   │ (port    │  │  │ (RISC Zero)  │
    │  discovery) │   │  8000)   │  │  └──────────────┘
    └─────────────┘   └──────────┘  │
                                    │
                         ┌──────────┼──────────┐
                         │          ▼          │
                         │  ┌─────────────┐   │
                         │  │ Signer      │   │
                         │  │ Sidecar     │   │
                         │  │ (localhost   │   │
                         │  │  :8545)     │   │
                         │  └──────┬──────┘   │
                         │         │          │
                         │         ▼          │
                         │  ┌─────────────┐   │
                         │  │ L1 RPC      │   │
                         │  │ (Ethereum)  │   │
                         │  └─────────────┘   │
                         │   K8s Pod          │
                         └────────────────────┘
```

### Registration Flow

```
1. Query ALB target group  ──►  List of instances + health status
2. For each instance:
   a. Connect directly by private IP:8000
   b. Call enclave_signerAttestation()  ──►  raw Nitro attestation bytes
   c. Extract signer address from attestation public key
   d. Check SystemConfigGlobal.signerPCR0(address) on L1
   e. If not registered:
      i.   Submit attestation to Boundless Network  ──►  ZK proof
      ii.  Call SystemConfigGlobal.registerSigner(output, proofBytes)
           via signer sidecar
3. Call SystemConfigGlobal.getRegisteredSigners() to get all on-chain signers
4. For each registered signer NOT backed by an active instance:
   a. Call SystemConfigGlobal.deregisterSigner(address)
```

---

## Directory Structure

```
base/
├── bin/
│   └── prover-registrar/              # Binary crate (base-proof-tee-registrar-bin)
│       ├── Cargo.toml
│       ├── README.md
│       └── src/
│           ├── main.rs                # Minimal entrypoint
│           └── cli.rs                 # CLI args, delegates to library
│
└── crates/
    └── proof/
        └── tee/
            └── registrar/             # Library crate (base-proof-tee-registrar)
                ├── Cargo.toml
                ├── README.md
                └── src/
                    ├── lib.rs         # Module declarations + re-exports
                    ├── config.rs      # RegistrarConfig, CLI args
                    ├── error.rs       # Error types
                    ├── types.rs       # ProverInstance, AttestationProof, etc.
                    ├── traits.rs      # Core trait definitions
                    ├── discovery.rs   # AwsTargetGroupDiscovery
                    ├── prover.rs      # ProverClient (JSON-RPC to enclave)
                    ├── registry.rs    # SystemConfigGlobal contract bindings
                    ├── proof.rs       # BoundlessNitroProofProvider
                    ├── signer.rs      # HttpSignerClient + LocalKeySigner
                    ├── driver.rs      # RegistrationDriver (main loop)
                    ├── health.rs      # Health check server
                    └── metrics.rs     # Prometheus metrics
```

---

## Core Types and Traits

### Types

```rust
/// A discovered prover instance from the infrastructure layer.
pub struct ProverInstance {
    pub instance_id: String,
    pub private_ip: String,
    pub health_status: InstanceHealthStatus,
}

pub enum InstanceHealthStatus {
    Initial,    // Health checks in progress (not yet receiving traffic)
    Healthy,    // Receiving traffic
    Unhealthy,  // Failing health checks
    Draining,   // Being decommissioned
}

/// Response from polling a prover's enclave_signerAttestation endpoint.
pub struct AttestationResponse {
    pub attestation_bytes: Bytes,  // Raw Nitro attestation (COSE_Sign1)
    pub signer_address: Address,   // Derived from the attestation's public key
}

/// ZK proof ready for on-chain registration.
pub struct AttestationProof {
    pub output: Bytes,      // ABI-encoded VerifierJournal
    pub proof_bytes: Bytes, // ZK proof bytes
}

/// A signer currently registered on-chain (read via getRegisteredSigners()).
pub struct RegisteredSigner {
    pub address: Address,
    pub pcr0: B256,
}
```

### Traits

```rust
/// Discovers prover instances in the infrastructure.
/// Implementations: AwsTargetGroupDiscovery.
/// Future: TDX instance discovery, manual endpoint list.
pub trait InstanceDiscovery: Send + Sync {
    async fn discover_instances(&self) -> Result<Vec<ProverInstance>>;
}

/// Generates ZK proofs for TEE attestation documents.
/// Implementations: BoundlessNitroProofProvider.
/// Future: SP1 provider, TDX attestation provider.
pub trait AttestationProofProvider: Send + Sync {
    async fn generate_proof(
        &self,
        attestation_bytes: &[u8],
    ) -> Result<AttestationProof>;
}
```

---

## Step Breakdown

### Step 0: SystemConfigGlobal Contract — Add Signer Enumeration

**Repo**: `/Users/leopoldjoy/Coinbase-External/contracts` (contracts repo)

**Scope**: Add an `EnumerableSet` to `SystemConfigGlobal` so the registrar can
read the full set of registered signers via a single view call, rather than
reconstructing state from historical events (which is fragile and expensive).

**Changes to `src/multiproof/tee/SystemConfigGlobal.sol`**:
- Import `EnumerableSetLib` from Solady (consistent with existing multiproof
  contracts which already use Solady)
- Add `using EnumerableSetLib for EnumerableSetLib.AddressSet`
- Add `EnumerableSetLib.AddressSet private _registeredSigners` storage
- In `registerSigner()`: add `_registeredSigners.add(signerAddress)` after
  setting `signerPCR0`
- In `deregisterSigner()`: add `_registeredSigners.remove(signer)` after
  deleting `signerPCR0`
- Add new view function:
  ```solidity
  function getRegisteredSigners() external view returns (address[] memory) {
      return _registeredSigners.values();
  }
  ```
- Optionally add `getRegisteredSignerCount()` for convenience

**Changes to tests** (`test/multiproof/SystemConfigGlobal.t.sol`):
- Test `getRegisteredSigners()` returns correct set after register/deregister
- Test enumeration consistency across multiple register/deregister cycles

**Why this is a prerequisite**: Without on-chain enumeration, the registrar must
reconstruct signer state from historical events — which is fragile (RPC gaps,
reorgs), slow (block scanning), and adds significant complexity. A single view
call eliminates all of that. The storage overhead is ~2 extra slots per signer
(negligible at this scale).

---

### Step 0.5: Prover Proxy — Expose Signer HTTP Endpoints

**Repo**: This repo (base)

**Scope**: The prover's JSON-RPC endpoints (`enclave_signerPublicKey`,
`enclave_signerAttestation`) were removed when the nitro crate was restructured
to use vsock frames instead of JSON-RPC. The underlying `Server` methods still
exist, but they are no longer reachable over HTTP. The registrar needs to poll
prover instances over HTTP (by private IP), so we need to expose these two
methods via simple HTTP endpoints on the proxy that runs on the EC2 host.

**Changes to the nitro host module** (`crates/proof/tee/nitro/src/host/`):
A new `NitroProverServer` (JSON-RPC, cc536206) was recently added to the host
module, but it only exposes `prove()`. We need to add signer info endpoints.
Options:
- Add `signer_public_key` and `signer_attestation` methods to the existing
  `NitroProverServer` JSON-RPC interface (preferred — follows existing pattern)
- Or add simple HTTP routes to the proxy layer

The enclave-side `Server` struct still has `signer_public_key()` and
`signer_attestation()` methods. These need to be plumbed through the vsock
transport to the host-side server.

**Endpoints to add**:
- `signer_publicKey()` — returns the 65-byte uncompressed ECDSA public key
- `signer_attestation()` — returns the raw Nitro attestation document
  (COSE_Sign1 bytes)

**Why these are lightweight**: Both are read-only, no parameters, small responses.

**Testing**: Unit test verifying the proxy routes return expected formats.

---

### Step 1: Core Library Crate — Types, Traits, Config, Contract Bindings

**Scope**: Foundation crate with all type definitions, trait declarations, config
parsing, error types, and SystemConfigGlobal contract bindings. Compiles and is
testable, but has no runtime behavior yet.

**Crates created**:
- `crates/proof/tee/registrar/` (`base-proof-tee-registrar`)

**Files**:
- `Cargo.toml` — dependencies: alloy-primitives, alloy-sol-types, alloy-provider,
  clap, eyre, serde, tracing, tokio
- `README.md` — crate description
- `src/lib.rs` — module declarations + re-exports
- `src/types.rs` — `ProverInstance`, `InstanceHealthStatus`, `AttestationResponse`,
  `AttestationProof`, `RegisteredSigner`
- `src/traits.rs` — `InstanceDiscovery`, `AttestationProofProvider`
- `src/config.rs` — `RegistrarConfig` struct with clap derives (all CLI args and
  env vars), `RegistrarConfig::validate()` method
- `src/error.rs` — `RegistrarError` enum with variants for each failure mode
- `src/registry.rs` — SystemConfigGlobal contract bindings via `alloy::sol!`:
  - `registerSigner(bytes, bytes)` call
  - `deregisterSigner(address)` call
  - `signerPCR0(address) -> bytes32` view call
  - `isValidSigner(address) -> bool` view call
  - `getRegisteredSigners() -> address[]` view call (from Step 0)
  - `RegistryChecker` struct: methods to check individual registration status
    and fetch the complete set of registered signers via a single view call

**Workspace changes**:
- Add `base-proof-tee-registrar` to workspace `Cargo.toml` (members, dependencies)
- Add new external dependencies to workspace if not already present

**Testing**: Unit tests for config validation, contract ABI encoding.

---

### Step 2: AWS Instance Discovery + Prover Client

**Scope**: Implement the two "input" halves — discovering instances via AWS and
polling them for attestations. After this PR, the service can discover instances
and fetch their signer info, but cannot yet register them.

**New dependencies**: `aws-sdk-elasticloadbalancingv2`, `aws-sdk-ec2`,
`aws-config`, `jsonrpsee` (HTTP client)

**Files**:
- `src/discovery.rs` — `AwsTargetGroupDiscovery`:
  - Implements `InstanceDiscovery` trait
  - Uses `describe_target_health` to get all targets + health status
  - Uses `describe_instances` to resolve private IPs from instance IDs
  - Configurable target group ARN and AWS region
  - Maps AWS health states to `InstanceHealthStatus`
- `src/prover.rs` — `ProverClient`:
  - Connects to a prover instance by `http://{private_ip}:{port}`
  - Calls `GET /signer/public-key` to get the 65-byte public key (from Step 0.5)
  - Derives Ethereum address from public key (keccak256 of uncompressed point
    bytes [1..65], take last 20 bytes)
  - Calls `GET /signer/attestation` to get raw attestation bytes (only when
    registration is needed, to avoid unnecessary overhead)
  - Returns `AttestationResponse`
  - Configurable timeout, retry count

**Testing**:
- Unit tests with mock AWS responses (mock the SDK clients)
- Unit tests for attestation parsing and address derivation (use sample attestation
  from the Automata SDK's `samples/` directory)

---

### Step 3: ZK Proof Generation — Automata SDK / Boundless Integration

**Scope**: Implement the `AttestationProofProvider` trait using the Automata SDK
with RISC Zero / Boundless Network backend. After this PR, the service can take
raw attestation bytes and produce a ZK proof ready for on-chain submission.

**New dependencies**: `aws-nitro-enclave-attestation` (Automata SDK, git dependency
from the `aws-nitro-enclave-attestation` repo — specifically the `prover` crate)

**Files**:
- `src/proof.rs` — `BoundlessNitroProofProvider`:
  - Implements `AttestationProofProvider` trait
  - Wraps `NitroEnclaveProver` from the Automata SDK
  - Configures RISC Zero / Boundless backend via `ProverConfig`
  - Takes raw attestation bytes, calls `prove_attestation_report()`
  - Maps `OnchainProof` output to `AttestationProof` (extract `journal` → `output`,
    `encoded_proof` → `proof_bytes`)
  - All Boundless parameters configurable: RPC URL, private key, program URLs,
    pricing, timeout
  - Optionally connects to `NitroEnclaveVerifierContract` for certificate caching

**Config additions** (in `config.rs`):
- `boundless_rpc_url`, `boundless_private_key`, `boundless_verifier_program_url`,
  `boundless_min_price`, `boundless_max_price`, `boundless_timeout`
- `nitro_verifier_contract_address` (optional, for cert caching)

**Design note — future TDX/other TEE support**:
The `AttestationProofProvider` trait is TEE-agnostic. A future `TdxProofProvider`
would implement the same trait with a different ZK circuit and attestation format.
The driver doesn't care which TEE type is being verified.

**Testing**:
- Unit tests with RISC0 dev mode (`RISC0_DEV_MODE=1`) for fast local testing
- Integration test scaffold (requires Boundless Network access, gated behind feature)

---

### Step 4: Transaction Signing + Registration Driver

**Scope**: The core orchestration loop. Ties together discovery, polling, status
checking, proof generation, and on-chain registration/deregistration. After this PR,
the full registration pipeline works end-to-end in the library.

**Files**:
- `src/signer.rs` — Transaction signing abstraction:
  - `HttpSignerClient`: Sends unsigned transactions to the signer sidecar via
    `eth_signTransaction` JSON-RPC (matches OP Stack signer protocol at
    `{signer_endpoint}`). Returns signed raw transaction bytes. Submits via
    L1 provider's `eth_sendRawTransaction`.
  - `LocalKeySigner`: Uses `alloy-signer-local::PrivateKeySigner` directly.
    Clearly documented as **development/testing only**.
  - Both implement a common `RegistrarSigner` trait with
    `sign_and_send(tx: TransactionRequest) -> Result<TxHash>` and
    `wait_for_receipt(tx_hash: TxHash) -> Result<TransactionReceipt>`
  - **Note**: A new `base-tx-manager` crate (with `TxManager` trait and
    `TxCandidate` type) has been scaffolded in `crates/utilities/tx-manager/`.
    Once its `SimpleTxManager` implementation is complete, the registrar
    should migrate to use it. For now, the registrar implements its own
    signing abstraction since `SimpleTxManager` is still stubbed (`todo!()`).
- `src/driver.rs` — `RegistrationDriver`:
  - **No internal state reconstruction needed** — reads registered signers
    directly from the contract via `getRegisteredSigners()` each cycle
  - **Main loop** (runs on configurable `poll_interval`, default 30s):
    1. `discover_instances()` — get current instances from AWS
    2. For each instance with status `Initial` or `Healthy`:
       - Poll via `ProverClient` to get attestation + signer address
       - Check `RegistryChecker::is_registered(address)` on L1
       - If NOT registered:
         - Log and set metrics
         - Call `AttestationProofProvider::generate_proof(attestation_bytes)`
         - Build `registerSigner(output, proofBytes)` calldata
         - Send via `RegistrarSigner::sign_and_send()`
         - Wait for receipt, verify success
    3. Fetch `RegistryChecker::get_registered_signers()` from contract
    4. For each registered signer whose address does NOT match any active
       instance's signer:
       - Build `deregisterSigner(address)` calldata
       - Send via `RegistrarSigner::sign_and_send()`
  - **Error handling**:
    - Individual instance failures don't stop the loop
    - ZK proof generation failures trigger retries with exponential backoff
    - L1 transaction failures are logged with full context, retried next cycle
    - All errors surfaced via metrics
  - **Graceful shutdown**: Responds to SIGTERM/SIGINT, completes in-flight
    registrations before exiting

**Testing**:
- Unit tests with mock implementations of all traits
- Integration test scaffold with local Anvil L1 fork

---

### Step 5: Binary Crate — CLI, Health Checks, Metrics

**Scope**: The deployable binary. Wires up all library components via CLI args,
adds health and metrics endpoints.

**Crates created**:
- `bin/prover-registrar/` (`base-proof-tee-registrar-bin`)

**Files**:
- `Cargo.toml` — depends on `base-proof-tee-registrar`, `base-cli-utils`, clap,
  tokio, tracing-subscriber, eyre
- `README.md` — usage and configuration reference
- `src/main.rs` — minimal entrypoint (same pattern as bin/proposer)
- `src/cli.rs` — `Cli` struct, flattens `RegistrarConfig` from library,
  `run()` method:
  1. Initialize tracing (JSON or text format)
  2. Parse and validate config
  3. Initialize Prometheus metrics via `base_cli_utils::PrometheusServer`
     (reuses the shared `MetricsConfig` pattern used by proposer/challenger)
  4. Construct all components (discovery, prover client, proof provider, signer)
  5. Start health check server
  6. Start `RegistrationDriver::run()` loop
  7. Await shutdown signal

**Files in library crate** (added this PR):
- `src/health.rs` — HTTP health check server:
  - `GET /healthz` — liveness (always 200)
  - `GET /readyz` — readiness (503 until first successful poll, then 200)
- `src/metrics.rs` — Registrar-specific metric definitions using the `metrics`
  crate (counters, gauges, histograms). The Prometheus exporter/server itself
  is handled by `base-cli-utils::PrometheusServer`.
  Metrics defined:
  - `prover_registrar_registered_signers` (gauge)
  - `prover_registrar_registrations_total` (counter)
  - `prover_registrar_deregistrations_total` (counter)
  - `prover_registrar_proof_generation_duration_seconds` (histogram)
  - `prover_registrar_registration_errors_total` (counter, by error type)
  - `prover_registrar_poll_duration_seconds` (histogram)

**Workspace changes**:
- Add `base-proof-tee-registrar-bin` to workspace members and default-members

**Testing**: Smoke test (binary starts, prints help, exits cleanly).

---

### Step 6: Deployment Configuration (base-proofs repo)

**Repo**: `/Users/leopoldjoy/Coinbase/base-proofs` (NOT base)

**Scope**: Dockerfile, Helm chart, Terraform, and CodeFlow modifications to deploy
the registrar service alongside the signer sidecar.

**Files to create/modify**:

1. **Dockerfile** (`docker/prover-registrar/Dockerfile`):
   - Multi-stage build (same pattern as `docker/proposer/Dockerfile`)
   - Clone base at pinned commit
   - `cargo build --profile release --package base-proof-tee-registrar-bin
     --bin base-proof-tee-registrar`
   - Copy binary to runtime image

2. **Entrypoint** (`docker/prover-registrar/entrypoint.sh`):
   - Load env vars from envmapper
   - Exec `base-proof-tee-registrar` with all config flags

3. **Helm chart** (extend existing or create new):
   - New service `prover-registrar` with containers:
     - `prover-registrar`: Main registrar binary
     - `signer`: The existing signer sidecar image
       (`652969937640.dkr.ecr.us-east-1.amazonaws.com/protocols/base/signer@sha256:066f...`)
   - Environment variables:
     ```yaml
     _registrar_env: &registrar_env
       # L1
       REGISTRAR_L1_RPC_URL: *l1_eth_rpc
       REGISTRAR_SYSTEM_CONFIG_GLOBAL_ADDRESS: "<deployed address>"
       # Signer sidecar
       REGISTRAR_SIGNER_ENDPOINT: "http://localhost:8545"
       REGISTRAR_SIGNER_ADDRESS: "<manager address>"
       REGISTRAR_SIGNER_TLS_ENABLED: "false"
       # AWS
       REGISTRAR_TARGET_GROUP_ARN: "<target group ARN>"
       REGISTRAR_AWS_REGION: "us-east-1"
       REGISTRAR_PROVER_PORT: "8000"
       # Boundless
       REGISTRAR_BOUNDLESS_RPC_URL: "<boundless rpc>"
       REGISTRAR_BOUNDLESS_VERIFIER_PROGRAM_URL: "<ipfs url>"
       # Polling
       REGISTRAR_POLL_INTERVAL_SECS: "30"

     _signer_env: &signer_env
       CB_SIGNER_LOG_FORMAT: "json"
       CB_SIGNER_KEYCHAIN_S2S_AUDIENCE: "c3/keychain::::"
       CB_SIGNER_KEYCHAIN_SERVICE_ADDRESS: "rpc.keychain.us-east-1.production.cbhq.net:8360"
     ```
   - Scaling: `min: 1, max: 1` (single instance, tied to one signer/manager key)

4. **Terraform** (`terraform/main/main.tf`):
   - Add `module "service_prover_registrar"` (K8s/EKS service via `cb-service`)
   - IAM policy for the registrar's service account:
     - `elasticloadbalancing:DescribeTargetHealth` on the prover target group
     - `ec2:DescribeInstances`

5. **ALB health check tuning** (modify existing prover-nitro target group):
   Extract `healthy_threshold` and `interval` into Terraform variables so the
   warm-up window is easily tunable without editing the module:
   ```hcl
   variable "prover_health_check_interval" {
     description = "Seconds between ALB health checks for prover instances"
     type        = number
     default     = 60
   }

   variable "prover_healthy_threshold" {
     description = "Consecutive health checks before instance receives traffic"
     type        = number
     default     = 60  # 60 × 60s = ~1 hour warm-up
   }

   # In the target group config:
   health_check = {
     enabled             = true
     port                = 8000
     path                = "/"
     interval            = var.prover_health_check_interval
     healthy_threshold   = var.prover_healthy_threshold
     unhealthy_threshold = 2
     timeout             = 30
     matcher             = "200"
   }
   ```
   Default: ~1 hour before a new instance receives traffic, giving the
   registrar ample time to detect and register new signers via Boundless.
   Tune down once actual proving times are observed in production.

6. **CodeFlow ASG modification** (manual, in CodeFlow UI):
   - Update autoscaling to `min: 4, max: 4` for high availability, and add
     `health_check_grace_period` to prevent premature termination during warm-up:
   ```yaml
   autoscaling:
     min_size: 4
     max_size: 4
     health_check_grace_period: 3600
   ```
   The `health_check_grace_period` should match the ALB warm-up window (~1 hour)
   to prevent the ASG from terminating instances that are still warming up.
   With `min: 4`, if one instance dies the other keeps serving while the ASG
   launches a replacement. Note: enclave-to-enclave key syncing has been removed
   (cb1c79b2), so each instance generates its own key and needs independent
   registration. The registrar handles this automatically.

7. **Deployment & rollout runbook** (documentation):
   - Create a runbook covering:
     - Initial deployment steps (first-time setup, manager key configuration)
     - Enclave software upgrade process (build new EIF → upload verifier ELF
       to IPFS via `nitro-attest-cli upload --risc0` → update
       `REGISTRAR_BOUNDLESS_VERIFIER_PROGRAM_URL` → deploy)
     - PCR0 rotation workflow (register new PCR0 on SystemConfigGlobal →
       deploy new enclave image → registrar auto-registers new signers)
     - Troubleshooting common scenarios (Boundless timeouts, signer sidecar
       issues, ALB warm-up timing)
   - Location: `docs/guides/` in base-proofs or alongside the Helm chart

8. **(Optional) Migrate base-proofs-nitro-prover into base-proofs**:
   - The defunct `base-proofs-nitro-prover` repo's Docker/entrypoint logic already
     exists in `base-proofs/docker/prover-nitro/`. Confirm these are in sync and
     deprecate the standalone repo.

---

## Configuration Reference

All parameters are available as both CLI flags and environment variables.

| Parameter | Env Var | Default | Description |
|-----------|---------|---------|-------------|
| `--l1-rpc-url` | `REGISTRAR_L1_RPC_URL` | (required) | L1 Ethereum RPC endpoint |
| `--system-config-global-address` | `REGISTRAR_SYSTEM_CONFIG_GLOBAL_ADDRESS` | (required) | SystemConfigGlobal contract address on L1 |
| `--target-group-arn` | `REGISTRAR_TARGET_GROUP_ARN` | (required) | AWS ALB target group ARN for prover instances |
| `--aws-region` | `REGISTRAR_AWS_REGION` | `us-east-1` | AWS region |
| `--prover-port` | `REGISTRAR_PROVER_PORT` | `8000` | Port to poll on prover instances |
| `--signer-endpoint` | `REGISTRAR_SIGNER_ENDPOINT` | (none) | HTTP signer sidecar URL (prod) |
| `--signer-address` | `REGISTRAR_SIGNER_ADDRESS` | (required) | Manager address for signing registration txs |
| `--private-key` | `REGISTRAR_PRIVATE_KEY` | (none) | Direct private key (LOCAL DEV ONLY) |
| `--signer-tls-enabled` | `REGISTRAR_SIGNER_TLS_ENABLED` | `false` | Enable TLS for signer sidecar (always false — sidecar is localhost) |
| `--boundless-rpc-url` | `REGISTRAR_BOUNDLESS_RPC_URL` | (required) | Boundless Network RPC |
| `--boundless-private-key` | `REGISTRAR_BOUNDLESS_PRIVATE_KEY` | (required) | Wallet key for Boundless proving fees |
| `--boundless-verifier-program-url` | `REGISTRAR_BOUNDLESS_VERIFIER_PROGRAM_URL` | (required) | IPFS URL of verifier ELF |
| `--boundless-min-price` | `REGISTRAR_BOUNDLESS_MIN_PRICE` | `100000` | Min price (wei/cycle) |
| `--boundless-max-price` | `REGISTRAR_BOUNDLESS_MAX_PRICE` | `1000000` | Max price (wei/cycle) |
| `--boundless-timeout` | `REGISTRAR_BOUNDLESS_TIMEOUT` | `600` | Proof generation timeout (seconds) |
| `--nitro-verifier-address` | `REGISTRAR_NITRO_VERIFIER_ADDRESS` | (none) | NitroEnclaveVerifier address (cert caching) |
| `--poll-interval` | `REGISTRAR_POLL_INTERVAL_SECS` | `30` | Seconds between poll cycles |
| `--health-port` | `REGISTRAR_HEALTH_PORT` | `7300` | Health/metrics HTTP port |
| `--log-format` | `REGISTRAR_LOG_FORMAT` | `json` | Log format: `json` or `text` |

---

## Assumptions and Requirements

1. **Network access**: The registrar pod (K8s) must be in the same VPC as the prover
   EC2 instances to reach them by private IP on port 8000.

2. **IAM permissions**: The registrar's K8s service account needs IAM permissions for:
   - `elasticloadbalancing:DescribeTargetHealth` (target group query)
   - `ec2:DescribeInstances` (resolve private IPs from instance IDs)

3. **Signer sidecar**: Uses the existing Coinbase signer image
   (`protocols/base/signer@sha256:066f...`). Speaks `eth_signTransaction` JSON-RPC
   on port 8545.

4. **SystemConfigGlobal `onlyOwnerOrManager`**: The `REGISTRAR_SIGNER_ADDRESS` must
   be set as the `manager` on the SystemConfigGlobal contract.

5. **Boundless Network**: Verifier program ELF must be pre-uploaded to IPFS. Use the
   Automata SDK CLI: `nitro-attest-cli upload --risc0`.

6. **Quad-instance ASG**: The ASG config is `min: 4, max: 4` for high availability.
   Enclave-to-enclave key syncing has been removed (cb1c79b2), so each instance
   generates its own unique signer key at startup. The registrar is essential for
   ALL instances — every new instance needs independent registration. With `min: 4`,
   if one instance dies the other keeps serving while the replacement boots and
   gets registered by the registrar during the ALB warm-up window.

7. **L1 finality**: Registration transactions target L1 (Ethereum Sepolia / Mainnet).
   The registrar waits for transaction receipts before updating internal state.

---

## Risk Mitigation

| Risk | Mitigation |
|------|------------|
| ZK proof generation slow | Default 1-hour ALB warm-up provides ample buffer; timing is tunable via Terraform variables; registrar logs proof generation duration |
| Boundless Network unavailable | Exponential backoff retries; clear metrics/alerts; manual registration remains possible |
| Signer sidecar unavailable | Health check returns 503 (not ready); pod restart via K8s liveness probe |
| Multiple instances simultaneously (ASG scaling event) | Driver handles multiple instances per cycle; each registered independently |
| Stale signer not deregistered (edge case) | Each cycle reads `getRegisteredSigners()` from contract (authoritative) and compares against active instances; deregister orphans |
| Race between registrar and proposer | ALB health check delay ensures registration completes before traffic routes |

---

## Tickets / PRs

Granular breakdown for implementation. Each ticket maps 1:1 to a PR (~500 lines
or less). Tickets are ordered by dependency — each can only start once its
blockers are complete.

### CHAIN-3445: [Admin] Keychain provisioning for base-proofs
- **Step**: Parallel (non-blocking)
- **Repo**: N/A (administrative)
- **Scope**: Make internal Coinbase requests to provision the base-proofs repo
  with the keychain resources needed to bootstrap the signer sidecar. This
  involves coordinating with the relevant internal teams to set up key
  allocation, S2S audience, and service account permissions.
- **Est. size**: Administrative — no code
- **Blocks**: CHAIN-3458 (deployment cannot proceed without keychain access)
- **Note**: Can and should be started immediately, in parallel with all dev
  work, as provisioning requests may have lead time.

### CHAIN-3446: [Contracts] Add signer enumeration to SystemConfigGlobal
- **Step**: 0
- **Repo**: contracts
- **Scope**: Add Solady `EnumerableSetLib.AddressSet` to `SystemConfigGlobal`, update
  `registerSigner`/`deregisterSigner` to maintain the set, add
  `getRegisteredSigners()` view function, add tests
- **Est. size**: ~150 lines (contract) + ~100 lines (tests)
- **Blocks**: CHAIN-3449

### CHAIN-3447: [Prover] Expose signer public key endpoint on host server
- **Step**: 0.5
- **Repo**: base
- **Scope**: Plumb `Server::signer_public_key()` through the vsock transport to
  the host-side `NitroProverServer`. Add `signer_publicKey()` JSON-RPC method.
  Add unit test.
- **Est. size**: ~150 lines
- **Blocks**: CHAIN-3451

### CHAIN-3448: [Prover] Expose signer attestation endpoint on host server
- **Step**: 0.5
- **Repo**: base
- **Scope**: Plumb `Server::signer_attestation()` through the vsock transport to
  the host-side `NitroProverServer`. Add `signer_attestation()` JSON-RPC method.
  Add unit test.
- **Est. size**: ~150 lines
- **Blocks**: CHAIN-3451

### CHAIN-3449: [Registrar] Crate scaffolding — types, traits, errors, config
- **Step**: 1
- **Repo**: base
- **Scope**: Create `crates/proof/tee/registrar/` crate. Add `Cargo.toml`,
  `README.md`, `lib.rs`, `types.rs` (ProverInstance, InstanceHealthStatus,
  AttestationResponse, AttestationProof, RegisteredSigner), `traits.rs`
  (InstanceDiscovery, AttestationProofProvider), `error.rs` (RegistrarError),
  `config.rs` (RegistrarConfig with clap derives). Add crate to workspace.
- **Est. size**: ~400 lines
- **Blocks**: CHAIN-3450, CHAIN-3451, CHAIN-3452, CHAIN-3453, CHAIN-3454

### CHAIN-3450: [Registrar] SystemConfigGlobal contract bindings
- **Step**: 1
- **Repo**: base
- **Scope**: Add `registry.rs` with `alloy::sol!` bindings for
  `registerSigner`, `deregisterSigner`, `signerPCR0`, `isValidSigner`,
  `getRegisteredSigners`. Implement `RegistryChecker` struct with methods
  for checking registration status and fetching the full signer set. Unit tests.
- **Est. size**: ~250 lines
- **Blocked by**: CHAIN-3449
- **Blocks**: CHAIN-3454

### CHAIN-3451: [Registrar] AWS target group instance discovery
- **Step**: 2
- **Repo**: base
- **Scope**: Add `discovery.rs` with `AwsTargetGroupDiscovery` implementing
  `InstanceDiscovery`. Uses `describe_target_health` and `describe_instances`.
  Add AWS SDK workspace dependencies. Unit tests with mock AWS clients.
- **Est. size**: ~300 lines
- **Blocked by**: CHAIN-3449
- **Blocks**: CHAIN-3454

### CHAIN-3452: [Registrar] Prover HTTP client for signer endpoints
- **Step**: 2
- **Repo**: base
- **Scope**: Add `prover.rs` with `ProverClient`. Calls `signer_publicKey()`
  and `signer_attestation()` on prover instances by private IP. Derives
  Ethereum address from public key. Unit tests for address derivation.
- **Est. size**: ~200 lines
- **Blocked by**: CHAIN-3449
- **Blocks**: CHAIN-3454

### CHAIN-3453: [Registrar] ZK proof generation via Automata SDK / Boundless
- **Step**: 3
- **Repo**: base
- **Scope**: Add `proof.rs` with `BoundlessNitroProofProvider` implementing
  `AttestationProofProvider`. Add Automata SDK git dependency. Configure
  Boundless parameters. Add config fields. Unit tests with RISC0 dev mode.
- **Est. size**: ~300 lines
- **Blocked by**: CHAIN-3449
- **Blocks**: CHAIN-3454

### CHAIN-3454: [Registrar] Transaction signing abstraction
- **Step**: 4
- **Repo**: base
- **Scope**: Add `signer.rs` with `RegistrarSigner` trait, `HttpSignerClient`
  (sends `eth_signTransaction` to sidecar, submits via `eth_sendRawTransaction`),
  and `LocalKeySigner` (dev only, clearly documented). Nonce management.
  Unit tests.
- **Est. size**: ~350 lines
- **Blocked by**: CHAIN-3449
- **Blocks**: CHAIN-3455

### CHAIN-3455: [Registrar] Registration driver — core loop
- **Step**: 4
- **Repo**: base
- **Scope**: Add `driver.rs` with `RegistrationDriver`. Main loop: discover
  instances, poll each for signer address, check registration on L1, generate
  ZK proof if needed, submit registration tx. Error handling with per-instance
  isolation and exponential backoff.
- **Est. size**: ~350 lines
- **Blocked by**: CHAIN-3450, CHAIN-3451, CHAIN-3452, CHAIN-3453, CHAIN-3454
- **Blocks**: CHAIN-3457

### CHAIN-3456: [Registrar] Deregistration logic
- **Step**: 4
- **Repo**: base
- **Scope**: Extend `RegistrationDriver` with deregistration: each cycle call
  `getRegisteredSigners()`, compare against active instances, deregister
  orphaned signers. Graceful shutdown (SIGTERM/SIGINT). Unit tests with mock
  traits.
- **Est. size**: ~200 lines
- **Blocked by**: CHAIN-3455
- **Blocks**: CHAIN-3457

### CHAIN-3457: [Registrar] Binary crate, health checks, and metrics
- **Step**: 5
- **Repo**: base
- **Scope**: Create `bin/prover-registrar/` with `main.rs` and `cli.rs`. Add
  `health.rs` and `metrics.rs` to library crate. Wire up all components.
  Uses `base-cli-utils::PrometheusServer`. Add to workspace. Smoke test.
- **Est. size**: ~350 lines
- **Blocked by**: CHAIN-3455, CHAIN-3456

### CHAIN-3458: [Deploy] Dockerfile and Helm chart for registrar
- **Step**: 6
- **Repo**: base-proofs
- **Scope**: Create `docker/prover-registrar/Dockerfile` and `entrypoint.sh`.
  Add Helm chart service with registrar + signer sidecar containers.
  Environment variable configuration.
- **Est. size**: ~200 lines
- **Blocked by**: CHAIN-3457

### CHAIN-3459: [Deploy] Terraform, ALB tuning, CodeFlow ASG config
- **Step**: 6
- **Repo**: base-proofs
- **Scope**: Add `service_prover_registrar` Terraform module with IAM policy.
  Extract ALB health check params into Terraform variables (default ~1 hour).
  Document CodeFlow ASG changes (`min: 4, max: 4`,
  `health_check_grace_period: 3600`).
- **Est. size**: ~150 lines
- **Blocked by**: CHAIN-3458

### CHAIN-3460: [Deploy] Deployment and rollout runbook
- **Step**: 6
- **Repo**: base-proofs
- **Scope**: Create runbook covering initial deployment, enclave upgrade
  process (EIF build → IPFS ELF upload → config update → deploy), PCR0
  rotation workflow, and troubleshooting guide.
- **Est. size**: ~200 lines (documentation)
- **Blocked by**: CHAIN-3459
