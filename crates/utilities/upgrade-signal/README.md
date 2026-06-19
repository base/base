# `base-upgrade-signal`

Shared utilities for reading network upgrade activation signals from L1.

The crate reads an L1 contract interface and decodes the announced activation timestamp and minimum
node protocol version for each configured upgrade ID. Metrics are recorded for both startup reads
and live signal changes.

Three graduated rollout modes are supported:

- **metrics-only** — observe signals and record metrics without applying them
- **startup-apply** — pin activation timestamps into the chain spec at node startup
- **runtime-admin** — write live overrides into `RuntimeUpgradeRegistry` so fork checks reflect
  contract-sourced signals without a node restart

## Protocol Versions

The contract exposes protocol versions as `uint256`, so this crate reads them as `U256`. The value
is not semver onchain.

The node advertises its supported level with
[`UpgradeSignalDefaults::NODE_PROTOCOL_VERSION`](src/config/mod.rs). A signal is supported when:

- the activation timestamp is positive
- the signal also provides a non-zero protocol version
- the signaled minimum protocol version is less than or equal to the node's supported protocol
  version

## Upgrade Timestamps

Each upgrade ID also has an activation timestamp in the contract, exposed as `uint256` and reduced
to `u64` in the node after decode.

The timestamp semantics are:

- `0` means "no activation is currently scheduled"
- any positive value is an L2 activation timestamp for that upgrade
- values larger than `u64::MAX` are rejected as malformed contract data

The crate validates timestamps and protocol versions together. A positive timestamp without a
minimum protocol version is rejected.
