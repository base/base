# `base-common-chain-config`

Canonical Base chain configuration, upgrade schedules, genesis formats, and derived execution metadata.

`ChainConfig` owns network identity, the typed `ChainUpgrades` schedule, genesis parameters, fee settings, and configured bootnode strings. `BaseUpgrade` provides schedule queries directly. Runtime activation overrides and the activation administrator remain part of the shared configuration model.

`BaseChainSpec` derives genesis headers, hashes, fork IDs, and fee rules from that configuration. `GenesisInfo`, `RollupConfig`, and `ChainGenesis` support genesis JSON and rollup configuration at application boundaries. Execution bootnode parsing belongs to networking.

```rust
use base_common_chain_config::{BaseChainSpec, ChainConfig};

let config = ChainConfig::mainnet();
assert_eq!(config.chain_id, 8453);
assert_eq!(BaseChainSpec::mainnet().chain_id(), config.chain_id);
```

The crate has no default features and supports `no_std` with allocation. Enable `std` for standard-library integrations, `serde` for configuration serialization, and `test-utils` for fixtures.

This crate combines the former Base genesis, chain schedule, and execution chain-spec packages. It depends on shared types and has no dependency on the EVM runtime, node services, or networking implementations.
