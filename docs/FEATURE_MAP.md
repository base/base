# Feature Map

A short map of what Base owns, where to start, and why the boundaries exist.
Read it before changing product, protocol, builder, proof, Reth integration, or
operator behavior. It is an orientation index, not an API reference.

## How to use this map

1. Start at the system path that owns the observable behavior.
2. Follow the listed flow before changing a boundary.
3. Use the generated index to locate a crate group, then read that crate's
   README, public API, and focused tests.
4. Keep Base policy at the narrowest required boundary; do not add an adapter
   that only forwards an upstream API.

## System paths

### Hierarchy

- **Entry points** — `bin/base`, `bin/node`, and `bin/consensus` start the
  unified node, execution node, and consensus node.
- **Shared protocol** — `crates/common` owns chain/genesis data, transaction
  and EVM/precompile rules, EIP-8130 types, RPC types, signing, and events.
- **Execution** — `crates/execution` owns the Base Reth node, chain spec, EVM,
  txpool, payloads, trie/state, Engine/RPC extensions, and node lifecycle.
- **Consensus** — `crates/consensus` owns L1 following, derivation, origins,
  Engine requests, unsafe gossip, SafeDB, peer discovery, upgrades, and
  consensus RPC.
- **Batcher** — `crates/batcher` turns safe/finalized L2 blocks into encoded,
  compressed blob/calldata submissions and tracks L1 confirmation.
- **Builder** — `crates/builder` adapts pool/state into Base payload ordering,
  metering, multiplexing, sealing, and publication.
- **Proofs** — `crates/proof` owns witness/preimages, proof backends, workers,
  proposer/challenger flows, submission, disputes, and recovery.
- **Operations** — `crates/infra`, `crates/utilities`, `actions/harness`, and
  `etc/systems` provide CLI, snapshots, health, telemetry, devnet, benchmarks,
  system tests, and operator evidence.

### Principal flows

- **User transaction:** RPC → admission/txpool → builder/payload → EVM →
  state/trie → Engine/RPC result. EIP-8130 must agree across all consumers.
- **Derived block:** L1 source → derivation/origin → Engine payload → execution
  → SafeDB/status. Sequencing adds payload requests and unsafe gossip.
- **Batch:** L2 block source → encode/compress → blob/calldata submission → L1
  confirmation → derivation can reproduce the chain.
- **Proof:** agreed inputs → witness/preimages → backend → artifact → proposer
  or challenger → on-chain resolution.
- **Operator lifecycle:** configuration → service/actor ownership → metrics and
  status → restart, recovery, snapshot, or shutdown behavior.

### Repository support surfaces

| Surface | Location | Why it matters |
| --- | --- | --- |
| Workspace contract | `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `clippy.toml`, `rustfmt.toml`, `deny.toml`, `Justfile`, `README.md`, `CONTRIBUTING.md`, `SECURITY.md` | packages, pins, tools, and developer contract |
| Automation | `.config`, `.depot`, `.github` | CI, Depot validation, and repository automation |
| Runnables | `actions`, `bin`, `crates` | fixtures, binaries, and libraries |
| Deployment | `docker-compose.yml`, `.env.mainnet`, `.env.sepolia`, `etc/docker`, `etc/just`, `etc/scripts`, `etc/benchmarks` | images, devnet, release, and benchmark workflows |
| System evidence | `etc/systems`, `etc/tools` | E2E harnesses and focused developer tools |
| Upstream/distribution | `etc/upstream-pins`, `audits`, `baseup` | Reth pin, audit records, and binary distribution |
| Product guidance | `docs` | feature map and operational guides |

Update the relevant image, workflow, script, environment, release path, or
system coverage when a supported operational flow changes.

### Flashblocks to 200 ms blocks

Base is moving from Flashblocks to 200 ms blocks. The Flashblock builder is
scheduled for removal by **October 31, 2026**, after 200 ms blocks activate.
Sequencing work should prepare the 200 ms path, migrate callers/operators, and
remove replaced Flashblocks surface after migration is proven.

## Reth integration boundary

Reth supplies generic Ethereum-node facilities. Base owns rollup policy and the
boundary that applies it.

| Reth facility | Base owns |
| --- | --- |
| Node/CLI | chain spec, node types, runtime extensions, binary wiring |
| DB/provider/trie | proof history, retention, custom witness/trie behavior |
| EVM/execution | Base EVM config, precompiles, native assets, L1 fees, EIP-8130 |
| Pool/payload | admission/order, metering, composition, sealing policy |
| RPC/Engine | Base namespaces, rollup Engine handling, trusted-proxy policy |
| P2P/discovery | rollup peer policy, unsafe gossip, telemetry |
| ExEx/metrics | shadow indexing, tracing, Base events, system-test adapters |

Use an upstream capability directly when it has the required contract. Add a
Base adapter only for Base policy, an externally visible Base contract, or an
upstream compatibility boundary.

## Product direction and retirement commitments

Improve a user/operator-visible correctness, reliability, security, latency,
throughput, or resource-use outcome; complete a planned vertical slice; retire a
superseded path; or make production behavior reproducibly observable.

Current product areas: upgrade delivery; EIP-8130 and validity transactions;
native assets and policies; proof production/disputes; and node/operator
experience. Favor one canonical owner, deterministic feedback, supported-path
performance evidence, and complete ingress-to-execution-to-operations slices.

### Transition-system convergence

Retire pre-Holocene compatibility when the replacement is explicit and tested.
For upgrade, derivation, execution, and operator flows, define states, events,
legal/rejected transitions, recovery, and one canonical owner. Expose effective
state and transition/failure reasons through status, metrics, or structured logs.

Before a PR, state the outcome, preserved contract, roadmap fit, removed or
avoided surface, and focused evidence. Performance work needs a representative
baseline and repeatable comparison. Block-production work also follows
`docs/guides/BLOCK_PRODUCTION_REVIEW.md`.

## Keeping this map current

Run these after a package-set or direct-Reth-boundary change:

```sh
python3 etc/scripts/ci/check_feature_map.py --write
python3 etc/scripts/ci/check_feature_map.py --check
node etc/scripts/ci/check_feature_map_tokens.cjs
```

<!-- BEGIN GENERATED FEATURE MAP INVENTORY -->
### Generated repository index

Generated from Cargo manifests and direct Reth dependencies. It confirms coverage; 
the system-path index above explains ownership and data flow.

#### Base package groups
- **Action harness** — `actions/harness/**` (1 packages)
- **Operational binaries** — `bin/**` (24 packages)
- **L1 batching** — `crates/batcher/**` (8 packages)
- **Payload building** — `crates/builder/**` (5 packages)
- **Shared protocol and primitives** — `crates/common/**` (18 packages)
- **Rollup consensus** — `crates/consensus/**` (13 packages)
- **Execution node** — `crates/execution/**` (23 packages)
- **Operations and E2E tools** — `crates/infra/**` (13 packages)
- **Prover-service API** — `crates/proof/prover-service/**` (4 packages)
- **TEE proving** — `crates/proof/tee/**` (4 packages)
- **ZK proving** — `crates/proof/zk/**` (9 packages)
- **Proofs and disputes** — `crates/proof/**` (14 packages)
- **Shared utilities** — `crates/utilities/**` (15 packages)
- **System-test harness** — `etc/systems/**` (1 packages)
- **Developer tools** — `etc/tools/**` (1 packages)

#### Direct Reth boundary
Base Reth git dependencies are pinned at **`base-v2.5.2.6`**.
- **Node assembly and CLI** (9 direct packages)
- **Payload, EVM, and execution** (15 direct packages)
- **State, database, and trie** (13 direct packages)
- **RPC** (8 direct packages)
- **P2P and discovery** (10 direct packages)
- **Consensus, primitives, and forks** (8 direct packages)
- **Runtime, tracing, and test support** (8 direct packages)
<!-- END GENERATED FEATURE MAP INVENTORY -->
