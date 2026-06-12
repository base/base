# `base-upgrade-signal`

Shared utilities for reading network upgrade activation signals from L1.

The crate reads an L1 contract interface and decodes the announced activation timestamp and minimum
node protocol version for each configured hardfork ID. Metrics are recorded for both startup reads
and live signal changes.

Three graduated rollout modes are supported:

- **metrics-only** — observe signals and record metrics without applying them
- **startup-apply** — pin activation timestamps into the chain spec at node startup
- **runtime-admin** — write live overrides into `RuntimeHardForkRegistry` so fork checks reflect
  contract-sourced signals without a node restart
