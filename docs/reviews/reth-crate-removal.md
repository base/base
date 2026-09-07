# Vendored Reth crate removal review

Reviewed against `8425d3a07`, with the authorized initial removals applied in this working tree.

## Scope and result

Inventoried all **109 vendored `reth-*` crates** using workspace manifests, Cargo metadata, and the resolved Base dependency tree. Inspected source consumers for removal candidates and the shared functionality that blocks deletion. This is a crate/dependency architecture review, not a line-by-line correctness audit of every Reth implementation.

Before this change, `cargo tree --offline -p base -e normal,build` reached **106 Reth crates**. After this change it reaches **100**. The three crates originally outside that production graph were `reth-e2e-test-utils`, `reth-exex-test-utils`, and `reth-testing-utils`.

“Base only” is interpreted as a Base execution implementation, preserving Base mainnet, Base Sepolia, Base Zeronet, and local Base development/testing. It does not make Ethereum-compatible transactions, hardfork rules, execution-layer networking, database maintenance, or L1 interaction obsolete.

## Applied removals

- **Deleted `reth-era`, `reth-era-downloader`, and `reth-era-utils`.** Removed ERA import/export command implementations, the ERA stage, stage selection, pipeline arguments, node flags, ERA configuration, workspace dependencies, and associated fixtures. Base's exposed maintenance command enums did not offer import/export-era, but ERA was still compiled and available through node configuration and pipeline setup.
- **Removed `reth-node-ethereum` from `base-builder-core`.** No Rust source reference to that dependency was found in the builder crate. Removed its test-utils feature forwarding as well. This also removes `reth-ethereum-consensus` and `reth-ethereum-payload-builder` from the Base production graph.
- **Kept the three Ethereum implementation crates in the workspace for existing tests.** `reth-e2e-test-utils/src/setup_import.rs`, `reth-exex-test-utils/src/lib.rs`, and engine/builder/RPC tests still use the Ethereum node. Stages and engine-tree tests also use Ethereum consensus. These are concrete blockers to deleting those directories, separate from removing them from the binary build.
- Removed dependencies left unused by ERA deletion. `url` moved to a dev-dependency of `reth-config` because its bootnode tests still use it.
- Made the existing `revm/p256-aws-lc-rs` production feature explicit in `base-node-core`; previously the unused Ethereum node dependency enabled it. Removing a dependency should not silently change the cryptographic backend used by Base.

Older configuration files containing `[stages.era]` can still load via the existing unknown-field handling. Saving configuration drops that obsolete section. `--era.enable`, `--era.path`, and `--era.url` are rejected. Stage checkpoints use string keys, so removing the ERA identifier does not renumber the remaining checkpoint keys.

## Next removal candidates

### Ethereum node test fixtures

Migrate the Ethereum-specific fixtures in `reth-e2e-test-utils` and `reth-exex-test-utils`, plus the node-builder, engine-tree, RPC engine, RPC builder, and stages tests. Then delete `reth-node-ethereum`, `reth-ethereum-consensus`, and `reth-ethereum-payload-builder` from the workspace and lockfile. Keep shared storage, execution, and pipeline behavior coverage; merely disabling all those tests would lose useful Base coverage.

A direct dependency from a vendored low-level crate back into `base-node-core` can create a cycle: Base already depends on those crates. Put Base integration fixtures/tests above the execution libraries, or use generic test helpers and small component implementations at the appropriate layer.

### `reth-evm-ethereum`

Base supplies `BaseExecutorProvider`, but shared crates still reference `EthEvmConfig`:

- `reth-transaction-pool/src/lib.rs`: default EVM parameter of `EthTransactionPool`.
- `reth-rpc/src/eth/core.rs` and `eth/helpers/types.rs`: Ethereum convenience builder and converter aliases.
- Network test-utils and several existing execution/storage tests.

Remove the Ethereum conveniences, make shared APIs accept their EVM explicitly, and migrate fixtures. The **generic transaction validator and RPC internals must survive**. `BaseTransactionValidator` wraps `EthTransactionValidator`, and Base RPC uses `reth_rpc::eth::core::EthApiInner`.

### `reth-ethereum-engine-primitives`

Remaining production edges include:

- `reth-payload-builder/src/lib.rs`: re-exports of `BlobSidecars` and `EthBuiltPayload`.
- `reth-engine-local/src/payload.rs`: Ethereum local payload attributes builder. Base already supplies `BaseLocalPayloadAttributesBuilder`.
- `reth-rpc/src/testing.rs`: Ethereum payload construction and default engine types. Wiring for this testing API lives in the Ethereum node implementation.

Remove unused exports and Ethereum-specific builders/testing RPC code, then replace the provider/payload/engine/RPC test defaults. Keep `reth-engine-primitives` and `reth-payload-primitives`: their generic handles, validation, payload contracts, and engine messages are active Base infrastructure.

## Capabilities that are not obsolete just because Base is the only network

| Crate | Actual dependency and removal condition |
| --- | --- |
| `reth-consensus-debug-client` | Used by `reth-node-builder/src/launch/debug.rs` for fetching payloads from RPC/Etherscan and driving Engine API calls. Base's proof-history launch uses debug capabilities. Remove only with the corresponding debug launch modes. |
| `reth-engine-local` | Generic local mining, mining mode, and finality defaults are wired into node configuration and debug launch. Base has its own local attributes builder. Remove the Ethereum attributes implementation first; removing the entire crate also removes or relocates Base local-mining support. |
| `reth-node-ethstats` | Launched through `spawn_ethstats` in the engine launcher and exposed by `--ethstats`. Optional telemetry, not multi-network machinery. Delete if this integration is unwanted. |
| `reth-invalid-block-hooks` | Node builder installs invalid-block witness hooks. This is useful for Base execution diagnostics; deletion removes that behavior. |
| `reth-engine-util` | Engine launcher uses `EngineMessageStreamExt`; the crate implements engine-message recording/skipping/reorg debugging. Remove with those debugging features, or absorb the needed parts. |

## Crate boundaries that can be consolidated

These are opportunities to eliminate standalone packages, **not to delete their behavior**. Use `base-*` names for resulting crates. Preserve dependency direction and the existing `no_std`/feature boundaries.

| Current crates | Proposed direction |
| --- | --- |
| `reth-cli`, `reth-cli-commands`, `reth-cli-runner`, `reth-cli-util` | Move the Base parser, selected maintenance commands, runtime setup, and CLI utilities into Base CLI libraries. Specialize `ChainSpecParser` and generic command parameters. Keep database init, stage maintenance, prune, and re-execute: `bin/base/src/commands/reth.rs` dispatches them today. |
| `reth-node-api`, `reth-node-types` | Consolidate node type bundles and adapters in a lower-level Base execution API crate; remove arbitrary-network type builders after concrete Base types are wired through. Do not move them into an upper-level node crate that already consumes provider/engine crates. |
| `reth-chainspec`, `reth-ethereum-forks` | Remove Ethereum mainnet/Sepolia/Holesky/Hoodi presets and adopt a Base-oriented chain-spec implementation. Retain fork IDs/filtering, activation rules, fee parameters, and required traits. `BaseChainSpec` currently wraps `reth_chainspec::ChainSpec`, so these crates cannot simply be dropped. |
| `reth-ethereum-primitives` | Replace wrapper aliases with direct Alloy types where appropriate and replace `EthPrimitives` defaults with Base primitives in execution code. Preserve ordinary Ethereum transaction compatibility and L1 use. This small crate is largely aliases, but many storage/network/default/test types still reference it. |
| `reth-payload-builder-primitives`, `reth-payload-util` | Fold payload events and transaction iteration helpers into a compatible payload library. Base's payload builder and debug RPC use the iteration helpers. |
| `reth-rpc-traits` | Fold conversion traits into an appropriate lower-level RPC conversion/types crate if its `no_std` consumers remain supported. |
| `reth-errors` | Replace the facade's re-exports with direct imports; move its aggregate error/result types to a compatible lower-level error module if still needed. |
| `reth-db-models` | Consider consolidating storage model types with a lower-level storage/codec package. Preserve persisted encodings and avoid database/provider dependency cycles. |

`reth-node-builder` and `reth-node-core` also contain substantial network-agnostic construction abstractions worth specializing. Their engine launch, RPC/network setup, configuration, and task lifecycle code remains required; they are not whole-crate deletion candidates in the first pass.

## Shared infrastructure to retain

- **Execution and consensus:** `reth-consensus-common` supplies validation routines used directly by Base consensus; generic EVM/execution/revm code remains essential.
- **ExEx:** `base-execution-exex` uses `reth-exex` for proof history, and `base-shadow-indexer` also consumes ExEx. The manager, notifications, WAL, and pruning coordination are not unused plugin machinery.
- **Networking:** discovery v4/v5, DNS discovery, peer handling, ETH wire protocol, ECIES, downloads, NAT, and ban lists support Base's execution network. Remove unrelated bootnode presets inside `reth-network-peers`, not the whole crate.
- **Storage:** provider, DB APIs/backends, codecs, ETL, pruning, stage processing, trie implementations, static files, and compression remain active. A single supported network does not remove the need for sync, recovery, persisted formats, or historical reads.
- **RPC and payload services:** Base reuses the shared engines, servers, caches, transaction pool, payload scheduling, and RPC implementation pieces.
- **Observability and runtime:** tracing/OTLP, metrics, events, tasks, and Tokio helpers implement active Base behavior.

## Complete inventory

The following table accounts for all 109 original Reth crates. “Retain shared infrastructure” means no whole-crate removal justified by the single-network scope was found. It does not rule out removing unused internal modules or future consolidation.

| Original crate | Disposition |
| --- | --- |
| [reth-basic-payload-builder](../../vendor/reth-basic-payload-builder/Cargo.toml) | Retain shared infrastructure |
| [reth-chain-state](../../vendor/reth-chain-state/Cargo.toml) | Retain shared infrastructure |
| [reth-chainspec](../../vendor/reth-chainspec/Cargo.toml) | Consolidate; retain required code |
| [reth-cli](../../vendor/reth-cli/Cargo.toml) | Consolidate; retain required code |
| [reth-cli-commands](../../vendor/reth-cli-commands/Cargo.toml) | Consolidate; retain required code |
| [reth-cli-runner](../../vendor/reth-cli-runner/Cargo.toml) | Consolidate; retain required code |
| [reth-cli-util](../../vendor/reth-cli-util/Cargo.toml) | Consolidate; retain required code |
| [reth-codecs](../../vendor/reth-codecs/Cargo.toml) | Retain shared infrastructure |
| [reth-codecs-derive](../../vendor/reth-codecs-derive/Cargo.toml) | Retain shared infrastructure |
| [reth-config](../../vendor/reth-config/Cargo.toml) | Retain shared infrastructure |
| [reth-consensus](../../vendor/reth-consensus/Cargo.toml) | Retain shared infrastructure |
| [reth-consensus-common](../../vendor/reth-consensus-common/Cargo.toml) | Retain shared infrastructure |
| [reth-consensus-debug-client](../../vendor/reth-consensus-debug-client/Cargo.toml) | Optional capability; separate removal decision |
| [reth-db](../../vendor/reth-db/Cargo.toml) | Retain shared infrastructure |
| [reth-db-api](../../vendor/reth-db-api/Cargo.toml) | Retain shared infrastructure |
| [reth-db-common](../../vendor/reth-db-common/Cargo.toml) | Retain shared infrastructure |
| [reth-db-models](../../vendor/reth-db-models/Cargo.toml) | Consolidate; retain required code |
| [reth-discv4](../../vendor/reth-discv4/Cargo.toml) | Retain shared infrastructure |
| [reth-discv5](../../vendor/reth-discv5/Cargo.toml) | Retain shared infrastructure |
| [reth-dns-discovery](../../vendor/reth-dns-discovery/Cargo.toml) | Retain shared infrastructure |
| [reth-downloaders](../../vendor/reth-downloaders/Cargo.toml) | Retain shared infrastructure |
| [reth-e2e-test-utils](../../vendor/reth-e2e-test-utils/Cargo.toml) | Test support; retain or migrate |
| [reth-ecies](../../vendor/reth-ecies/Cargo.toml) | Retain shared infrastructure |
| [reth-engine-local](../../vendor/reth-engine-local/Cargo.toml) | Optional capability; separate removal decision |
| [reth-engine-primitives](../../vendor/reth-engine-primitives/Cargo.toml) | Retain shared infrastructure |
| [reth-engine-tree](../../vendor/reth-engine-tree/Cargo.toml) | Retain shared infrastructure |
| [reth-engine-util](../../vendor/reth-engine-util/Cargo.toml) | Optional capability; separate removal decision |
| reth-era | Deleted |
| reth-era-downloader | Deleted |
| reth-era-utils | Deleted |
| [reth-errors](../../vendor/reth-errors/Cargo.toml) | Consolidate; retain required code |
| [reth-eth-wire](../../vendor/reth-eth-wire/Cargo.toml) | Retain shared infrastructure |
| [reth-eth-wire-types](../../vendor/reth-eth-wire-types/Cargo.toml) | Retain shared infrastructure |
| [reth-ethereum-consensus](../../vendor/reth-ethereum-consensus/Cargo.toml) | Removed from Base build; test migration remains |
| [reth-ethereum-engine-primitives](../../vendor/reth-ethereum-engine-primitives/Cargo.toml) | Remove after Ethereum implementation cleanup |
| [reth-ethereum-forks](../../vendor/reth-ethereum-forks/Cargo.toml) | Consolidate; retain required code |
| [reth-ethereum-payload-builder](../../vendor/reth-ethereum-payload-builder/Cargo.toml) | Removed from Base build; test migration remains |
| [reth-ethereum-primitives](../../vendor/reth-ethereum-primitives/Cargo.toml) | Consolidate; retain required code |
| [reth-etl](../../vendor/reth-etl/Cargo.toml) | Retain shared infrastructure |
| [reth-evm](../../vendor/reth-evm/Cargo.toml) | Retain shared infrastructure |
| [reth-evm-ethereum](../../vendor/reth-evm-ethereum/Cargo.toml) | Remove after Ethereum implementation cleanup |
| [reth-execution-cache](../../vendor/reth-execution-cache/Cargo.toml) | Retain shared infrastructure |
| [reth-execution-errors](../../vendor/reth-execution-errors/Cargo.toml) | Retain shared infrastructure |
| [reth-execution-types](../../vendor/reth-execution-types/Cargo.toml) | Retain shared infrastructure |
| [reth-exex](../../vendor/reth-exex/Cargo.toml) | Retain shared infrastructure |
| [reth-exex-test-utils](../../vendor/reth-exex-test-utils/Cargo.toml) | Test support; retain or migrate |
| [reth-exex-types](../../vendor/reth-exex-types/Cargo.toml) | Retain shared infrastructure |
| [reth-fs-util](../../vendor/reth-fs-util/Cargo.toml) | Retain shared infrastructure |
| [reth-invalid-block-hooks](../../vendor/reth-invalid-block-hooks/Cargo.toml) | Optional capability; separate removal decision |
| [reth-ipc](../../vendor/reth-ipc/Cargo.toml) | Retain shared infrastructure |
| [reth-libmdbx](../../vendor/reth-libmdbx/Cargo.toml) | Retain shared infrastructure |
| [reth-mdbx-sys](../../vendor/reth-mdbx-sys/Cargo.toml) | Retain shared infrastructure |
| [reth-metrics](../../vendor/reth-metrics/Cargo.toml) | Retain shared infrastructure |
| [reth-net-banlist](../../vendor/reth-net-banlist/Cargo.toml) | Retain shared infrastructure |
| [reth-net-nat](../../vendor/reth-net-nat/Cargo.toml) | Retain shared infrastructure |
| [reth-network](../../vendor/reth-network/Cargo.toml) | Retain shared infrastructure |
| [reth-network-api](../../vendor/reth-network-api/Cargo.toml) | Retain shared infrastructure |
| [reth-network-p2p](../../vendor/reth-network-p2p/Cargo.toml) | Retain shared infrastructure |
| [reth-network-peers](../../vendor/reth-network-peers/Cargo.toml) | Retain shared infrastructure |
| [reth-network-types](../../vendor/reth-network-types/Cargo.toml) | Retain shared infrastructure |
| [reth-nippy-jar](../../vendor/reth-nippy-jar/Cargo.toml) | Retain shared infrastructure |
| [reth-node-api](../../vendor/reth-node-api/Cargo.toml) | Consolidate; retain required code |
| [reth-node-builder](../../vendor/reth-node-builder/Cargo.toml) | Retain shared infrastructure |
| [reth-node-core](../../vendor/reth-node-core/Cargo.toml) | Retain shared infrastructure |
| [reth-node-ethereum](../../vendor/reth-node-ethereum/Cargo.toml) | Removed from Base build; test migration remains |
| [reth-node-ethstats](../../vendor/reth-node-ethstats/Cargo.toml) | Optional capability; separate removal decision |
| [reth-node-events](../../vendor/reth-node-events/Cargo.toml) | Retain shared infrastructure |
| [reth-node-metrics](../../vendor/reth-node-metrics/Cargo.toml) | Retain shared infrastructure |
| [reth-node-types](../../vendor/reth-node-types/Cargo.toml) | Consolidate; retain required code |
| [reth-payload-builder](../../vendor/reth-payload-builder/Cargo.toml) | Retain shared infrastructure |
| [reth-payload-builder-primitives](../../vendor/reth-payload-builder-primitives/Cargo.toml) | Consolidate; retain required code |
| [reth-payload-primitives](../../vendor/reth-payload-primitives/Cargo.toml) | Retain shared infrastructure |
| [reth-payload-util](../../vendor/reth-payload-util/Cargo.toml) | Consolidate; retain required code |
| [reth-payload-validator](../../vendor/reth-payload-validator/Cargo.toml) | Retain shared infrastructure |
| [reth-primitives-traits](../../vendor/reth-primitives-traits/Cargo.toml) | Retain shared infrastructure |
| [reth-provider](../../vendor/reth-provider/Cargo.toml) | Retain shared infrastructure |
| [reth-prune](../../vendor/reth-prune/Cargo.toml) | Retain shared infrastructure |
| [reth-prune-types](../../vendor/reth-prune-types/Cargo.toml) | Retain shared infrastructure |
| [reth-revm](../../vendor/reth-revm/Cargo.toml) | Retain shared infrastructure |
| [reth-rpc](../../vendor/reth-rpc/Cargo.toml) | Retain shared infrastructure |
| [reth-rpc-api](../../vendor/reth-rpc-api/Cargo.toml) | Retain shared infrastructure |
| [reth-rpc-builder](../../vendor/reth-rpc-builder/Cargo.toml) | Retain shared infrastructure |
| [reth-rpc-convert](../../vendor/reth-rpc-convert/Cargo.toml) | Retain shared infrastructure |
| [reth-rpc-engine-api](../../vendor/reth-rpc-engine-api/Cargo.toml) | Retain shared infrastructure |
| [reth-rpc-eth-api](../../vendor/reth-rpc-eth-api/Cargo.toml) | Retain shared infrastructure |
| [reth-rpc-eth-types](../../vendor/reth-rpc-eth-types/Cargo.toml) | Retain shared infrastructure |
| [reth-rpc-layer](../../vendor/reth-rpc-layer/Cargo.toml) | Retain shared infrastructure |
| [reth-rpc-server-types](../../vendor/reth-rpc-server-types/Cargo.toml) | Retain shared infrastructure |
| [reth-rpc-traits](../../vendor/reth-rpc-traits/Cargo.toml) | Consolidate; retain required code |
| [reth-stages](../../vendor/reth-stages/Cargo.toml) | Retain shared infrastructure |
| [reth-stages-api](../../vendor/reth-stages-api/Cargo.toml) | Retain shared infrastructure |
| [reth-stages-types](../../vendor/reth-stages-types/Cargo.toml) | Retain shared infrastructure |
| [reth-static-file](../../vendor/reth-static-file/Cargo.toml) | Retain shared infrastructure |
| [reth-static-file-types](../../vendor/reth-static-file-types/Cargo.toml) | Retain shared infrastructure |
| [reth-storage-api](../../vendor/reth-storage-api/Cargo.toml) | Retain shared infrastructure |
| [reth-storage-errors](../../vendor/reth-storage-errors/Cargo.toml) | Retain shared infrastructure |
| [reth-storage-overlay](../../vendor/reth-storage-overlay/Cargo.toml) | Retain shared infrastructure |
| [reth-tasks](../../vendor/reth-tasks/Cargo.toml) | Retain shared infrastructure |
| [reth-testing-utils](../../vendor/reth-testing-utils/Cargo.toml) | Test support; retain or migrate |
| [reth-tokio-util](../../vendor/reth-tokio-util/Cargo.toml) | Retain shared infrastructure |
| [reth-tracing](../../vendor/reth-tracing/Cargo.toml) | Retain shared infrastructure |
| [reth-tracing-otlp](../../vendor/reth-tracing-otlp/Cargo.toml) | Retain shared infrastructure |
| [reth-transaction-pool](../../vendor/reth-transaction-pool/Cargo.toml) | Retain shared infrastructure |
| [reth-trie](../../vendor/reth-trie/Cargo.toml) | Retain shared infrastructure |
| [reth-trie-common](../../vendor/reth-trie-common/Cargo.toml) | Retain shared infrastructure |
| [reth-trie-db](../../vendor/reth-trie-db/Cargo.toml) | Retain shared infrastructure |
| [reth-trie-parallel](../../vendor/reth-trie-parallel/Cargo.toml) | Retain shared infrastructure |
| [reth-trie-sparse](../../vendor/reth-trie-sparse/Cargo.toml) | Retain shared infrastructure |
| [reth-zstd-compressors](../../vendor/reth-zstd-compressors/Cargo.toml) | Retain shared infrastructure |

## Validation

The review uses the host/default Base build, not an all-target/all-feature guarantee. Test-only Ethereum crates can re-enter when compiling tests or enabling test-utils.

Passed:

- `cargo check --offline --locked -p base --all-targets`
- `cargo test --offline -p reth-config --features serde --lib` — 16 tests, including loading and saving old ERA configuration.
- `cargo test --offline -p reth-stages-types --features reth-codecs/alloy --lib` — 17 tests. The explicit codec feature supplies the Alloy codec implementations needed by this isolated test build.
- `cargo test --offline -p reth-stages --features test-utils --test pipeline` — full forward sync, unwind, and re-sync test.
- `cargo test --offline --locked -p reth-stages --features test-utils --test preimage` — 7 storage/preimage pipeline tests.
- `cargo test --offline -p base-execution-cli --lib node::tests` — 37 matching tests, including rejection of ERA CLI flags.
- Formatting of affected packages and `git diff --check`.
- Dependency-tree comparison confirms all six named crates are absent from Base's normal/build graph. Searches found no remaining ERA production references.

Total: **78 selected tests passed**. Existing vendored warnings remain; the entire workspace test suite and all feature/platform combinations were not run.
