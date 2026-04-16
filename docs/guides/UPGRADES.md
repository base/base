# Adding a New Upgrade

This guide covers every code change required to introduce a new network upgrade to this repository. Changes are split into two groups: those required for every upgrade, and those that depend on whether the upgrade changes EVM execution rules.

The Azul upgrade is used as the running example throughout. Replace `Azul` / `azul` / `BASE_AZUL` with the actual upgrade name.

---

## Architecture overview

Upgrade activation flows through three layers:

1. **Config layer** — `HardForkConfig` stores an optional activation timestamp per upgrade. `RollupConfig` embeds it and exposes `is_X_active(timestamp)` helpers.
2. **Trait layer** — `BaseUpgrade` (enum) and `BaseUpgrades` (trait) provide typed, generic activation checks used by both the consensus and execution layers.
3. **Execution layer** — `OpSpecId` maps the active upgrade to an EVM spec. `spec_by_timestamp_after_bedrock` and `RollupConfig::spec_id` resolve which spec to use. `BasePrecompiles` routes to the correct precompile set.

---

## Part 1 — Required for every upgrade

### 1. Add the variant to the `BaseUpgrade` enum

**File:** [`crates/common/chains/src/upgrade.rs`](../../crates/common/chains/src/upgrade.rs)

Inside the `hardfork!` macro, append the new variant after the current last entry:

```rust
hardfork!(
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[derive(Default)]
    BaseUpgrade {
        // ... existing variants ...
        /// Jovian: <https://github.com/ethereum-optimism/specs/tree/main/specs/protocol/jovian>
        Jovian,
        /// Azul: First Base-specific network upgrade.
        Azul,   // <-- add here
    }
);
```

Then update all four chain config array methods from `[(Self, ForkCondition); N]` to `N+1` and append the new entry. Mainnet and sepolia use `ForkCondition::Never` until the upgrade is scheduled; the generic devnet uses `ForkCondition::ZERO_TIMESTAMP`:

```rust
pub const fn mainnet() -> [(Self, ForkCondition); 10] {
    [
        // ... existing entries ...
        (Self::Azul, ForkCondition::Never),
    ]
}

pub const fn devnet() -> [(Self, ForkCondition); 10] {
    [
        // ... existing entries ...
        (Self::Azul, ForkCondition::ZERO_TIMESTAMP),
    ]
}
```

For named devnets like `base_devnet_0_sepolia_dev_0`, use the same timestamp as the previous upgrade rather than `ZERO_TIMESTAMP`, so the new upgrade does not activate before the one it follows:

```rust
pub const fn base_devnet_0_sepolia_dev_0() -> [(Self, ForkCondition); 10] {
    [
        // ... existing entries ...
        (Self::Jovian, ForkCondition::Timestamp(BASE_DEVNET_0_SEPOLIA_DEV_0_JOVIAN_TIMESTAMP)),
        (Self::Azul, ForkCondition::Timestamp(BASE_DEVNET_0_SEPOLIA_DEV_0_JOVIAN_TIMESTAMP)),
    ]
}
```

Update `check_base_upgrade_from_str` in the test module to include the new upgrade variant.

---

### 2. Add the `BaseChainUpgrades` index arm

**File:** [`crates/common/chains/src/chain.rs`](../../crates/common/chains/src/chain.rs)

Add `Azul` to the `use BaseUpgrade::{...}` import and add a match arm to `Index<BaseUpgrade>`:

```rust
use BaseUpgrade::{
    Azul, Bedrock, Canyon, Ecotone, Fjord, Granite, Holocene, Isthmus, Jovian, Regolith,
};

impl Index<BaseUpgrade> for BaseChainUpgrades {
    fn index(&self, hf: BaseUpgrade) -> &Self::Output {
        match hf {
            // ... existing arms ...
            Jovian  => &self.forks[Jovian.idx()].1,
            Azul    => &self.forks[Azul.idx()].1,  // <-- add
        }
    }
}
```

---

### 3. Add the config field and nested struct

**File:** [`crates/consensus/genesis/src/chain/hardfork.rs`](https://github.com/base/base/blob/main/crates/consensus/genesis/src/chain/hardfork.rs)

For standard upgrades (flat timestamp field), add directly to `HardForkConfig`:

```rust
/// `azul_time` sets the activation time for the Base Azul network upgrade.
#[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
pub azul_time: Option<u64>,
```

For namespaced upgrades with the `{ "base": { "azul": <timestamp> } }` JSON shape, define a sub-struct and embed it:

```rust
/// Hardfork configuration for Base-specific upgrades.
#[derive(Debug, Copy, Clone, Default, Hash, Eq, PartialEq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct BaseHardforkConfig {
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub azul: Option<u64>,
}

pub struct HardForkConfig {
    // ... existing fields ...
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub base: Option<BaseHardforkConfig>,
}
```

Also update `HardForkConfig::iter()` to include the new entry, and re-export any new public types from `crates/consensus/genesis/src/chain/mod.rs` and `crates/consensus/genesis/src/lib.rs`.

---

### 4. Add activation methods to `RollupConfig`

**File:** [`crates/consensus/genesis/src/rollup.rs`](https://github.com/base/base/blob/main/crates/consensus/genesis/src/rollup.rs)

Add `is_X_active` and `is_first_X_block` after the previous upgrade's methods.

There are two patterns depending on whether the new upgrade is **standalone** or **cascading**:

**Standalone** (e.g. `pectra_blob_schedule`, `Azul`) — activated independently, never implied by a later upgrade being active. Use this pattern when the upgrade affects only protocol-level behavior and is not a prerequisite for the next upgrade:

```rust
/// Returns true if Base Azul is active at the given timestamp.
pub fn is_base_azul_active(&self, timestamp: u64) -> bool {
    self.hardforks.base.as_ref().and_then(|b| b.azul).is_some_and(|t| timestamp >= t)
}

/// Returns true if the timestamp marks the first Base Azul block.
pub fn is_first_base_azul_block(&self, timestamp: u64) -> bool {
    self.is_base_azul_active(timestamp)
        && !self.is_base_azul_active(timestamp.saturating_sub(self.block_time))
}
```

The previous terminal upgrade's `is_X_active` method is left unchanged (no cascade added).

**Cascading** (e.g. `Canyon`, `Ecotone`, `Isthmus`) — the previous upgrade is considered active whenever the new one is. Update the previous terminal upgrade's method and add the new one:

```rust
/// Returns true if Jovian is active at the given timestamp.
pub fn is_jovian_active(&self, timestamp: u64) -> bool {
    self.hardforks.jovian_time.is_some_and(|t| timestamp >= t)
        || self.is_next_active(timestamp)  // <-- cascade to next fork
}

/// Returns true if Next is active at the given timestamp.
pub fn is_next_active(&self, timestamp: u64) -> bool {
    self.hardforks.next_time.is_some_and(|t| timestamp >= t)
}
```

Also update `upgrade_activation` in `impl BaseUpgrades for RollupConfig` to add the new arm. For **standalone** upgrades, the previous arm keeps `unwrap_or(ForkCondition::Never)`:

```rust
BaseUpgrade::Jovian => self
    .hardforks
    .jovian_time
    .map(ForkCondition::Timestamp)
    .unwrap_or(ForkCondition::Never),  // standalone: no cascade
BaseUpgrade::Azul => self
    .hardforks
    .base
    .as_ref()
    .and_then(|b| b.azul)
    .map(ForkCondition::Timestamp)
    .unwrap_or(ForkCondition::Never),
_ => ForkCondition::Never,  // required: BaseUpgrade is #[non_exhaustive]
```

For **cascading** upgrades, replace the previous arm's `unwrap_or(ForkCondition::Never)` with `.unwrap_or_else(|| self.upgrade_activation(BaseUpgrade::Next))`.

---

### 5. Add the trait method

**File:** [`crates/common/chains/src/upgrades.rs`](../../crates/common/chains/src/upgrades.rs)

```rust
/// Returns `true` if [`Azul`](BaseUpgrade::Azul) is active at given block timestamp.
fn is_base_azul_active_at_timestamp(&self, timestamp: u64) -> bool {
    self.upgrade_activation(BaseUpgrade::Azul).active_at_timestamp(timestamp)
}
```

---

### 6. Update timestamp constants and test fixtures

**Files:**
- [`crates/common/chains/src/upgrade.rs`](../../crates/common/chains/src/upgrade.rs) (mainnet, sepolia, devnet constants)
- [`crates/common/chains/src/lib.rs`](../../crates/common/chains/src/lib.rs)
- [`crates/consensus/registry/src/test_utils/mod.rs`](https://github.com/base/base/blob/main/crates/consensus/registry/src/test_utils/mod.rs)

Add named constants once an activation timestamp is confirmed:

```rust
// mainnet.rs
/// Base Azul mainnet activation timestamp.
pub const BASE_MAINNET_BASE_AZUL_TIMESTAMP: u64 = <timestamp>;

// sepolia.rs
/// Base Azul sepolia activation timestamp.
pub const BASE_SEPOLIA_BASE_AZUL_TIMESTAMP: u64 = <timestamp>;
```

Re-export from `lib.rs` alongside the other timestamp constants.

Update the `HardForkConfig` literal in both registry fixture files:

```rust
hardforks: HardForkConfig {
    // ... existing fields ...
    jovian_time: Some(BASE_MAINNET_JOVIAN_TIMESTAMP),
    base: Some(BaseHardforkConfig { azul: Some(BASE_MAINNET_BASE_AZUL_TIMESTAMP) }),
},
```

Until an activation timestamp is confirmed, leave `base: None` and the chain arrays at `ForkCondition::Never`.

---

### 7. Update the default rollup config

**File:** [`crates/consensus/registry/src/test_utils/mod.rs`](https://github.com/base/base/blob/main/crates/consensus/registry/src/test_utils/mod.rs)

The `default_rollup_config()` function sets all upgrades active at genesis for dev use. Add the new upgrade:

```rust
hardforks: HardForkConfig {
    // ... existing fields ...
    jovian_time: Some(0),
    base: Some(BaseHardforkConfig { azul: Some(0) }),
},
```

---

### 8. Verify the upgrade consistency tests

**File:** [`crates/consensus/registry/tests/hardfork_consistency.rs`](https://github.com/base/base/blob/main/crates/consensus/registry/tests/hardfork_consistency.rs)

These tests assert that `BaseChainConfig::mainnet().upgrade_activation(fork)` matches `BaseChainUpgrades::mainnet().upgrade_activation(fork)` for every `BaseUpgrade` variant. They should pass without changes as long as both sides consistently return `ForkCondition::Never` for an unscheduled upgrade or the same timestamp once scheduled.

If there is a known discrepancy (e.g. the cascade causes a mismatch for an unset upgrade), add a skip with an explanatory comment as done for `Regolith`:

```rust
if *fork == BaseUpgrade::Azul {
    continue; // explanation of why the two sides differ
}
```

---

## Part 2 — Required when the upgrade changes EVM execution

Skip this section if the upgrade only affects protocol-level behavior (batch decoding, derivation rules, system config) without introducing new EVM opcodes, precompile addresses, or gas rule changes.

### 9. Add the `OpSpecId` variant

**File:** [`crates/common/evm/src/spec.rs`](https://github.com/base/base/blob/main/crates/common/evm/src/spec.rs)

```rust
pub enum OpSpecId {
    // ... existing variants ...
    JOVIAN,
    AZUL,  // <-- add
    OSAKA,
}
```

Extend `into_eth_spec()` — if no new Ethereum EL upgrade is paired, reuse the previous mapping:

```rust
Self::ISTHMUS | Self::JOVIAN | Self::AZUL => SpecId::PRAGUE,
```

Add a `#[strum(serialize = "...")]` attribute on the new variant with its canonical string name:

```rust
/// Base Azul spec id.
#[strum(serialize = "Azul")]
AZUL,
```

`FromStr` and `From<OpSpecId> for &'static str` are derived automatically.

---

### 10. Route precompiles

**File:** [`crates/common/evm/src/precompiles/provider.rs`](https://github.com/base/base/blob/main/crates/common/evm/src/precompiles/provider.rs)

If the upgrade introduces new precompiles, add a new `pub fn azul()` method on `BasePrecompiles`. If it reuses the previous set, extend the existing arm in `new_with_spec`:

```rust
// Reuse previous precompile set
OpSpecId::JOVIAN | OpSpecId::AZUL => Self::jovian(),

// Or add a new set
OpSpecId::AZUL => Self::azul(),
```

---

### 11. Update spec resolution

**File:** [`crates/common/evm/src/spec.rs`](https://github.com/base/base/blob/main/crates/common/evm/src/spec.rs)

Add the new upgrade as the first check (newest upgrade wins):

```rust
pub fn spec_by_timestamp_after_bedrock(chain_spec: impl BaseUpgrades, timestamp: u64) -> OpSpecId {
    if chain_spec.is_base_azul_active_at_timestamp(timestamp) {
        OpSpecId::AZUL
    } else if chain_spec.is_jovian_active_at_timestamp(timestamp) {
        OpSpecId::JOVIAN
    } // ... remaining checks unchanged
}
```

**File:** [`crates/consensus/genesis/src/rollup.rs`](https://github.com/base/base/blob/main/crates/consensus/genesis/src/rollup.rs)

Same pattern in the `#[cfg(feature = "revm")] impl RollupConfig` block:

```rust
pub fn spec_id(&self, timestamp: u64) -> base_revm::OpSpecId {
    if self.is_base_azul_active(timestamp) {
        base_revm::OpSpecId::AZUL
    } else if self.is_jovian_active(timestamp) {
        base_revm::OpSpecId::JOVIAN
    } // ... remaining checks unchanged
}
```

---

### 12. Update the reth `ChainHardforks` builder

**File:** [`crates/execution/upgrades/src/chain.rs`](https://github.com/base/base/blob/main/crates/execution/upgrades/src/chain.rs)

Append the new upgrade in `to_chain_hardforks()`. If it pairs with a new Ethereum upgrade (like Canyon→Shanghai), push both; if not, push only the Base upgrade entry:

```rust
// No paired Ethereum hardfork
forks.push((BaseUpgrade::Jovian.boxed(), self[BaseUpgrade::Jovian]));
forks.push((BaseUpgrade::Azul.boxed(), self[BaseUpgrade::Azul]));  // <-- add
```

---

## Checklist

### Always required

- [ ] `BaseUpgrade` variant added in `upgrade.rs`; all four chain arrays updated
- [ ] `Index<BaseUpgrade>` arm added in `chain.rs`
- [ ] Config field (flat or nested struct) added to `HardForkConfig` in `upgrade.rs`; `iter()` updated; new types re-exported
- [ ] `is_X_active` + `is_first_X_block` added to `RollupConfig`; `upgrade_activation` arm added; previous terminal upgrade cascades to new one (unless standalone)
- [ ] `is_X_active_at_timestamp` added to `BaseUpgrades` trait
- [ ] Timestamp constants added to `mainnet.rs`, `sepolia.rs`, `devnet_0_sepolia_dev_0.rs`; re-exported from `lib.rs`
- [ ] Registry fixtures (`test_utils/mod.rs`) updated
- [ ] Default rollup config updated (`defaults.rs`)
- [ ] Upgrade consistency tests pass

### Required when EVM execution changes

- [ ] `OpSpecId` variant added with `into_eth_spec` mapping and `#[strum(serialize = "...")]` attribute
- [ ] Precompile match arm updated (or new precompile set added)
- [ ] `spec_by_timestamp_after_bedrock` updated (`common/evm/src/spec.rs`)
- [ ] `RollupConfig::spec_id` updated (`consensus/genesis/src/rollup.rs`)
- [ ] `to_chain_hardforks` updated (`execution/upgrades/src/chain.rs`)
