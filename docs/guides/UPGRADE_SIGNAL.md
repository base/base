# L1 Upgrade Signal

The L1 upgrade signal feature lets Base nodes read hardfork activation timestamps from an L1
contract instead of baking every activation timestamp into the node binary. It is designed for a
conservative rollout model:

1. The node reads a configured L1 contract at startup.
2. The startup result is applied to the local execution chain spec and consensus rollup config.
3. While the node is live, it keeps polling the same contract for metrics only.
4. Live polling never mutates the already-started node schedule.

This means operators can observe whether the L1 signal changes during a rollout, but a running node
does not silently change its fork schedule after startup.

## Contract Interface

The shared contract interface lives in
[`IUpgradeSignal`](../../crates/utilities/upgrade-signal/src/contract.rs). Nodes only require two
read methods:

```solidity
function getTimestamp(string hardforkId) external view returns (uint256);
function getProtocolVersion(string hardforkId) external view returns (uint256);
```

For each configured hardfork ID, the node reads:

- `activation_timestamp`: the L2 timestamp for the hardfork activation. A positive timestamp
  schedules the hardfork. A zero timestamp clears an existing schedule for supported hardforks.
- `protocol_version`: the minimum node protocol version required to apply the timestamp. Startup
  schedule application rejects positive activation timestamps with a missing version or a version
  above the binary's supported version, so older nodes do not activate partial upgrade logic.
- `l1_block_number`: the L1 block number used for the read, also recorded as a metric.

## Configuration

The shared CLI arguments are defined in
[`config.rs`](../../crates/utilities/upgrade-signal/src/config.rs).

| Argument | Environment Variable | Meaning |
| --- | --- | --- |
| `--upgrade-signal.contract` | `BASE_NODE_UPGRADE_SIGNAL_CONTRACT` | L1 contract or proxy address. Enables the feature when present. |
| `--upgrade-signal.hardfork-id` | `BASE_NODE_UPGRADE_SIGNAL_HARDFORK_ID` | Optional comma-delimited hardfork IDs to read. Defaults to all timestamp-based Base hardfork IDs. |
| `--upgrade-signal.l1-rpc` | `BASE_NODE_UPGRADE_SIGNAL_L1_RPC` | L1 execution RPC URL used by standalone execution nodes. |

Standalone execution nodes require both `--upgrade-signal.contract` and
`--upgrade-signal.l1-rpc`. Consensus nodes already have `--l1-eth-rpc`, so they reuse that endpoint.

The integrated `base rpc` command has a single L1 RPC source of truth. It derives the execution
upgrade-signal L1 RPC from consensus `--l1-eth-rpc`, then copies the shared upgrade-signal contract
arguments into the embedded consensus config. This keeps clap argument IDs unique and avoids a
second `--upgrade-signal.l1-rpc` knob in the integrated command. The integrated command wiring is in
[`bin/base/src/commands/rpc.rs`](../../bin/base/src/commands/rpc.rs).

## Read Semantics

`AlloyUpgradeSignalReader::read_schedule` intentionally pins every read in one schedule to the same
L1 block:

1. It asks the L1 provider for the latest block.
2. It stores both the block number and concrete block hash.
3. It reads every configured hardfork ID using that same block hash.

This avoids uncertainty from reading one hardfork at L1 block `N` and another at block `N + 1`.
Every `UpgradeSignal` still carries the block number so metrics and logs can show exactly which L1
block supplied the schedule.

## Startup Application

Startup application is the only path that mutates node configuration.

### Execution

The execution path starts in
[`StandardBaseRethNode::apply_initial_upgrade_signal`](../../crates/execution/cli/src/standard_node.rs).
When configured, it builds an `ExecutionUpgradeSignalConfig` and calls
[`ExecutionUpgradeSignal::apply_initial_signal_to_chain_spec`](../../crates/execution/cli/src/upgrade_signal.rs).

That function:

1. Reads the pinned schedule from L1.
2. Records startup schedule metrics and logs the timestamp, L1 block, and minimum protocol
   version for each signal.
3. Validates that every signal's minimum protocol version is supported by this binary.
4. Applies supported hardfork timestamps to `BaseChainSpec`.
5. Refreshes the genesis header after changing fork conditions.

Unsupported hardfork IDs are ignored with a debug log. A zero timestamp clears the supported
hardfork schedule. Positive timestamps go through the chain spec setters, so existing invariants
still run. For example, Beryl cannot be scheduled without the required activation admin address.

### Consensus

The consensus path starts in
[`ConsensusNodeArgs::build_rollup_node_with_overrides`](../../crates/consensus/cli/src/node.rs).
When configured, it calls `apply_initial_upgrade_signal` before constructing the `RollupNode`.

That function:

1. Reads the pinned schedule from L1 using consensus `--l1-eth-rpc`.
2. Records startup schedule metrics and logs the timestamp, L1 block, and minimum protocol
   version for each signal.
3. Validates that every signal's minimum protocol version is supported by this binary.
4. Applies supported hardfork timestamps to `RollupConfig`.

Just like execution, unsupported hardfork IDs are ignored and a zero timestamp clears the supported
hardfork schedule.

## Live Metrics Only

After startup, the feature keeps observing the contract so rollout dashboards can see whether the L1
signal changes while nodes are running. This live path is intentionally metrics-only. It does not
call execution chain-spec setters, consensus rollup-config setters, or any other schedule mutation
API.

### Execution Live Observer

The execution runtime observer is
[`ExecutionUpgradeSignalMetricsExtension`](../../crates/execution/cli/src/upgrade_signal.rs). It is
installed by
[`StandardBaseRethNode::install_upgrade_signal_metrics_extension`](../../crates/execution/cli/src/standard_node.rs)
when upgrade-signal config is present.

The extension starts after the execution node has started. Every
`DEFAULT_UPGRADE_SIGNAL_POLL_INTERVAL` seconds, it reads the pinned L1 schedule and updates metrics.

### Consensus Live Observer

The consensus runtime observer is
[`UpgradeSignalMetricsActor`](../../crates/consensus/service/src/actors/upgrade_signal.rs). The
builder carries `upgrade_signal_metrics_config` into `RollupNode`, and `RollupNode::start` spawns
the actor when the feature is configured.

The actor uses the same 12-second polling interval and the same metrics state model as execution.

## Live Update Detection

Live update detection is implemented by
[`UpgradeSignalMonitor`](../../crates/utilities/upgrade-signal/src/state.rs).

For each hardfork ID, the monitor stores the last live signal it read. Each new live read produces
one of three states:

- `Initialized`: first live read for that hardfork. This establishes the baseline and does not
  count as an update.
- `Unchanged`: the hardfork ID, activation timestamp, and protocol version match the previous live
  read.
- `Changed`: activation timestamp or protocol version changed while the node was live.

L1 block number changes alone do not count as signal updates. This lets the observer poll every 12
seconds without reporting an update every time L1 advances.

## Metrics

Metric definitions live in
[`metrics.rs`](../../crates/utilities/upgrade-signal/src/metrics.rs) under the
`base.upgrade_signal` scope.

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `base.upgrade_signal.activation_timestamp` | gauge | `hardfork` | Latest activation timestamp read for the hardfork. |
| `base.upgrade_signal.expected_protocol_version` | gauge | `hardfork` | Latest minimum node protocol version read for the hardfork. |
| `base.upgrade_signal.last_l1_read_block` | gauge | `hardfork` | L1 block number used for the latest successful read. |
| `base.upgrade_signal.l1_read_errors_total` | counter | `hardfork` | Failed attempts to read the L1 signal. |
| `base.upgrade_signal.signal_updates_total` | counter | `hardfork` | Live signal value changes after the initial live baseline. |
| `base.upgrade_signal.l2_timestamp_errors_total` | counter | `hardfork` | Reserved for L2 timestamp observation compatibility. The current live path does not read L2 timestamps. |
| `base.upgrade_signal.activation_observed` | gauge | `hardfork` | Reserved activation-observation surface. The current live path initializes it to `0`. |
| `base.upgrade_signal.activation_observed_total` | counter | `hardfork` | Reserved activation-observation surface. The current live path does not increment it. |

Startup reads record the schedule gauges. Live reads refresh those gauges and increment
`signal_updates_total` only when the contract-backed values change after the live baseline.

Read failures are treated differently by phase:

- Startup failures are fatal to the startup path and increment `l1_read_errors_total`.
- Live failures are logged, increment `l1_read_errors_total`, and the observer keeps polling.

## Devnet Flow

The devnet helper script
[`etc/scripts/devnet/setup-upgrade-signal.sh`](../../etc/scripts/devnet/setup-upgrade-signal.sh)
deploys or configures a mock upgrade signal contract and writes `.devnet/upgrade-signal.env`.

The compose override
[`etc/docker/docker-compose.upgrade-signal.yml`](../../etc/docker/docker-compose.upgrade-signal.yml)
injects the contract address into the execution, consensus, and integrated RPC services. Standalone
execution services also receive `BASE_NODE_UPGRADE_SIGNAL_L1_RPC`; consensus and integrated RPC
services use their existing L1 RPC configuration.

After restarting services, metrics can be checked with:

```bash
curl -s http://localhost:7300/metrics | grep upgrade_signal
curl -s http://localhost:8090/metrics | grep upgrade_signal
curl -s http://localhost:8300/metrics | grep upgrade_signal
```

## Invariants

Keep these invariants intact when changing this feature:

- All hardfork reads for one schedule must use the same concrete L1 block.
- Startup is the only path that mutates execution or consensus fork schedules.
- Startup schedule application must reject positive activation timestamps that are missing a
  minimum protocol version or require a version newer than the node software.
- Live polling is metrics-only and must not apply schedule changes to a running node.
- The first live poll establishes a baseline and must not count as a live update.
- L1 block movement alone must not count as a live signal update.
- Startup schedule application must continue to use existing chain spec and rollup config setters,
  so local validation rules are not bypassed.
