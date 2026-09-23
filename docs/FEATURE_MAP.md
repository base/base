# Feature Map

This is the maintained, high-level map of
Base's code and its Reth boundary. It gives an agent or contributor enough
context to place a change in the right subsystem, follow the principal data
path, and avoid extending a retired one. It is not an API reference, ownership
roster, or substitute for the crate README and tests.

## How to use this map

1. Start with the **system path** that owns the observable behavior.
2. Use the **Reth boundary** to distinguish Base policy and integration code
   from upstream node facilities.
3. Consult the generated inventory to find the crate group, then read the
   target crate's README, public API, and focused tests before changing it.
4. Preserve dependency direction: shared protocol code is below execution,
   consensus, batching, builder, proof, and operational layers. Do not add a
   wrapper merely to forward an upstream API; put Base-specific policy at the
   narrowest boundary that needs it.

The map intentionally records durable responsibilities and interfaces rather
than mirroring files or internal types. If a refactor changes a responsibility,
a boundary, a data flow, or the set of workspace packages, update this document
in the same change.

## System paths

### Execution node and user-facing RPC

`bin/base` and `bin/node` launch the Base Reth node. `base-execution-cli`
parses Base configuration and chain specifications; `base-node-core` supplies
Base node, Engine API, storage, proof-history, and Rollup argument types; and
`base-node-runner` assembles the Reth node lifecycle. The execution group owns
Base's chain specification, EVM behavior, transaction-pool policy, payload
building, trie/state-root work, RPC extensions, ExEx integrations, transaction
forwarding, and shadow indexing.

**Flow:** JSON-RPC / Engine API → Base RPC and pool admission → transaction
selection and payload construction → Base EVM execution → state/trie updates →
Reth provider/database and RPC response. EIP-8130 admission and RPC support
span `base-common-eip8130`, `base-execution-eip8130`, and its RPC/node crates;
the complete behavior must remain consistent across the pool, builder,
execution, RPC, batching, and proof consumers.

### Rollup consensus, derivation, and sequencing

`bin/consensus` starts `base-consensus-node`. The consensus group follows L1,
derives L2 inputs, selects origins, drives Engine API requests, handles unsafe
payload gossip and peer discovery, persists safe checkpoints, and exposes
consensus RPC. The service's actors are the runtime coordination boundary;
`protocol`, `engine`, `derive`, `sources`, `providers`, and `upgrades` carry
protocol rules and external-source adapters.

**Flow:** L1 source → derivation/origin selection → Engine API payload
submission → execution node → safe/finalized checkpoint and status. The
sequencer path adds payload-building requests and unsafe gossip. Upgrade
signals enter through consensus and execution runtime extensions and must be
observable in action/devnet coverage.

### L1 batching and data availability

`bin/base`/batcher commands and the `crates/batcher` group encode L2 data,
compress it, choose blob or calldata transport, submit and monitor L1
transactions, and verify L2 block parity. `base-batcher-service` is the service
boundary; `source`, `encoder`, `comp`, `blobs`, and `core` separate ingestion,
encoding, compression, DA transport, and submission policy.

**Flow:** finalized/safe L2 blocks → batch source → encode/compress →
blob/calldata transaction → L1 confirmation → derivation can reproduce the L2
chain. A batching change must preserve the decoder/derivation contract and
operational recovery behavior.

### Payload builder and block production

The builder group adapts Reth payload services to Base ordering, metering,
publishing, and multiplexing. It connects the execution pool and EVM to
consensus' build/seal requests. `base-builder-core` is the integration-heavy
node/payload path; `multiplex` coordinates downstream builders and `publish`
and `metering` provide the external publication and accounting surfaces.

**Flow:** pool and chain state → Base/Reth payload builder → consensus sealing
and Engine API → execution validation/state root → optional publication and
batching. Keep the normal Reth payload path canonical; do not create a parallel
builder path without a supported end-to-end need and targeted block-production
validation.

### Proof production, disputes, and recovery

The proof group covers derivation/execution inputs, preimages and MPT witness
construction, proof program execution, proof submission, challenge handling,
and worker/prover-service orchestration. Binaries launch proposers,
challengers, prover services, ZK hosts, and TEE components. `base-proof-driver`
orchestrates the proving pipeline; the ZK and TEE subtrees are backend-specific
execution environments rather than alternate protocol definitions.

**Flow:** agreed L2/L1 inputs → witness/preimages → proof executor/backend →
proof artifact → submission/proposer or challenge → on-chain resolution. Any
change must retain deterministic inputs, bounded retry/recovery, and a
reproducible system or E2E path.

### Shared protocol surface

`crates/common` contains no node lifecycle. It is the lowest Base-owned layer
for chain/genesis data, transaction and bundle types, EVM/precompile rules,
L1-fee logic, consensus serialization, RPC types, signing/network primitives,
EIP-8130 types, and observability events. `base-common-evm` is deliberately
usable by the execution stack and integrates with Reth only behind its optional
Reth feature; it must not absorb node, service, or operator concerns.

### Operations, validation, and developer interfaces

`crates/infra`, `crates/utilities`, `bin/*`, `actions/harness`, and
`etc/systems` turn production behavior into runnable interfaces and evidence:
CLI utilities, telemetry, health, snapshots, proxies, load testing, audit
archiving, system tests, action harnesses, devnet orchestration, and benchmark
tools. These consumers should depend downward on stable library interfaces;
they should not become a backdoor for core layers to depend on operator code.

The repository also includes Docker/Compose assets, scripts, Just recipes,
release automation, documentation guides, and GitHub workflows. They compose
these binaries and system tests; update them when a supported operational flow,
configuration name, image, or validation command changes.

### Repository support surfaces

| Surface | Location | Responsibility |
| --- | --- | --- |
| Workspace contract | `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `clippy.toml`, `rustfmt.toml`, `deny.toml`, `Justfile`, `README.md`, `CONTRIBUTING.md`, `SECURITY.md` | workspace membership, dependency pins, toolchain/lint policy, developer entry points, and contribution/security contracts |
| CI and repository automation | `.github`, `.depot`, `.config` | GitHub-facing workflows/actions, Depot build/test/benchmark workflows, and nextest/Zepter configuration |
| Test actions and runnable programs | `actions`, `bin`, `crates` | action fixtures, minimal executable glue, and the Base library layers indexed below |
| Deployment and local operations | `docker-compose.yml`, `.env.mainnet`, `.env.sepolia`, `etc/docker`, `etc/just`, `etc/scripts`, `etc/benchmarks` | Compose entry point, environment defaults, images, recipes, devnet/release/CI helpers, and benchmark scenarios |
| System validation and developer tools | `etc/systems`, `etc/tools` | system/E2E harness and focused tools such as witness comparison |
| Upstream, security, and distribution records | `etc/upstream-pins`, `audits`, `baseup` | Reth pin metadata, audit artifacts, and installer/updater assets |
| Product guidance | `docs` | feature map, operational guides, transaction-event documentation, and static assets |

These are production support surfaces, not incidental files. An operational
change is incomplete if its relevant image, workflow, script, environment,
release flow, or system coverage remains stale.

### Deprecated Flashblocks surface

`base-common-flashblocks`, `base-flashblocks`, `base-flashblocks-node`, and
Flashblock-specific builder paths exist solely to keep deployed systems safe
while they are retired. They are not an extension point. The Flashblock builder
is scheduled for removal by **October 31, 2026**, after 200 ms blocks are
activated. Permitted work prepares that activation, migrates callers and
operators, fixes retirement-blocking safety issues, or deletes the obsolete
surface and its tests, flags, metrics, and documentation.

## Reth integration boundary

Base is a downstream node built against the Base Reth fork pinned in the root
workspace manifest. Reth supplies generic Ethereum-node machinery; Base owns
rollup protocol policy and the adapters that put it into that machinery.

| Reth facility | What Reth provides | Base-owned boundary |
| --- | --- | --- |
| Node assembly and CLI | component lifecycle, launch context, task management, standard commands and metrics | Base CLI arguments, chain spec, node types, runtime extensions, and binary wiring |
| Database, providers, chain state, and trie | MDBX/database interfaces, canonical chain/provider access, pruning/stage types, trie calculation | Base storage setup, proof history, custom trie/witness behavior, and rollup retention policy |
| EVM and execution | EVM traits, REVM integration, execution types/errors, Ethereum primitives | Base EVM configuration, precompiles, native assets, L1 fees, validity transactions, and execution consensus |
| Transaction pool and payloads | pool traits/events, payload jobs, validation primitives, basic builder and Engine payload types | Base transaction admission/order, EIP-8130 handling, metering, payload composition, builder multiplexing, and sealing policy |
| RPC and Engine API | RPC server/module primitives, ETH/Engine types, conversion and transport layers | Base RPC namespaces and methods, rollup Engine handling, txpool RPC, proof/consensus endpoints, and trusted-proxy policy |
| P2P and discovery | peer/network abstractions, ETH wire, ECIES, discv4/discv5, NAT support | rollup peer policy, unsafe-payload gossip, discovery configuration, and telemetry |
| ExEx and observability | ExEx interfaces, tracing, OTLP, node metrics, test utilities | shadow indexer, transaction tracing, Base events/metrics, and system-test adapters |

When an upstream capability already exposes the correct abstraction, call it
directly. Add a Base type or adapter only when it enforces a Base protocol
rule, translates an externally visible Base contract, or isolates an upstream
compatibility boundary. Do not duplicate Reth's lifecycle, provider, payload,
or RPC plumbing just to add a forwarding layer.

## Product direction and retirement commitments

Base is converging on a smaller, faster, and more testable production system.
A change should improve a user- or operator-visible correctness, reliability,
security, latency, throughput, or resource-use outcome; complete a planned
vertical slice; retire a superseded path; or make important production behavior
reproducibly observable. Do not propose a speculative parallel protocol path or
documentation-only cleanup without an explicit request.

Current product areas are upgrade delivery; EIP-8130 and validity
transactions; native assets and policies; proof production and disputes; and
node/operator experience. Favor deleting and consolidating, measuring supported
hot paths, exercising an ingress-to-execution-to-batching/proof vertical slice,
and shortening deterministic feedback loops.

### Transition-system convergence

Retire inherited Optimism-era behavior that predates the supported Holocene
path when its migration condition and replacement behavior are explicit and
tested. Do not retain compatibility branches, flags, mappings, or special cases
solely because they are established.

For upgrade, derivation, execution, and operator flows, keep state transitions
with one canonical owner. Define meaningful states, inputs/events, legal and
terminal transitions, rejection behavior, and recovery. Expose active state,
transition reason, and failure reason through operator-facing status, metrics,
or structured logs. A new abstraction is justified only when it clarifies that
state machine or eliminates real duplication; it must not conceal hardfork
conditions or Base-versus-upstream ownership.

Before a PR, state the affected user or operator, observable outcome, roadmap
fit (including any Flashblocks retirement impact), obsolete surface avoided or
removed, focused validation, and whether durable documentation is actually
needed. Performance claims need a representative baseline and repeatable
comparison. For block-production-sensitive work, also follow
`docs/guides/BLOCK_PRODUCTION_REVIEW.md`.

## Keeping this map current

The generated inventory below is the complete package-level index of repository
Cargo packages and the direct Reth dependency boundary. It is maintained by
`etc/scripts/ci/check_feature_map.py`; do not hand-edit the marked block.

Run this before committing a package move, addition, deletion, or Reth pin/set
change:

```sh
python3 etc/scripts/ci/check_feature_map.py --write
python3 etc/scripts/ci/check_feature_map.py --check
```

The `Feature map` pull-request workflow runs on every pull request. If the
complete generated block is already in the PR diff, it posts a GitHub review
with a one-click replacement suggestion when the inventory is stale. Otherwise
it posts the exact patch and local regeneration command in the review; GitHub
only permits one-click suggestions for lines already changed by the PR. It
intentionally does not attempt to judge architecture from names alone: changes
to the prose above remain a reviewer responsibility whenever responsibilities
or boundaries change.

<!-- BEGIN GENERATED FEATURE MAP INVENTORY -->
### Generated coverage inventory

This inventory is generated from package manifests and the root Reth dependency set. It is deliberately an index, not an ownership chart or a dependency graph.

#### Repository Cargo packages
- **Action harness** — `actions/harness/**`: `base-action-harness`
- **Operational binaries** — `bin/**`: `audit-archiver`, `base`, `base-builder-bin`, `base-challenger-bin`, `base-challenger-e2e-bin`, `base-consensus`, `base-load-tester-bin`, `base-proof-tee-registrar-bin`, `base-proposer-bin`, `base-prover-nitro-enclave`, `base-prover-nitro-host`, `base-prover-service-bin`, `base-prover-zk-host`, `base-reth-node`, `base-roxy-bin`, `base-shadow-metrics-bin`, `base-sidecrush-bin`, `base-snapshotter-bin`, `base-snark-e2e-bin`, `base-telemetry-bin`, `base-zk-benchmarks-bin`, `base-zk-fork-dispute-bin`, `basectl`, `websocket-proxy-bin`
- **L1 batching** — `crates/batcher/**`: `base-batcher-admin`, `base-batcher-cli`, `base-batcher-core`, `base-batcher-encoder`, `base-batcher-service`, `base-batcher-source`, `base-blobs`, `base-comp`
- **Payload building** — `crates/builder/**`: `base-builder-cli`, `base-builder-core`, `base-builder-metering`, `base-builder-multiplex`, `base-builder-publish`
- **Shared protocol and primitives** — `crates/common/**`: `base-bundles`, `base-common-chains`, `base-common-consensus`, `base-common-eip8130`, `base-common-evm`, `base-common-evm2`, `base-common-flashblocks`, `base-common-flz`, `base-common-genesis`, `base-common-l1-fees`, `base-common-network`, `base-common-precompiles`, `base-common-rpc-types`, `base-common-rpc-types-engine`, `base-common-signer`, `base-observability-events`, `base-precompile-macros`, `base-precompile-storage`
- **Rollup consensus** — `crates/consensus/**`: `base-consensus-cli`, `base-consensus-derive`, `base-consensus-disc`, `base-consensus-engine`, `base-consensus-gossip`, `base-consensus-node`, `base-consensus-peers`, `base-consensus-providers`, `base-consensus-rpc`, `base-consensus-safedb`, `base-consensus-sources`, `base-consensus-upgrades`, `base-protocol`
- **Execution node** — `crates/execution/**`: `base-execution-chainspec`, `base-execution-cli`, `base-execution-consensus`, `base-execution-eip8130`, `base-execution-eip8130-rpc`, `base-execution-eip8130-rpc-node`, `base-execution-evm`, `base-execution-exex`, `base-execution-payload-builder`, `base-execution-rpc`, `base-execution-trie`, `base-execution-txpool`, `base-flashblocks`, `base-flashblocks-node`, `base-metering`, `base-node-core`, `base-node-runner`, `base-proofs-extension`, `base-shadow-indexer`, `base-shadow-indexer-db`, `base-tx-forwarding`, `base-txpool-rpc`, `base-txpool-tracing`
- **Operations and E2E tools** — `crates/infra/**`: `audit-archiver-lib`, `base-challenger-e2e`, `base-load-tests`, `base-roxy`, `base-shadow-metrics`, `base-sidecrush`, `base-snapshotter`, `base-snark-e2e`, `base-telemetry-service`, `base-zk-benchmarks`, `base-zk-fork-dispute`, `basectl-cli`, `websocket-proxy`
- **Prover-service API** — `crates/proof/prover-service/**`: `base-prover-service`, `base-prover-service-client`, `base-prover-service-db`, `base-prover-service-protocol`
- **TEE proving** — `crates/proof/tee/**`: `base-proof-tee-nitro-enclave`, `base-proof-tee-nitro-host`, `base-proof-tee-nitro-verifier`, `base-proof-tee-registrar`
- **ZK proving** — `crates/proof/zk/**`: `aggregation`, `base-proof-succinct-elfs`, `base-proof-succinct-range-utils`, `base-proof-succinct-scripts`, `base-proof-zk-backend`, `base-proof-zk-host`, `base-proof-zk-utils`, `base-proof-zk-witness`, `range`
- **Proofs and disputes** — `crates/proof/**`: `base-challenger`, `base-proof`, `base-proof-client`, `base-proof-contracts`, `base-proof-driver`, `base-proof-executor`, `base-proof-host`, `base-proof-mpt`, `base-proof-preimage`, `base-proof-primitives`, `base-proof-rpc`, `base-proof-submission`, `base-proof-worker`, `base-proposer`
- **Shared utilities** — `crates/utilities/**`: `base-balance-monitor`, `base-cli-utils`, `base-health`, `base-jwt`, `base-l1-head`, `base-metrics`, `base-optimism-rpc`, `base-reth-cli`, `base-retry`, `base-ring-buffer`, `base-runtime`, `base-test-utils`, `base-trusted-proxy`, `base-tx-manager`, `base-upgrade-signal`
- **System-test harness** — `etc/systems/**`: `base-system-tests`
- **Developer tools** — `etc/tools/**`: `base-witness-diff`

#### Reth packages used directly by this workspace
Base Reth is pinned at **`base-v2.5.2.6`** where the dependency is git-sourced. Version-only compatibility crates remain listed because they are part of the integration boundary.
- **Node assembly and CLI:** `reth-cli`, `reth-cli-commands`, `reth-cli-runner`, `reth-cli-util`, `reth-node-api`, `reth-node-builder`, `reth-node-core`, `reth-node-ethereum`, `reth-node-metrics`
- **Payload, EVM, and execution:** `reth-basic-payload-builder`, `reth-evm`, `reth-evm-ethereum`, `reth-execution-cache`, `reth-execution-errors`, `reth-execution-types`, `reth-exex`, `reth-exex-test-utils`, `reth-payload-builder`, `reth-payload-builder-primitives`, `reth-payload-primitives`, `reth-payload-util`, `reth-payload-validator`, `reth-revm`, `reth-transaction-pool`
- **State, database, and trie:** `reth-chain-state`, `reth-db`, `reth-db-api`, `reth-db-common`, `reth-provider`, `reth-prune-types`, `reth-stages-types`, `reth-storage-api`, `reth-storage-errors`, `reth-trie`, `reth-trie-common`, `reth-trie-db`, `reth-trie-parallel`
- **RPC:** `reth-rpc`, `reth-rpc-api`, `reth-rpc-convert`, `reth-rpc-engine-api`, `reth-rpc-eth-api`, `reth-rpc-eth-types`, `reth-rpc-layer`, `reth-rpc-server-types`
- **P2P and discovery:** `reth-discv4`, `reth-discv5`, `reth-ecies`, `reth-eth-wire`, `reth-eth-wire-types`, `reth-net-nat`, `reth-network`, `reth-network-api`, `reth-network-p2p`, `reth-network-peers`
- **Consensus, primitives, and forks:** `reth-chainspec`, `reth-codecs`, `reth-consensus`, `reth-consensus-common`, `reth-ethereum-forks`, `reth-ethereum-primitives`, `reth-primitives-traits`, `reth-zstd-compressors`
- **Runtime, tracing, and test support:** `reth-e2e-test-utils`, `reth-engine-tree`, `reth-errors`, `reth-ipc`, `reth-tasks`, `reth-testing-utils`, `reth-tracing`, `reth-tracing-otlp`
<!-- END GENERATED FEATURE MAP INVENTORY -->
