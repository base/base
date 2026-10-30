# Base network upgrades and protocol features

This guide describes Base hardforks across derivation, execution, sequencing, batching,
RPC, and proofs. It explains how to determine activation on a particular network and
how to change consensus-critical behavior using the checked feature registry.
It does not publish a mutable list of which networks are currently activated.

## Features describe behavior; upgrades schedule it

[`BaseFeature`](../../crates/common/genesis/src/feature.rs) is the single registry of
Base protocol behaviors, grouped by the hardfork that introduces them. Each hardfork
block lists its new features and their contracts, not a cumulative feature set.
Consumers check a feature, not a hardfork name. These are consensus rules, **not Cargo features or
operator-selectable flags**. Moving a feature to another upgrade changes consensus.

- `RollupConfig::is_feature_active_cascading(feature, timestamp)` checks the
  rollup schedule, including its historical cascade and runtime overrides.
- `Upgrades::is_feature_active_at_timestamp(feature, timestamp)` checks execution
  fork conditions. `BaseSpecId::is_feature_enabled(feature)` checks a resolved EVM
  spec. Do not replace one adapter with another: sparse and non-monotonic schedules
  have historically different cascade semantics.
- `BaseForkActivations::is_feature_active_at_timestamp(feature, timestamp)` in
  [EVM2](../../crates/common/evm2/src/transition.rs) uses its schedule implementation's
  activation rules. The `UpgradeConfig` implementation reads explicit timestamps
  without cascading: a configured Delta does not imply Canyon. Bedrock features
  return false because Bedrock has no timestamp slot.
- `RollupConfig::is_feature_activation_block_cascading` and
  `Upgrades::is_feature_activation_block` detect transitions using their respective
  activation semantics and the **actual parent timestamp**, not an assumed interval.
- `RollupConfig::configured_feature_activation_timestamp` reads the runtime-aware
  configured boundary without cascading. `Upgrades::feature_activation_timestamp`
  reads the resolved execution fork condition. An absent configured timestamp is
  not proof that a cascaded rollup feature is disabled; use the boolean query for
  behavior. The distinct names make the semantics explicit for concrete, `Arc`,
  and generic receivers.

A feature covers a cohesive protocol change across its consumers, not a separate
switch for every layer. Fee calculation and the L1 attributes supplying its inputs
share a feature; callers still retain their different timestamp sources and
first-block format exceptions. Independent rules stay separate even when they
activate together.

Runtime schedules can change between reads. Preserve captured boundaries, shared
decisions, query order, and short-circuiting when migrating a gate. Where one old
decision controls several features, `BaseFeature::activation_states` evaluates
their shared upgrade once using the caller's activation adapter. It does not make
different upgrades atomic and must not combine historically separate reads.
RPC simulation retains one captured boundary for the whole sanitization pass.

The L1 blob data source intentionally captures its configured feature boundary at
construction; it does not follow subsequent runtime overrides or rollup cascades.
Its boundary is an L1 timestamp, not an L2
format-activation check. See [the data source](../../crates/consensus/derive/src/sources/ethereum.rs).

The adapters live in [rollup configuration](../../crates/common/genesis/src/rollup.rs),
the [execution upgrade trait](../../crates/common/chains/src/upgrades.rs), and
the [EVM spec](../../crates/common/evm/src/spec.rs). Fork names remain legitimate in
schedule parsing, L1 signal identities, configuration, upgrade transaction bundles,
Ethereum spec compatibility, logging, and historical test fixtures. Ethereum's
`EthereumHardfork` / `SpecId` gates are not Base feature gates.

## Determine activation for a network and block

1. Identify the L2 chain ID and the exact L2 block and parent. Use chain timestamps,
   not wall-clock time. Mainnet, Sepolia, devnet, and zeronet defaults are defined in
   [`ChainConfig`](../../crates/common/chains/src/config.rs); custom genesis and CLI
   configuration can differ from those defaults.
2. Determine the node's upgrade-signal mode and L1 observation block tag. In
   `metrics-only`, the observed contract schedule does **not** change activation.
   `startup-apply` pins a startup schedule; `runtime-admin` can apply later changes.
   Read the [upgrade-signal guide](../../crates/utilities/upgrade-signal/README.md)
   for flags, network contract addresses, and metrics. The read-only
   [`base_upgradeReadiness`](../../crates/consensus/rpc/src/jsonrpsee.rs) RPC and
   [`basectl upgrade-readiness --cl-rpc <URL> --json`](../../crates/infra/basectl/src/commands/upgrade_readiness.rs)
   report schedule/version readiness, not proof that a feature has executed.
   Inspect both consensus and execution nodes; observing an announcement is not
   evidence that both nodes applied it.
3. Resolve the effective schedule, including [`RuntimeUpgradeRegistry` overrides](../../crates/common/genesis/src/chain/upgrade.rs).
   `UpgradeActivation::Never` explicitly disables a configured activation. A zero
   **contract signal** means unscheduled, while timestamp zero in **genesis** means
   active from genesis. Do not conflate these encodings or infer activation from
   the enum's default/`LATEST` value.
4. Look up the feature in the registry and evaluate it through the adapter used by
   the relevant consumer. For activation transactions and L1-info layouts, also
   check the parent: some formats start only **after** the transition block.
   For millisecond blocks, use the [timestamp schedule](../../crates/common/genesis/src/schedule.rs)
   and header's millisecond component rather than adding the legacy block interval.
5. For a historical proof, use the schedule pinned to its L1 origin, not today's
   in-process runtime schedule. See [proof schedule pinning](../../crates/proof/proof/src/schedule_id.rs).

Contract upgrade registration is append-only and position-sensitive. Bedrock is
block-activated; Delta and PectraBlobSchedule are not EVM execution-spec entries.
Zenith is genesis-only and cannot be enabled by an L1 runtime signal.

## Change a feature or add an upgrade

Load the [managing-upgrades skill](../../.agents/skills/managing-upgrades/SKILL.md)
for protocol changes, activation-sensitive bug fixes, and consensus-critical review.
Benchmark-only changes that do not alter protocol behavior do not require it.

1. Reuse a feature when changing the same behavior. Otherwise add a semantic entry
   and its contract description to `BaseFeature`. Include the inputs and transition
   work belonging to that rule rather than adding one feature per implementation
   layer. Distinct behaviors must not be coupled just because they happen to ship
   in the same upgrade.
2. For a new upgrade, extend [the canonical enum and configuration](../../crates/common/genesis/src/chain/upgrade.rs),
   schedule resolution, [chain defaults](../../crates/common/chains/src/config.rs),
   and [execution fork mapping](../../crates/common/chains/src/upgrade.rs).
   Preserve contract registration order, distinguish execution and contract-only
   upgrades, and explicitly decide whether rollup activation cascades. Add a guide
   section with `FEATURES` markers matching the upgrade variant.
3. Gate each behavior at its owner with the feature API. Check parity between the
   builder, normal execution, derivation, RPC, and proof execution. Do not move
   activation-block exceptions or silently change pre-fork behavior.
4. Exercise before/at/after activation, genesis activation, absent activation,
   sparse schedules, explicit runtime disabling/rescheduling, and actual-parent
   transitions. Existing historical fixtures should continue to pass. The rollup
   boundary test pins every feature's introducing upgrade independently of the
   registry; add an explicit expectation for each new feature.
5. Run `python3 etc/scripts/local/upgrade-docs.py sync`. Review the affected prose,
   registry contracts, and linked production code. Update prose whenever activation
   semantics or feature behavior changes, even if feature names and generated
   tables stay unchanged. Then run `just check::upgrade-docs`.

The check rejects missing or duplicate upgrade sections, unknown or unused features,
stale generated tables, broken local links, and common named-fork behavior checks
outside schedule adapters. Tables are compared directly with the registry's feature
names, activation upgrades, descriptions, and detected production consumer links.
Gate and consumer scans ignore comments and literals, including raw strings and
nested block comments. They are structural scans, not Rust name resolution or
macro expansion.
`sync` updates only those tables; it does not mark prose reviewed.

No source hashes or review stamps are stored. Logging, formatting, and refactoring
that leave the inventory unchanged do not require documentation edits. A behavior
change behind an unchanged feature contract can also pass these structural checks:
protocol review and behavior tests must establish semantic agreement with the prose.

## Upgrade inventory

Each table lists features **introduced** by that upgrade, not a cumulative feature
set. Execution specs inherit earlier features; timestamp queries use their own
schedule semantics. The linked files identify all detected production consumers,
including consensus and proof code where applicable. Rustdoc in the registry owns
the short behavior descriptions; the prose here explains cross-stack implications.

### Bedrock

Bedrock is the baseline rollup execution and derivation protocol: deposits, L1
attributes, and L1 data fees. It is block-activated rather than timestamp-activated.
The [fork mapping](../../crates/common/chains/src/upgrade.rs) pairs its execution
rules with Ethereum Merge; later sections describe departures from this baseline.

<!-- FEATURES:Bedrock:START -->
| Feature | Behavior | Production code |
| --- | --- | --- |
| `MonotonicBlockTimestamps` | Require child block timestamps to advance, subject to the subsecond schedule exception. | [`crates/execution/consensus/src/lib.rs`](../../crates/execution/consensus/src/lib.rs) |
<!-- FEATURES:Bedrock:END -->

### Regolith

Regolith changes deposit gas accounting, system-transaction handling, receipt
nonces, and the legacy signature-byte allowance in L1 fee calculation. Receipt and
gas rules must agree between block builders, re-execution, and proof execution.

<!-- FEATURES:Regolith:START -->
| Feature | Behavior | Production code |
| --- | --- | --- |
| `DepositGasAccounting` | Enforce deposit gas limits and reject the legacy system-transaction flag. | [`crates/common/consensus/src/info/variant.rs`](../../crates/common/consensus/src/info/variant.rs), [`crates/common/evm/src/executor/block_executor.rs`](../../crates/common/evm/src/executor/block_executor.rs), [`crates/common/evm/src/handler.rs`](../../crates/common/evm/src/handler.rs), [`crates/common/evm2/src/executor.rs`](../../crates/common/evm2/src/executor.rs) |
| `DepositNonce` | Include the sender's pre-execution nonce in deposit receipts. | [`crates/builder/core/src/flashblocks/context.rs`](../../crates/builder/core/src/flashblocks/context.rs), [`crates/common/evm/src/executor/block_executor.rs`](../../crates/common/evm/src/executor/block_executor.rs), [`crates/execution/consensus/src/proof.rs`](../../crates/execution/consensus/src/proof.rs), [`crates/execution/flashblocks/src/receipt_builder.rs`](../../crates/execution/flashblocks/src/receipt_builder.rs), [`crates/proof/executor/src/builder/assemble.rs`](../../crates/proof/executor/src/builder/assemble.rs) |
| `ActualL1SignatureBytes` | Do not add the legacy signature-byte allowance to L1 data gas. | [`crates/common/l1-fees/src/params.rs`](../../crates/common/l1-fees/src/params.rs) |
<!-- FEATURES:Regolith:END -->

### Canyon

Canyon adds versioned deposit receipts and the CREATE2 deployer transition. Empty
withdrawals fields and changed fee parameters accompany Ethereum Shanghai support.
Receipt-root compatibility retains the historical Regolith-era encoding distinction.

<!-- FEATURES:Canyon:START -->
| Feature | Behavior | Production code |
| --- | --- | --- |
| `DepositReceiptVersion` | Include the version field in deposit receipts. | [`crates/builder/core/src/flashblocks/context.rs`](../../crates/builder/core/src/flashblocks/context.rs), [`crates/common/evm/src/executor/block_executor.rs`](../../crates/common/evm/src/executor/block_executor.rs), [`crates/common/evm2/src/executor.rs`](../../crates/common/evm2/src/executor.rs), [`crates/execution/consensus/src/proof.rs`](../../crates/execution/consensus/src/proof.rs), [`crates/execution/consensus/src/validation/mod.rs`](../../crates/execution/consensus/src/validation/mod.rs), [`crates/execution/flashblocks/src/receipt_builder.rs`](../../crates/execution/flashblocks/src/receipt_builder.rs), [`crates/proof/executor/src/builder/assemble.rs`](../../crates/proof/executor/src/builder/assemble.rs) |
| `Create2Deployer` | Install the deterministic CREATE2 deployer at the transition. | [`crates/common/evm/src/canyon.rs`](../../crates/common/evm/src/canyon.rs), [`crates/common/evm2/src/canyon.rs`](../../crates/common/evm2/src/canyon.rs) |
| `Withdrawals` | Include empty withdrawals in block bodies and payload attributes. | [`crates/builder/core/src/flashblocks/payload.rs`](../../crates/builder/core/src/flashblocks/payload.rs), [`crates/consensus/derive/src/attributes/stateful.rs`](../../crates/consensus/derive/src/attributes/stateful.rs), [`crates/consensus/engine/src/attributes.rs`](../../crates/consensus/engine/src/attributes.rs), [`crates/consensus/service/src/standalone.rs`](../../crates/consensus/service/src/standalone.rs), [`crates/execution/consensus/src/lib.rs`](../../crates/execution/consensus/src/lib.rs), [`crates/execution/evm/src/build.rs`](../../crates/execution/evm/src/build.rs), [`crates/execution/node/src/engine.rs`](../../crates/execution/node/src/engine.rs), [`crates/execution/node/src/node.rs`](../../crates/execution/node/src/node.rs), [`crates/proof/executor/src/builder/assemble.rs`](../../crates/proof/executor/src/builder/assemble.rs), [`crates/proof/proof/src/l2/chain_provider.rs`](../../crates/proof/proof/src/l2/chain_provider.rs) |
| `BaseFeeDenominator` | Use the revised EIP-1559 base fee change denominator. | [`crates/proof/executor/src/builder/env.rs`](../../crates/proof/executor/src/builder/env.rs) |
| `ReadyChannelSelection` | Read the first ready channel rather than blocking on the oldest channel. | [`crates/consensus/derive/src/stages/channel/channel_bank.rs`](../../crates/consensus/derive/src/stages/channel/channel_bank.rs) |
| `GossipPayloadV2` | Publish and accept withdrawal-capable gossip payloads. | [`crates/consensus/gossip/src/handler.rs`](../../crates/consensus/gossip/src/handler.rs) |
<!-- FEATURES:Canyon:END -->

### Delta

Delta permits span batches. It changes batching and derivation, not the EVM spec
ladder. Derivation must reject span batches before activation rather than merely
relying on the batcher to choose the correct encoding.

<!-- FEATURES:Delta:START -->
| Feature | Behavior | Production code |
| --- | --- | --- |
| `SpanBatches` | Permit span batches in derivation. | [`crates/consensus/protocol/src/batch/span.rs`](../../crates/consensus/protocol/src/batch/span.rs) |
<!-- FEATURES:Delta:END -->

### Ecotone

Ecotone adds blob-aware L1 fee calculation, compact L1 attributes, upgrade deposits,
and the blob-capable Engine API/header path alongside Ethereum Cancun. The activation
block still uses the preceding L1 attributes layout where the existing transition
rules require it; genesis activation is not an ordinary transition block.

<!-- FEATURES:Ecotone:START -->
| Feature | Behavior | Production code |
| --- | --- | --- |
| `BlobAwareL1Fees` | Price L1 data with separate calldata and blob fee scalars, encode compact blob-aware L1 attributes after the transition block, and validate blob-aware system config scalars while ignoring legacy overhead. | [`crates/common/consensus/src/info/variant.rs`](../../crates/common/consensus/src/info/variant.rs), [`crates/common/evm/src/l1block.rs`](../../crates/common/evm/src/l1block.rs), [`crates/common/l1-fees/src/params.rs`](../../crates/common/l1-fees/src/params.rs), [`crates/consensus/derive/src/attributes/stateful.rs`](../../crates/consensus/derive/src/attributes/stateful.rs), [`crates/consensus/derive/src/stages/traversal/polling.rs`](../../crates/consensus/derive/src/stages/traversal/polling.rs), [`crates/consensus/service/src/actors/l1_watcher/actor.rs`](../../crates/consensus/service/src/actors/l1_watcher/actor.rs), [`crates/execution/rpc/src/eth/receipt.rs`](../../crates/execution/rpc/src/eth/receipt.rs) |
| `BlobHeader` | Include blob gas fields in L2 headers, initially with zero values. | [`crates/builder/core/src/flashblocks/context.rs`](../../crates/builder/core/src/flashblocks/context.rs), [`crates/execution/consensus/src/lib.rs`](../../crates/execution/consensus/src/lib.rs), [`crates/execution/evm/src/build.rs`](../../crates/execution/evm/src/build.rs), [`crates/proof/executor/src/builder/assemble.rs`](../../crates/proof/executor/src/builder/assemble.rs) |
| `BeaconRoots` | Include the parent beacon block root and apply the beacon-roots system call. | [`crates/consensus/derive/src/attributes/stateful.rs`](../../crates/consensus/derive/src/attributes/stateful.rs), [`crates/consensus/service/src/standalone.rs`](../../crates/consensus/service/src/standalone.rs), [`crates/execution/node/src/node.rs`](../../crates/execution/node/src/node.rs), [`crates/execution/rpc/src/config.rs`](../../crates/execution/rpc/src/config.rs) |
| `ConstantBlobBaseFee` | Set the L2 EVM blob base fee to the protocol's constant minimum. | [`crates/proof/executor/src/builder/env.rs`](../../crates/proof/executor/src/builder/env.rs) |
| `BlobDataSource` | Read blob batch data after the explicitly configured L1 timestamp boundary. | [`crates/consensus/derive/src/sources/ethereum.rs`](../../crates/consensus/derive/src/sources/ethereum.rs) |
| `EngineApiV3` | Use the blob-capable V3 Engine API payload methods. | [`crates/consensus/engine/src/versions.rs`](../../crates/consensus/engine/src/versions.rs) |
| `EcotoneUpgradeTransactions` | Inject the Ecotone contract-upgrade deposit bundle at the transition. | [`crates/consensus/derive/src/attributes/stateful.rs`](../../crates/consensus/derive/src/attributes/stateful.rs), [`crates/consensus/service/src/actors/sequencer/pool.rs`](../../crates/consensus/service/src/actors/sequencer/pool.rs) |
| `GossipPayloadV3` | Publish and accept blob-capable gossip payloads. | [`crates/consensus/gossip/src/handler.rs`](../../crates/consensus/gossip/src/handler.rs) |
<!-- FEATURES:Ecotone:END -->

### Fjord

Fjord adds FastLZ-estimated L1 fees, Brotli channels, larger channel limits, fixed
sequencer drift, and RIP-7212 signature verification. Compression acceptance belongs
to derivation as well as the batch encoder. Upgrade deposits change onchain contracts.

<!-- FEATURES:Fjord:START -->
| Feature | Behavior | Production code |
| --- | --- | --- |
| `FastLzL1Fees` | Estimate compressed transaction size with `FastLZ` for L1 data fees. | [`crates/common/evm/src/l1block.rs`](../../crates/common/evm/src/l1block.rs), [`crates/common/l1-fees/src/params.rs`](../../crates/common/l1-fees/src/params.rs) |
| `Rip7212Precompile` | Verify secp256r1 signatures using the original RIP-7212 gas price. | [`crates/common/evm2/src/precompiles.rs`](../../crates/common/evm2/src/precompiles.rs), [`crates/common/precompiles/src/provider.rs`](../../crates/common/precompiles/src/provider.rs) |
| `FixedSequencerDrift` | Use the fixed 1800-second sequencer drift bound. | [`crates/common/genesis/src/rollup.rs`](../../crates/common/genesis/src/rollup.rs) |
| `LargeChannels` | Accept the larger decompressed channel byte limit. | [`crates/common/genesis/src/rollup.rs`](../../crates/common/genesis/src/rollup.rs), [`crates/consensus/derive/src/stages/channel/channel_assembler.rs`](../../crates/consensus/derive/src/stages/channel/channel_assembler.rs), [`crates/consensus/derive/src/stages/channel/channel_reader.rs`](../../crates/consensus/derive/src/stages/channel/channel_reader.rs) |
| `BrotliCompression` | Accept and produce Brotli-compressed batch channels. | [`crates/batcher/encoder/src/config.rs`](../../crates/batcher/encoder/src/config.rs), [`crates/consensus/derive/src/stages/channel/channel_reader.rs`](../../crates/consensus/derive/src/stages/channel/channel_reader.rs) |
| `ChannelBankSizeExpansion` | Expand the buffered channel bank's maximum size. | [`crates/consensus/derive/src/stages/channel/channel_bank.rs`](../../crates/consensus/derive/src/stages/channel/channel_bank.rs) |
| `FjordUpgradeTransactions` | Inject the Fjord contract-upgrade deposit bundle at the transition. | [`crates/consensus/derive/src/attributes/stateful.rs`](../../crates/consensus/derive/src/attributes/stateful.rs), [`crates/consensus/service/src/actors/sequencer/pool.rs`](../../crates/consensus/service/src/actors/sequencer/pool.rs) |
<!-- FEATURES:Fjord:END -->

### Granite

Granite limits pairing-precompile input and changes channel resource limits/timeouts.
The derivation queue and EVM precompile checks protect different resources and have
separate features even though they share an activation upgrade.

<!-- FEATURES:Granite:START -->
| Feature | Behavior | Production code |
| --- | --- | --- |
| `Bn254PairingInputLimit` | Bound bn254 pairing input size before executing the precompile. | [`crates/common/evm2/src/precompiles.rs`](../../crates/common/evm2/src/precompiles.rs), [`crates/common/precompiles/src/provider.rs`](../../crates/common/precompiles/src/provider.rs) |
| `ShortChannelTimeout` | Use the shortened channel timeout. | [`crates/common/genesis/src/rollup.rs`](../../crates/common/genesis/src/rollup.rs) |
| `GraniteTransitionNoTxPool` | Exclude transaction-pool transactions from the Granite transition block. | [`crates/consensus/service/src/actors/sequencer/pool.rs`](../../crates/consensus/service/src/actors/sequencer/pool.rs) |
<!-- FEATURES:Granite:END -->

### Holocene

Holocene makes derivation ordering stricter and carries dynamic EIP-1559 parameters
in header extra data. Builders, engine validation, and proof reconstruction must
interpret the same fee parameters, including at the transition.
`StrictDerivationOrdering` covers the batch, channel, and frame ordering model
and the pipeline reset that transitions into it; fee parameters remain independent.

<!-- FEATURES:Holocene:START -->
| Feature | Behavior | Production code |
| --- | --- | --- |
| `DynamicBaseFeeParameters` | Read dynamic EIP-1559 parameters from the block header extra data. | [`crates/builder/core/src/flashblocks/context.rs`](../../crates/builder/core/src/flashblocks/context.rs), [`crates/builder/core/src/flashblocks/payload.rs`](../../crates/builder/core/src/flashblocks/payload.rs), [`crates/common/genesis/src/system/config.rs`](../../crates/common/genesis/src/system/config.rs), [`crates/consensus/engine/src/attributes.rs`](../../crates/consensus/engine/src/attributes.rs), [`crates/consensus/protocol/src/utils.rs`](../../crates/consensus/protocol/src/utils.rs), [`crates/execution/chainspec/src/spec.rs`](../../crates/execution/chainspec/src/spec.rs), [`crates/execution/consensus/src/validation/holocene.rs`](../../crates/execution/consensus/src/validation/holocene.rs), [`crates/execution/node/src/engine.rs`](../../crates/execution/node/src/engine.rs), [`crates/execution/payload/src/payload.rs`](../../crates/execution/payload/src/payload.rs), [`crates/proof/executor/src/builder/assemble.rs`](../../crates/proof/executor/src/builder/assemble.rs), [`crates/proof/executor/src/builder/env.rs`](../../crates/proof/executor/src/builder/env.rs), [`crates/proof/host/src/handler.rs`](../../crates/proof/host/src/handler.rs) |
| `StrictDerivationOrdering` | Apply strict batch ordering and revised invalid-batch handling, assemble one channel at a time, drop out-of-order frames, and reset the pipeline when crossing into these rules. | [`crates/consensus/derive/src/stages/batch/batch_provider.rs`](../../crates/consensus/derive/src/stages/batch/batch_provider.rs), [`crates/consensus/derive/src/stages/batch/batch_queue.rs`](../../crates/consensus/derive/src/stages/batch/batch_queue.rs), [`crates/consensus/derive/src/stages/batch/batch_stream.rs`](../../crates/consensus/derive/src/stages/batch/batch_stream.rs), [`crates/consensus/derive/src/stages/channel/channel_provider.rs`](../../crates/consensus/derive/src/stages/channel/channel_provider.rs), [`crates/consensus/derive/src/stages/frame_queue.rs`](../../crates/consensus/derive/src/stages/frame_queue.rs), [`crates/consensus/derive/src/stages/traversal/polling.rs`](../../crates/consensus/derive/src/stages/traversal/polling.rs), [`crates/consensus/protocol/src/batch/single.rs`](../../crates/consensus/protocol/src/batch/single.rs), [`crates/consensus/protocol/src/batch/span.rs`](../../crates/consensus/protocol/src/batch/span.rs), [`crates/proof/zk/witness/src/fetcher.rs`](../../crates/proof/zk/witness/src/fetcher.rs) |
| `DepositsOnlyOnInvalidPayload` | Retry invalid execution payloads as deposits-only payloads. | [`crates/consensus/engine/src/task_queue/tasks/seal/task.rs`](../../crates/consensus/engine/src/task_queue/tasks/seal/task.rs), [`crates/proof/driver/src/core.rs`](../../crates/proof/driver/src/core.rs), [`crates/proof/zk/utils/src/client.rs`](../../crates/proof/zk/utils/src/client.rs) |
| `HoloceneTransitionNoTxPool` | Exclude transaction-pool transactions from the Holocene transition block. | [`crates/consensus/service/src/actors/sequencer/pool.rs`](../../crates/consensus/service/src/actors/sequencer/pool.rs) |
<!-- FEATURES:Holocene:END -->

### PectraBlobSchedule

This optional correction selects the L1 blob fee schedule used to construct L1
attributes for affected networks. It is independent of the execution fork ladder;
do not infer it from the highest active Base EVM spec or enable it on every network.

<!-- FEATURES:PectraBlobSchedule:START -->
| Feature | Behavior | Production code |
| --- | --- | --- |
| `BlobScheduleCorrection` | Apply the optional L1 Pectra blob schedule correction. | [`crates/common/consensus/src/info/variant.rs`](../../crates/common/consensus/src/info/variant.rs) |
<!-- FEATURES:PectraBlobSchedule:END -->

### Isthmus

Isthmus introduces operator fees, the L2-to-L1 message-passer storage root in the
header, extended L1 attributes, and upgrade deposits alongside Ethereum Prague.
Execution requests and Engine API versioning must stay consistent across producer,
validator, and proof paths. BLS precompiles have Base-specific input bounds.
`OperatorFee` also selects the attributes supplying its parameters, but the new
attributes format still starts after the transition block.

<!-- FEATURES:Isthmus:START -->
| Feature | Behavior | Production code |
| --- | --- | --- |
| `OperatorFee` | Charge the operator fee in addition to execution and L1 data fees, and include its fields in the L1 attributes deposit after the transition block. | [`crates/common/consensus/src/info/variant.rs`](../../crates/common/consensus/src/info/variant.rs), [`crates/common/evm/src/handler.rs`](../../crates/common/evm/src/handler.rs), [`crates/common/evm/src/l1block.rs`](../../crates/common/evm/src/l1block.rs), [`crates/common/evm2/src/handler.rs`](../../crates/common/evm2/src/handler.rs), [`crates/consensus/protocol/src/utils.rs`](../../crates/consensus/protocol/src/utils.rs) |
| `BlsInputLimits` | Bound BLS12-381 multi-scalar multiplication and pairing input sizes. | [`crates/common/evm2/src/precompiles.rs`](../../crates/common/evm2/src/precompiles.rs), [`crates/common/precompiles/src/provider.rs`](../../crates/common/precompiles/src/provider.rs) |
| `Eip7702` | Permit EIP-7702 set-code transactions in batches. | [`crates/consensus/protocol/src/batch/single.rs`](../../crates/consensus/protocol/src/batch/single.rs), [`crates/consensus/protocol/src/batch/span.rs`](../../crates/consensus/protocol/src/batch/span.rs) |
| `HistoryStorage` | Apply the historical-block-hash storage system call. | [`crates/execution/rpc/src/config.rs`](../../crates/execution/rpc/src/config.rs), [`crates/proof/proof/src/l2/chain_provider.rs`](../../crates/proof/proof/src/l2/chain_provider.rs) |
| `L2ToL1MessageRoot` | Put the L2-to-L1 message-passer storage root in the header withdrawals root. | [`crates/builder/core/src/flashblocks/payload.rs`](../../crates/builder/core/src/flashblocks/payload.rs), [`crates/consensus/engine/src/attributes.rs`](../../crates/consensus/engine/src/attributes.rs), [`crates/consensus/engine/src/query.rs`](../../crates/consensus/engine/src/query.rs), [`crates/execution/chainspec/src/spec.rs`](../../crates/execution/chainspec/src/spec.rs), [`crates/execution/consensus/src/lib.rs`](../../crates/execution/consensus/src/lib.rs), [`crates/execution/consensus/src/validation/mod.rs`](../../crates/execution/consensus/src/validation/mod.rs), [`crates/execution/evm/src/build.rs`](../../crates/execution/evm/src/build.rs), [`crates/execution/node/src/engine.rs`](../../crates/execution/node/src/engine.rs), [`crates/execution/payload/src/builder.rs`](../../crates/execution/payload/src/builder.rs), [`crates/proof/executor/src/builder/assemble.rs`](../../crates/proof/executor/src/builder/assemble.rs) |
| `EmptyRequestsHash` | Set the header requests hash to the empty execution-requests hash. | [`crates/builder/core/src/flashblocks/payload.rs`](../../crates/builder/core/src/flashblocks/payload.rs), [`crates/consensus/gossip/src/block_validity.rs`](../../crates/consensus/gossip/src/block_validity.rs), [`crates/execution/consensus/src/lib.rs`](../../crates/execution/consensus/src/lib.rs), [`crates/execution/evm/src/build.rs`](../../crates/execution/evm/src/build.rs), [`crates/proof/executor/src/builder/assemble.rs`](../../crates/proof/executor/src/builder/assemble.rs) |
| `EngineApiV4` | Use execution-request-capable V4 Engine API payload methods. | [`crates/consensus/engine/src/versions.rs`](../../crates/consensus/engine/src/versions.rs) |
| `IsthmusUpgradeTransactions` | Inject the Isthmus contract-upgrade deposit bundle at the transition. | [`crates/consensus/derive/src/attributes/stateful.rs`](../../crates/consensus/derive/src/attributes/stateful.rs), [`crates/consensus/service/src/actors/sequencer/pool.rs`](../../crates/consensus/service/src/actors/sequencer/pool.rs) |
| `GossipPayloadV4` | Publish and accept execution-request-capable gossip payloads. | [`crates/consensus/gossip/src/handler.rs`](../../crates/consensus/gossip/src/handler.rs) |
| `StrictProofSourceExhaustion` | Reject proof derivation that exhausts its source before the claimed block. | [`crates/proof/zk/utils/src/client.rs`](../../crates/proof/zk/utils/src/client.rs) |
<!-- FEATURES:Isthmus:END -->

### Jovian

Jovian adds DA-footprint accounting, a minimum base fee, revised operator-fee scaling,
extended L1 attributes, upgrade deposits, and tighter variable-input precompile
bounds. DA budgeting affects transaction selection as well as block validation.
`DaFootprint` also selects the attributes supplying its scalar after the transition
block. DA-aware base fees and minimum base fees remain separately named rules.

<!-- FEATURES:Jovian:START -->
| Feature | Behavior | Production code |
| --- | --- | --- |
| `ScaledOperatorFees` | Scale the operator fee scalar component by 100 instead of dividing by one million. | [`crates/common/l1-fees/src/params.rs`](../../crates/common/l1-fees/src/params.rs) |
| `DaFootprint` | Meter transaction DA footprint against the block budget, and include the DA footprint scalar in L1 attributes after the transition block. | [`crates/builder/core/src/flashblocks/context.rs`](../../crates/builder/core/src/flashblocks/context.rs), [`crates/common/consensus/src/info/variant.rs`](../../crates/common/consensus/src/info/variant.rs), [`crates/common/evm/src/executor/block_executor.rs`](../../crates/common/evm/src/executor/block_executor.rs), [`crates/common/evm/src/l1block.rs`](../../crates/common/evm/src/l1block.rs), [`crates/common/evm2/src/executor.rs`](../../crates/common/evm2/src/executor.rs), [`crates/consensus/gossip/src/block_validity.rs`](../../crates/consensus/gossip/src/block_validity.rs), [`crates/execution/consensus/src/lib.rs`](../../crates/execution/consensus/src/lib.rs), [`crates/execution/consensus/src/validation/mod.rs`](../../crates/execution/consensus/src/validation/mod.rs), [`crates/execution/evm/src/build.rs`](../../crates/execution/evm/src/build.rs), [`crates/execution/flashblocks/src/state_builder.rs`](../../crates/execution/flashblocks/src/state_builder.rs), [`crates/execution/payload/src/builder.rs`](../../crates/execution/payload/src/builder.rs), [`crates/execution/rpc/src/eth/receipt.rs`](../../crates/execution/rpc/src/eth/receipt.rs), [`crates/execution/txpool/src/validator.rs`](../../crates/execution/txpool/src/validator.rs), [`crates/proof/executor/src/builder/assemble.rs`](../../crates/proof/executor/src/builder/assemble.rs) |
| `DaAwareBaseFee` | Calculate the base fee from the larger of execution gas and DA footprint, with a floor. | [`crates/execution/chainspec/src/spec.rs`](../../crates/execution/chainspec/src/spec.rs), [`crates/proof/executor/src/builder/env.rs`](../../crates/proof/executor/src/builder/env.rs) |
| `TightBn254PairingInputLimit` | Tighten the bn254 pairing input limit. | [`crates/common/evm2/src/precompiles.rs`](../../crates/common/evm2/src/precompiles.rs), [`crates/common/precompiles/src/provider.rs`](../../crates/common/precompiles/src/provider.rs) |
| `TightBlsInputLimits` | Tighten BLS12-381 multi-scalar multiplication and pairing input limits. | [`crates/common/evm2/src/precompiles.rs`](../../crates/common/evm2/src/precompiles.rs), [`crates/common/precompiles/src/provider.rs`](../../crates/common/precompiles/src/provider.rs) |
| `MinBaseFee` | Enforce the minimum base fee carried in block extra data. | [`crates/builder/core/src/flashblocks/context.rs`](../../crates/builder/core/src/flashblocks/context.rs), [`crates/builder/core/src/flashblocks/payload.rs`](../../crates/builder/core/src/flashblocks/payload.rs), [`crates/consensus/derive/src/attributes/stateful.rs`](../../crates/consensus/derive/src/attributes/stateful.rs), [`crates/consensus/engine/src/attributes.rs`](../../crates/consensus/engine/src/attributes.rs), [`crates/consensus/protocol/src/utils.rs`](../../crates/consensus/protocol/src/utils.rs), [`crates/consensus/service/src/standalone.rs`](../../crates/consensus/service/src/standalone.rs), [`crates/execution/consensus/src/validation/holocene.rs`](../../crates/execution/consensus/src/validation/holocene.rs), [`crates/execution/node/src/engine.rs`](../../crates/execution/node/src/engine.rs), [`crates/execution/payload/src/payload.rs`](../../crates/execution/payload/src/payload.rs), [`crates/proof/executor/src/builder/assemble.rs`](../../crates/proof/executor/src/builder/assemble.rs), [`crates/proof/executor/src/builder/env.rs`](../../crates/proof/executor/src/builder/env.rs), [`crates/proof/host/src/handler.rs`](../../crates/proof/host/src/handler.rs) |
| `JovianUpgradeTransactions` | Inject the Jovian contract-upgrade deposit bundle at the transition. | [`crates/consensus/derive/src/attributes/stateful.rs`](../../crates/consensus/derive/src/attributes/stateful.rs), [`crates/consensus/protocol/src/batch/single.rs`](../../crates/consensus/protocol/src/batch/single.rs), [`crates/consensus/service/src/actors/sequencer/pool.rs`](../../crates/consensus/service/src/actors/sequencer/pool.rs) |
<!-- FEATURES:Jovian:END -->

### Azul

Azul pairs Base execution with Ethereum Osaka and its transaction gas cap and opcode
changes. Base precompiles adopt the updated MODEXP/P256 rules while preserving
Base-specific bounds. Ethereum-defined rules continue to use Ethereum spec gates.

<!-- FEATURES:Azul:START -->
| Feature | Behavior | Production code |
| --- | --- | --- |
| `RepricedP256Precompile` | Use the Osaka price for the secp256r1 signature verifier. | [`crates/common/evm2/src/precompiles.rs`](../../crates/common/evm2/src/precompiles.rs), [`crates/common/precompiles/src/provider.rs`](../../crates/common/precompiles/src/provider.rs) |
| `TransactionGasLimitCap` | Configure the Osaka per-transaction gas limit cap in Base EVM environments. | [`crates/execution/evm/src/env.rs`](../../crates/execution/evm/src/env.rs), [`crates/execution/txpool/src/validator.rs`](../../crates/execution/txpool/src/validator.rs) |
| `CompactFlashblockMetadata` | Omit receipt and balance metadata from published flashblocks. | [`crates/builder/core/src/flashblocks/payload.rs`](../../crates/builder/core/src/flashblocks/payload.rs) |
| `EngineApiV5` | Request V5 payload envelopes from the execution engine. | [`crates/consensus/engine/src/versions.rs`](../../crates/consensus/engine/src/versions.rs) |
<!-- FEATURES:Azul:END -->

### Beryl

Beryl adds native token, factory, policy, and activation-registry precompiles.
These dynamic precompiles are installed separately from the static Ethereum table;
their contract/storage behavior is part of consensus, not an RPC-only facility.

<!-- FEATURES:Beryl:START -->
| Feature | Behavior | Production code |
| --- | --- | --- |
| `TokenPrecompiles` | Resolve dynamic B20 token addresses to their native precompile implementations. | [`crates/common/precompiles/src/provider.rs`](../../crates/common/precompiles/src/provider.rs), [`crates/execution/metering/src/meter.rs`](../../crates/execution/metering/src/meter.rs) |
| `ActivationRegistry` | Install the native activation registry for token and policy precompiles. | [`crates/common/precompiles/src/provider.rs`](../../crates/common/precompiles/src/provider.rs), [`crates/execution/chainspec/src/spec.rs`](../../crates/execution/chainspec/src/spec.rs) |
| `AssetPrecompile` | Enable the first B20 asset contract implementation. | [`crates/common/precompiles/src/b20_asset/versions.rs`](../../crates/common/precompiles/src/b20_asset/versions.rs) |
| `StablecoinPrecompile` | Enable the first B20 stablecoin contract implementation. | [`crates/common/precompiles/src/b20_stablecoin/versions.rs`](../../crates/common/precompiles/src/b20_stablecoin/versions.rs) |
| `PolicyPrecompile` | Enable the first B20 policy contract implementation. | [`crates/common/precompiles/src/policy/versions.rs`](../../crates/common/precompiles/src/policy/versions.rs), [`crates/common/precompiles/src/provider.rs`](../../crates/common/precompiles/src/provider.rs) |
| `TokenFactoryPrecompile` | Enable the B20 factory contract implementation. | [`crates/common/precompiles/src/b20_factory/versions.rs`](../../crates/common/precompiles/src/b20_factory/versions.rs), [`crates/common/precompiles/src/provider.rs`](../../crates/common/precompiles/src/provider.rs) |
<!-- FEATURES:Beryl:END -->

### Cobalt

Cobalt revises native token/policy implementations and storage accounting, tightens
activation-registry authorization/value handling, and adds transaction context and
nonce-manager precompiles. Existing onchain state must keep its historical behavior
when replayed under earlier specs.
`DynamicStorageTailCleanup` and `CompactAbiDecodeErrors` remain distinct features,
but the two-profile storage API requires them to activate together. A compile-time
assertion in the [storage adapter](../../crates/common/precompiles/src/spec.rs)
enforces that requirement; separating their activation requires extending the profiles.

<!-- FEATURES:Cobalt:START -->
| Feature | Behavior | Production code |
| --- | --- | --- |
| `AssetPrecompileV2` | Use the second B20 asset contract implementation. | [`crates/common/precompiles/src/b20_asset/versions.rs`](../../crates/common/precompiles/src/b20_asset/versions.rs) |
| `StablecoinPrecompileV2` | Use the second B20 stablecoin contract implementation. | [`crates/common/precompiles/src/b20_stablecoin/versions.rs`](../../crates/common/precompiles/src/b20_stablecoin/versions.rs) |
| `PolicyPrecompileV2` | Use the second B20 policy contract implementation. | [`crates/common/precompiles/src/policy/versions.rs`](../../crates/common/precompiles/src/policy/versions.rs) |
| `DynamicStorageTailCleanup` | Clear retired native-precompile dynamic storage slots and charge the preparatory reads. | [`crates/common/precompiles/src/spec.rs`](../../crates/common/precompiles/src/spec.rs) |
| `CompactAbiDecodeErrors` | Return only the selector, without decoder diagnostics, for native-precompile ABI errors. | [`crates/common/precompiles/src/spec.rs`](../../crates/common/precompiles/src/spec.rs) |
| `ActivationRegistryRejectsValue` | Reject nonzero call value in activation registry calls. | [`crates/common/precompiles/src/activation/dispatch.rs`](../../crates/common/precompiles/src/activation/dispatch.rs) |
| `StatefulActivationAdmin` | Read the activation administrator from storage, falling back to chain configuration. | [`crates/common/precompiles/src/provider.rs`](../../crates/common/precompiles/src/provider.rs) |
| `TransactionContextPrecompile` | Install the transaction-context precompile backed by transient storage. | [`crates/common/precompiles/src/provider.rs`](../../crates/common/precompiles/src/provider.rs) |
| `NonceManagerPrecompile` | Install the native two-dimensional nonce manager precompile. | [`crates/common/precompiles/src/provider.rs`](../../crates/common/precompiles/src/provider.rs) |
<!-- FEATURES:Cobalt:END -->

### Denim

Denim introduces deterministic 200ms blocks, millisecond header/payload timestamps,
the Base time predeploy, expanded sequencer drift, and revised native contract
implementations. This spans sequencing, batching, gossip, Engine/RPC formats,
execution, and proofs. Activation rounds to a legacy timestamp slot; subsequent
blocks advance by 200ms. Never infer the parent by subtracting the old block time.
`SubsecondBlocks` includes the timestamp schedule, transition gas-parameter scaling,
and retaining the parent's L1 origin within a whole second. One-shot payload
building and singular-only batching remain separate features.

<!-- FEATURES:Denim:START -->
| Feature | Behavior | Production code |
| --- | --- | --- |
| `AssetPrecompileV3` | Use the third B20 asset contract implementation. | [`crates/common/precompiles/src/b20_asset/versions.rs`](../../crates/common/precompiles/src/b20_asset/versions.rs) |
| `StablecoinPrecompileV3` | Use the third B20 stablecoin contract implementation. | [`crates/common/precompiles/src/b20_stablecoin/versions.rs`](../../crates/common/precompiles/src/b20_stablecoin/versions.rs) |
| `PolicyPrecompileV3` | Use the third B20 policy contract implementation. | [`crates/common/precompiles/src/policy/versions.rs`](../../crates/common/precompiles/src/policy/versions.rs) |
| `SubsecondBlocks` | Use deterministic 200ms block timestamps and the Base time predeploy, scale legacy whole-second gas parameters, and retain the parent's L1 origin within the same second. | [`crates/common/evm/src/base_time.rs`](../../crates/common/evm/src/base_time.rs), [`crates/common/evm2/src/base_time.rs`](../../crates/common/evm2/src/base_time.rs), [`crates/common/genesis/src/rollup.rs`](../../crates/common/genesis/src/rollup.rs), [`crates/consensus/derive/src/attributes/stateful.rs`](../../crates/consensus/derive/src/attributes/stateful.rs), [`crates/consensus/derive/src/stages/batch/batch_queue.rs`](../../crates/consensus/derive/src/stages/batch/batch_queue.rs), [`crates/consensus/derive/src/stages/batch/batch_validator.rs`](../../crates/consensus/derive/src/stages/batch/batch_validator.rs), [`crates/consensus/protocol/src/batch/single.rs`](../../crates/consensus/protocol/src/batch/single.rs), [`crates/consensus/service/src/actors/sequencer/actor.rs`](../../crates/consensus/service/src/actors/sequencer/actor.rs), [`crates/consensus/service/src/actors/sequencer/l1_origin/selector.rs`](../../crates/consensus/service/src/actors/sequencer/l1_origin/selector.rs), [`crates/consensus/service/src/standalone.rs`](../../crates/consensus/service/src/standalone.rs), [`crates/consensus/service/src/standalone_dev.rs`](../../crates/consensus/service/src/standalone_dev.rs), [`crates/execution/consensus/src/lib.rs`](../../crates/execution/consensus/src/lib.rs), [`crates/execution/evm/src/forecast.rs`](../../crates/execution/evm/src/forecast.rs), [`crates/execution/metering/src/meter.rs`](../../crates/execution/metering/src/meter.rs), [`crates/execution/node/src/engine.rs`](../../crates/execution/node/src/engine.rs), [`crates/execution/rpc/src/eth/call.rs`](../../crates/execution/rpc/src/eth/call.rs), [`crates/execution/txpool-rpc/src/rpc.rs`](../../crates/execution/txpool-rpc/src/rpc.rs), [`crates/execution/txpool/src/validator.rs`](../../crates/execution/txpool/src/validator.rs), [`etc/systems/src/l2/snapshot_stack.rs`](../../etc/systems/src/l2/snapshot_stack.rs) |
| `ExtendedSequencerDrift` | Use the extended six-hour sequencer drift bound. | [`crates/common/genesis/src/rollup.rs`](../../crates/common/genesis/src/rollup.rs) |
| `OneShotPayloadBuilding` | Build and finalize payloads once instead of replacing them for higher fees. | [`crates/builder/multiplex/src/router.rs`](../../crates/builder/multiplex/src/router.rs), [`crates/execution/payload/src/builder.rs`](../../crates/execution/payload/src/builder.rs) |
| `SingularBatchesOnly` | Reject span batches and encode only singular batches for subsecond blocks. | [`crates/consensus/derive/src/stages/batch/batch_queue.rs`](../../crates/consensus/derive/src/stages/batch/batch_queue.rs), [`crates/consensus/derive/src/stages/batch/batch_stream.rs`](../../crates/consensus/derive/src/stages/batch/batch_stream.rs), [`crates/consensus/protocol/src/batch/span.rs`](../../crates/consensus/protocol/src/batch/span.rs) |
<!-- FEATURES:Denim:END -->

### Everest

Everest adds enshrined EIP-8130 account-abstraction transactions and native nonce
manager deployment. Transaction validation, pool admission, execution, and related
RPC methods must agree on feature activation; RPC visibility alone is not enforcement.

<!-- FEATURES:Everest:START -->
| Feature | Behavior | Production code |
| --- | --- | --- |
| `Eip8130` | Execute enshrined EIP-8130 account-abstraction transactions. | [`crates/common/evm/src/eip8130.rs`](../../crates/common/evm/src/eip8130.rs), [`crates/consensus/protocol/src/batch/single.rs`](../../crates/consensus/protocol/src/batch/single.rs), [`crates/consensus/protocol/src/batch/span.rs`](../../crates/consensus/protocol/src/batch/span.rs), [`crates/execution/eip8130-rpc/src/everest_gate.rs`](../../crates/execution/eip8130-rpc/src/everest_gate.rs), [`crates/execution/rpc/src/eth/transaction.rs`](../../crates/execution/rpc/src/eth/transaction.rs), [`crates/execution/txpool-rpc/src/rpc.rs`](../../crates/execution/txpool-rpc/src/rpc.rs), [`crates/execution/txpool/src/validator.rs`](../../crates/execution/txpool/src/validator.rs) |
| `Eip8430` | Expose EIP-8430 payer resolution RPC methods. | [`crates/execution/payer-rpc/src/rpc.rs`](../../crates/execution/payer-rpc/src/rpc.rs) |
| `NativeNonceManager` | Plant a nonempty code stub on the native nonce manager to preserve its persistent state. | [`crates/common/evm/src/everest.rs`](../../crates/common/evm/src/everest.rs), [`crates/common/evm2/src/everest.rs`](../../crates/common/evm2/src/everest.rs) |
<!-- FEATURES:Everest:END -->

### Zenith

Zenith contains experimental keystore support. It is a genesis-only testing gate,
not an L1-contract-backed network activation. Runtime signals cannot schedule it.
Do not treat enum ordering or a future timestamp in an announcement as evidence of
production activation.

<!-- FEATURES:Zenith:START -->
| Feature | Behavior | Production code |
| --- | --- | --- |
| `Keystore` | Enable experimental EIP-8130 keystore support on genesis-configured chains only. | [`crates/common/evm/src/eip8130.rs`](../../crates/common/evm/src/eip8130.rs), [`crates/execution/payer-rpc/src/rpc.rs`](../../crates/execution/payer-rpc/src/rpc.rs), [`crates/execution/txpool/src/validator.rs`](../../crates/execution/txpool/src/validator.rs) |
<!-- FEATURES:Zenith:END -->
