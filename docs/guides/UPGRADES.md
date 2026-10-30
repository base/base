# Adding a New Upgrade

This guide covers adding a new network upgrade (hard fork) to this repository: where the upgrade is
defined, every place it must be added, and how it is later scheduled on a live chain. The Everest
network upgrade, the most recently added one, is the running example. Replace `Everest` /
`everest` with the new upgrade's name.

---

## How upgrades are modeled

- **`BaseUpgrade`** in [`crates/common/genesis/src/chain/upgrade.rs`](../../crates/common/genesis/src/chain/upgrade.rs)
  is the single upgrade enum, declared with the `hardfork!` macro. `BaseUpgrade::VARIANTS` lists
  every upgrade in chronological order. Two curated subsets sit beside it:
  - `EXECUTION_VARIANTS` is the execution fork ladder that maps onto the reth and revm hardfork
    schedule. It excludes the config-only `Delta` and `PectraBlobSchedule` upgrades and `Zenith`.
  - `CONTRACT_VARIANTS` is the set the L1 upgrade-signal (`ProtocolVersions`) contract can
    schedule. Its order must match the contract's registration order, so only ever append to it.
    It excludes block-activated `Bedrock` and `Zenith`.
- **`Zenith`** stays the last variant. It is a genesis-only gate for testing future features: it is
  never contract-backed, never in a built-in chain schedule, and only activates through genesis
  config. New upgrades go immediately before it.
- **`UpgradeConfig`** stores an optional activation timestamp per upgrade. Inherited upgrades use
  flat `<name>_time` fields; Base-specific upgrades (Azul onward) live in the nested
  `BaseUpgradeConfig`, serialized as `{ "base": { "everest": <timestamp> } }`.
- **`ChainConfig`** in [`crates/common/chains/src/config.rs`](../../crates/common/chains/src/config.rs)
  holds the built-in `MAINNET`, `SEPOLIA`, `DEVNET`, and `ZERONET` schedules. A new upgrade starts as
  `None` (never active) in all four. `DEVNET` activates the upgrades through Azul at genesis
  (except the Sepolia-only Pectra blob schedule) and leaves Beryl through Everest at `None`. Local
  devnets schedule newer upgrades per block through `L2_BASE_<NAME>_BLOCK` instead (see [Devnets and test tooling](#devnets-and-test-tooling)).
- **`BaseUpgradeExt`** in [`crates/common/chains/src/upgrade.rs`](../../crates/common/chains/src/upgrade.rs)
  maps upgrades onto revm specs (`into_eth_spec`), builds each chain's execution ladder
  (`forks_for`, which returns `[(BaseUpgrade, ForkCondition); 14]`, one entry per
  `EXECUTION_VARIANTS` member), and resolves the active upgrade (`from_timestamp`).

---

## Checklist

The compiler finds the exhaustive `match` statements and struct literals that need a new arm or
field, but it does not find the items marked **(manual)**: they fall through a wildcard arm or
stop at the previous upgrade without any error.

### Upgrade definition

- [ ] Add the variant to `BaseUpgrade` before `Zenith`, and describe it in the enum's doc comment.
- [ ] If it changes execution, append it to `EXECUTION_VARIANTS`, bump the array size, and add an
  `execution_idx` arm.
- [ ] If the L1 upgrade signal should schedule it, append it to `CONTRACT_VARIANTS`, bump the array
  size, and add a `from_contract_fork_name` arm **(manual)**. The `ProtocolVersions` contract must
  register the upgrade at the next registration ID.
- [ ] Add a `contract_id` arm, the field to `BaseUpgradeConfig` and `is_empty`, the arms to
  `UpgradeConfig::{activation, set_activation_timestamp, clear_activation_timestamp}`, and the
  field (`None`) to `UpgradeConfig::BASE_MAINNET` and `BASE_SEPOLIA`.

### Activation checks

- [ ] Add `is_everest_active` / `is_first_everest_block` to the activation macro list in
  [`crates/common/genesis/src/rollup.rs`](../../crates/common/genesis/src/rollup.rs), and add the
  upgrade to the first-block activation log chain in the same file **(manual)**.
- [ ] Add `is_everest_active_at_timestamp` to the `Upgrades` trait in
  [`crates/common/chains/src/upgrades.rs`](../../crates/common/chains/src/upgrades.rs), and add the
  variant to the timestamp arm of `impl Upgrades for RollupConfig` **(manual: it otherwise falls
  through to `ForkCondition::Never`)**.

### Chain schedules

- [ ] Add `everest_timestamp: Option<u64>` to `ChainConfig`, set it to `None` in `MAINNET`,
  `SEPOLIA`, `DEVNET`, and `ZERONET`, and map it into the `BaseUpgradeConfig` built by
  `ChainConfig` (`config.rs`).
- [ ] In `BaseUpgradeExt` (`crates/common/chains/src/upgrade.rs`): add the entry to `forks_for` and
  bump its array size (and the `mainnet`/`sepolia`/`devnet`/`zeronet` signatures) **(manual)**; add
  the new check first in `from_timestamp`, since the newest upgrade wins **(manual)**; extend
  `into_eth_spec` only if the paired Ethereum spec changes; add the name to
  `check_base_upgrade_from_str`.

### Execution layer

- [ ] Add `(BaseUpgrade::Everest.boxed(), genesis_info.base.everest)` to the genesis hardfork list
  in [`crates/execution/chainspec/src/spec.rs`](../../crates/execution/chainspec/src/spec.rs)
  **(manual)**.
- [ ] Push the upgrade in `to_chain_upgrades` in
  [`crates/execution/chainspec/src/upgrades.rs`](../../crates/execution/chainspec/src/upgrades.rs)
  **(manual: otherwise a configured timestamp never becomes an execution-layer hardfork)**.
- [ ] Map the upgrade in `BasePrecompiles::new_with_spec` in
  [`crates/common/precompiles/src/provider.rs`](../../crates/common/precompiles/src/provider.rs),
  either to the latest precompile set or to a new one **(manual: an unmapped upgrade panics at
  runtime)**.
- [ ] Add the field to `UpgradeInfo` in
  [`crates/common/rpc-types/src/genesis.rs`](../../crates/common/rpc-types/src/genesis.rs), which
  the chain spec reads genesis timestamps from **(manual)**.

### Operator tooling

- [ ] For contract-backed upgrades, add the metric label to `UPGRADE_METRIC_LABELS` in
  [`crates/consensus/cli/src/metrics.rs`](../../crates/consensus/cli/src/metrics.rs).
- [ ] Add the upgrade to `set_timestamp` and `specs_from_config` in
  [`crates/infra/basectl/src/app/views/upgrades.rs`](../../crates/infra/basectl/src/app/views/upgrades.rs)
  **(manual)**.

### Devnets and test tooling

- [ ] Add the name to `readUpgrades` in the devnet's `op-deployer`
  (`op-deployer/cmd/op-deployer/upgrades.go` in [base/optimism](https://github.com/base/optimism)),
  then bump the commit pinned in
  [`etc/docker/Dockerfile.devnet`](../../etc/docker/Dockerfile.devnet). Until then the devnet
  silently ignores `L2_BASE_EVEREST_BLOCK`.
- [ ] Pass `L2_BASE_EVEREST_BLOCK` through
  [`etc/docker/docker-compose.yml`](../../etc/docker/docker-compose.yml), document it in
  [`etc/docker/devnet-env`](../../etc/docker/devnet-env), and add a `just devnet up` mode in
  [`etc/docker/Justfile`](../../etc/docker/Justfile) if it needs one.
- [ ] Add the name to `FORKS` and `default_forks` in
  [`acceptance/src/config.rs`](../../acceptance/src/config.rs). A unit test fails until `FORKS`
  lists every upgrade from Azul onward. The list is not derived from `BaseUpgrade` because a
  scenario can only schedule an upgrade the pinned op-deployer accepts.
- [ ] Add the timestamp to the genesis `base` map in `ActionEngineClient::build_genesis_for_rollup`
  ([`actions/harness/src/engine.rs`](../../actions/harness/src/engine.rs)) **(manual)**, add a
  `with_everest_at` setter to
  [`actions/harness/src/test_rollup_config.rs`](../../actions/harness/src/test_rollup_config.rs),
  and add a case to `each_case_is_cumulative_without_enabling_the_next_fork` in
  [`actions/harness/src/matrix.rs`](../../actions/harness/src/matrix.rs).

These places derive their upgrade list and need no change:

- `ForkMatrix::all` in the action harness builds its progression from `BaseUpgrade::VARIANTS`.
- `SetupContainer::with_base_activation_block` and
  `SystemTestStackBuilder::with_base_activation_block` in `etc/systems` take a `BaseUpgrade` and
  derive the `L2_BASE_<NAME>_BLOCK` variable from its contract ID.
- [`etc/scripts/devnet/anvil-nitro-local.sh`](../../etc/scripts/devnet/anvil-nitro-local.sh) reads
  contract-backed IDs from the `contract_upgrade_ids` binary and looks up each one under
  `.base.<name>` or `.<name>_time` in the rollup config.
- The upgrade consistency tests in
  [`crates/common/chains/tests/upgrade_consistency.rs`](../../crates/common/chains/tests/upgrade_consistency.rs)
  check every `BaseUpgrade::VARIANTS` entry, asserting that the Base Mainnet and Base Sepolia
  rollup configs agree with `ChainUpgrades` (including `ForkCondition::Never` for unscheduled
  upgrades).

Finally, feature code gates new behavior on `is_everest_active` (consensus) or
`is_everest_active_at_timestamp` (execution).

---

## Scheduling an upgrade

Once an activation time is confirmed for a network, set it in both of that network's schedules:

- the `<name>_timestamp` field of `MAINNET` or `SEPOLIA` in `crates/common/chains/src/config.rs`;
- the matching field of `UpgradeConfig::BASE_MAINNET` or `BASE_SEPOLIA` in
  `crates/common/genesis/src/chain/upgrade.rs`.

Update the tests that pin the latest active upgrade, such as
`BaseUpgrade::from_chain_and_timestamp` expectations and the chain spec tests in
`crates/execution/chainspec/src/spec.rs`. The upgrade consistency tests fail if the two schedules
disagree. "chore(common): schedule Mainnet Denim upgrade" (#6124) is a complete example.
