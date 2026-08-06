# `base-upgrade-signal`

Shared utilities for reading network upgrade activation signals from L1.

The crate reads the L1 `ProtocolVersions` contract and decodes the announced activation timestamps
and the global minimum node protocol version. Metrics are recorded for both startup reads and live
signal changes.

Three graduated rollout modes are supported:

- **metrics-only** — observe signals and record metrics without applying them
- **startup-apply** — pin activation timestamps into the chain spec at node startup
- **runtime-admin** — apply the startup schedule, automatically re-apply changed live L1 schedules
  into `RuntimeUpgradeRegistry`, and expose a manual admin refresh RPC

## Configuration

The shared CLI flags are:

| Flag | Env var | Default | Description |
| ---- | ------- | ------- | ----------- |
| `--upgrade-signal.contract <ADDRESS>` | `BASE_NODE_UPGRADE_SIGNAL_CONTRACT` | unset | Enables L1 schedule reads from the `ProtocolVersions` contract or proxy. The full contract-backed upgrade schedule is always read; application depends on the selected mode. |
| `--upgrade-signal.mode <metrics-only\|startup-apply\|runtime-admin>` | `BASE_NODE_UPGRADE_SIGNAL_MODE` | `metrics-only` | Selects whether reads are observation-only, startup-applied, or runtime-applied. |
| `--upgrade-signal.l1-block-tag <finalized\|safe\|latest>` | `BASE_NODE_UPGRADE_SIGNAL_L1_BLOCK_TAG` | `finalized` | Selects the L1 block tag used for contract calls. Also selects the interval between live contract reads: `finalized` 15m, `safe` 6m24s, `latest` 12s. |

Execution-side readers also need `--upgrade-signal.l1-rpc` or
`BASE_NODE_UPGRADE_SIGNAL_L1_RPC`. Integrated `base rpc` and `base sequencer` commands derive this
from their consensus `--l1-eth-rpc` by default so execution and consensus read the same L1 source
unless an explicit override is supplied.

## Runtime Behavior

All modes with a configured contract start a live observer. The observer polls L1 on an interval
selected by the configured block tag (see the flag table above), matching how often that tag
typically advances. It records the latest schedule metrics and update counters when a
contract-backed signal changes. Read failures are metrics-only failures: they are logged and
counted, but they do not clear the last observed schedule or stop the node.

`startup-apply` reads and validates the L1 schedule before the node starts serving.
Execution-side callers apply the schedule to the chain spec, consensus-side callers apply it to the
rollup config, and integrated `base` commands apply one startup read to both. Live polling remains
observation-only after startup.

`runtime-admin` includes the startup application path and also auto-applies live changes. On the
first live observation and on each later signal change, the schedule is validated again and applied
to `RuntimeUpgradeRegistry` so fork checks can see contract-sourced activation updates without a
restart. The same mode exposes `admin_refreshUpgradeSignal` for a manual refresh against the
execution or consensus admin RPC when the admin namespace is enabled.

Applying a positive timestamp installs or replaces a runtime activation override. Applying `0`
clears the activation by installing an explicit never-active override for that upgrade.
Protocol-version failures fail the
refresh without mutation. Entries that a specific activation sink does not support are reported as
ignored, and contract schedule entries for upgrades newer than the binary knows are logged and
ignored while mapping the contract schedule.

## Protocol Versions

The contract exposes one global `minimumProtocolVersion()` as a packed-semver `uint256`
(`major << 96 | minor << 64 | patch << 32`), so this crate reads it as `U256` and attaches it to
every signal in a schedule.

The node advertises its supported level with
[`UpgradeSignalDefaults::node_protocol_version()`](src/config/mod.rs), which packs the Cargo
package semver synced from the `GitHub` release tag on release branches. Development `0.0.0` builds
advertise `U256::MAX` so contract minimums do not reject untagged local builds. A positive
activation signal is supported when:

- the contract provides a non-zero minimum protocol version
- the signaled minimum protocol version is less than or equal to the node's supported protocol
  version

## Upgrade Timestamps

The contract's `getSchedule()` returns one `uint64` activation timestamp per registered upgrade,
ordered by ascending numeric upgrade id. Upgrade names are kept offchain: the node maps schedule
entries onto its known hardfork ladder by registration id, aligning id `0` with the oldest
contract-backed hardfork. This is a positional mapping by id, not a sort by timestamp, so the
timestamps need not be monotonic. Contract entries beyond the ladder belong to upgrades newer than
this binary knows and are logged and ignored, and hardforks without a contract entry produce no
signal.

The timestamp semantics are:

- `0` means "no activation is currently scheduled"
- any positive value is an L2 activation timestamp for that upgrade

The crate validates timestamps and protocol versions together. A positive timestamp without a
minimum protocol version is rejected.
