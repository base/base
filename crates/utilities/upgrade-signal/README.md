# `base-upgrade-signal`

Shared utilities for reading network upgrade activation signals from L1.

The crate reads the L1 `ProtocolVersions` contract and decodes the announced activation timestamps
and the global minimum node protocol version. Metrics are recorded for both startup reads and live
signal changes.

Three graduated rollout modes are supported:

- **metrics-only** — observe signals and record metrics without applying them
- **startup-apply** — pin activation timestamps into the chain spec at node startup
- **runtime-admin** — write live overrides into `RuntimeUpgradeRegistry` so fork checks reflect
  contract-sourced signals without a node restart

## Protocol Versions

The contract exposes one global `minimumProtocolVersion()` as a packed-semver `uint256`
(`major << 96 | minor << 64 | patch << 32`), so this crate reads it as `U256` and attaches it to
every signal in a schedule.

The node advertises its supported level with
[`UpgradeSignalDefaults::node_protocol_version()`](src/config/mod.rs), which packs the Cargo
package semver synced from the `GitHub` release tag on release branches. Mainline `0.0.0` builds use
the latest protocol version implemented on main. A signal is supported when:

- the activation timestamp is positive
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
