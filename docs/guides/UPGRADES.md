# Adding a New Upgrade

This guide covers every code change required to introduce a new network upgrade to this repository. Changes are split into two groups: those required for every upgrade, and those that depend on whether the upgrade changes EVM execution rules.

The BaseV1 upgrade is used as the running example throughout. Replace `BaseV1` / `base_v1` / `BASE_V1` with the actual upgrade name.

---

## Architecture overview

Upgrade activation flows through three layers:

1. **Config layer** — `HardForkConfig` stores an optional activation timestamp per upgrade. `RollupConfig` embeds it and exposes `is_X_active(timestamp)` helpers.
2. **Trait layer** — `OpHardfork` (enum) and `OpHardforks` (trait) provide typed, generic activation checks used by both the consensus and execution layers.
3. **Execution layer** — `OpSpecId` maps the active upgrade to an EVM spec. `spec_by_timestamp_after_bedrock` and `RollupConfig::spec_id` resolve which spec to use. `OpPrecompiles` routes to the correct precompile set.

---

## Part 1 — Required for every upgrade

### 1. Add the variant to the `OpHardfork` enum

> The enum is named `OpHardfork` for historical reasons; new entries still represent upgrades.

**File:** [`crates/alloy/hardforks/src/hardfork.rs`](https://github.com/base/base/blob/main/crates/alloy/hardforks/src/hardfork.rs)

Inside the `hardfork!` macro, append the new variant after the current last entry:

```rust
hardfork!(
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[derive(Default)]
    OpHardfork {
        // ... existing variants ...
        /// Jovian: <https://github.com/ethereum-optimism/specs/tree/main/specs/protocol/jovian>
        Jovian,
        /// Base V1: First Base-specific network upgrade.
        BaseV1,   // <-- add here
    }
);
```

Then update all four chain config array methods from `[(Self, ForkCondition); N]` to `N+1` and append the new entry. Mainnet and sepolia use `ForkCondition::Never` until the upgrade is scheduled; devnets use `ForkCondition::ZERO_TIMESTAMP`:

```rust
pub const fn base_mainnet() -> [(Self, ForkCondition); 10] {
    [
        // ... existing entries ...
        (Self::BaseV1, ForkCondition::Never),
    ]
}

pub const fn devnet() -> [(Self, ForkCondition); 10] {
    [
        // ... existing entries ...
        (Self::BaseV1, ForkCondition::ZERO_TIMESTAMP),
    ]
}
```

Update `check_op_hardfork_from_str` in the test module to include the new upgrade variant.

---

### 2. Add the `OpChainHardforks` index arm

**File:** [`crates/alloy/hardforks/src/chain.rs`](https://github.com/base/base/blob/main/crates/alloy/hardforks/src/chain.rs)

Add `BaseV1` to the `use OpHardfork::{...}` import and add a match arm to `Index<OpHardfork>`:

```rust
use OpHardfork::{
    BaseV1, Bedrock, Canyon, Ecotone, Fjord, Granite, Holocene, Isthmus, Jovian, Regolith,
};

impl Index<OpHardfork> for OpChainHardforks {
    fn index(&self, hf: OpHardfork) -> &Self::Output {
        match hf {
            // ... existing arms ...
            Jovian  => &self.forks[Jovian.idx()].1,
            BaseV1  => &self.forks[BaseV1.idx()].1,  // <-- add
        }
    }
}
```

---

### 3. Add the config field and nested struct

**File:** [`crates/consensus/genesis/src/chain/hardfork.rs`](https://github.com/base/base/blob/main/crates/consensus/genesis/src/chain/hardfork.rs)

For standard upgrades (flat timestamp field), add directly to `HardForkConfig`:

```rust
/// `base_v1_time` sets the activation time for the Base V1 network upgrade.
#[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
pub base_v1_time: Option<u64>,
```

For namespaced upgrades with the `{ "base": { "v1": <timestamp> } }` JSON shape, define a sub-struct and embed it:

```rust
/// Hardfork configuration for Base-specific upgrades.
#[derive(Debug, Copy, Clone, Default, Hash, Eq, PartialEq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct BaseHardforkConfig {
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub v1: Option<u64>,
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

Add `is_X_active` and `is_first_X_block` after the previous upgrade's methods. Update the previous terminal upgrade to cascade into the new one:

```rust
/// Returns true if Jovian is active at the given timestamp.
pub fn is_jovian_active(&self, timestamp: u64) -> bool {
    self.hardforks.jovian_time.is_some_and(|t| timestamp >= t)
        || self.is_base_v1_active(timestamp)  // <-- cascade to next fork
}

/// Returns true if Base V1 is active at the given timestamp.
pub fn is_base_v1_active(&self, timestamp: u64) -> bool {
    self.hardforks.base.as_ref().and_then(|b| b.v1).is_some_and(|t| timestamp >= t)
}

/// Returns true if the timestamp marks the first Base V1 block.
pub fn is_first_base_v1_block(&self, timestamp: u64) -> bool {
    self.is_base_v1_active(timestamp)
        && !self.is_base_v1_active(timestamp.saturating_sub(self.block_time))
}
```

> **Note on standalone upgrades:** Some upgrades (e.g. `pectra_blob_schedule`, `BaseV1`) do not participate in the cascade — they are activated independently. For these, omit the `|| self.is_next_active(timestamp)` call and do not cascade the previous upgrade into them.

Also update `op_fork_activation` in `impl OpHardforks for RollupConfig` to add the new arm and update the previous terminal arm's fallback:

```rust
OpHardfork::Jovian => self
    .hardforks
    .jovian_time
    .map(ForkCondition::Timestamp)
    .unwrap_or_else(|| self.op_fork_activation(OpHardfork::BaseV1)),  // <-- cascade
OpHardfork::BaseV1 => self
    .hardforks
    .base
    .as_ref()
    .and_then(|b| b.v1)
    .map(ForkCondition::Timestamp)
    .unwrap_or(ForkCondition::Never),
_ => ForkCondition::Never,  // required: OpHardfork is #[non_exhaustive]
```

---

### 5. Add the trait method

**File:** [`crates/alloy/hardforks/src/hardforks.rs`](https://github.com/base/base/blob/main/crates/alloy/hardforks/src/hardforks.rs)

```rust
/// Returns `true` if [`BaseV1`](OpHardfork::BaseV1) is active at given block timestamp.
fn is_base_v1_active_at_timestamp(&self, timestamp: u64) -> bool {
    self.op_fork_activation(OpHardfork::BaseV1).active_at_timestamp(timestamp)
}
```

---

### 6. Update timestamp constants and test fixtures

**Files:**
- [`crates/alloy/hardforks/src/mainnet.rs`](https://github.com/base/base/blob/main/crates/alloy/hardforks/src/mainnet.rs)
- [`crates/alloy/hardforks/src/sepolia.rs`](https://github.com/base/base/blob/main/crates/alloy/hardforks/src/sepolia.rs)
- [`crates/alloy/hardforks/src/devnet_0_sepolia_dev_0.rs`](https://github.com/base/base/blob/main/crates/alloy/hardforks/src/devnet_0_sepolia_dev_0.rs)
- [`crates/alloy/hardforks/src/lib.rs`](https://github.com/base/base/blob/main/crates/alloy/hardforks/src/lib.rs)
- [`crates/consensus/registry/src/test_utils/base_mainnet.rs`](https://github.com/base/base/blob/main/crates/consensus/registry/src/test_utils/base_mainnet.rs)
- [`crates/consensus/registry/src/test_utils/base_sepolia.rs`](https://github.com/base/base/blob/main/crates/consensus/registry/src/test_utils/base_sepolia.rs)

Add named constants once an activation timestamp is confirmed:

```rust
// mainnet.rs
/// Base V1 mainnet activation timestamp.
pub const BASE_MAINNET_BASE_V1_TIMESTAMP: u64 = <timestamp>;

// sepolia.rs
/// Base V1 sepolia activation timestamp.
pub const BASE_SEPOLIA_BASE_V1_TIMESTAMP: u64 = <timestamp>;
```

Re-export from `lib.rs` alongside the other timestamp constants.

Update the `HardForkConfig` literal in both registry fixture files:

```rust
hardforks: HardForkConfig {
    // ... existing fields ...
    jovian_time: Some(BASE_MAINNET_JOVIAN_TIMESTAMP),
    base: Some(BaseHardforkConfig { v1: Some(BASE_MAINNET_BASE_V1_TIMESTAMP) }),
},
```

Until an activation timestamp is confirmed, leave `base: None` and the chain arrays at `ForkCondition::Never`.

---

### 7. Update the default rollup config

**File:** [`crates/proof/tee/core/src/config/defaults.rs`](https://github.com/base/base/blob/main/crates/proof/tee/core/src/config/defaults.rs)

The `default_rollup_config()` function sets all upgrades active at genesis for dev use. Add the new upgrade:

```rust
hardforks: HardForkConfig {
    // ... existing fields ...
    jovian_time: Some(0),
    base: Some(BaseHardforkConfig { v1: Some(0) }),
},
```

---

### 8. Verify the upgrade consistency tests

**File:** [`crates/consensus/registry/tests/hardfork_consistency.rs`](https://github.com/base/base/blob/main/crates/consensus/registry/tests/hardfork_consistency.rs)

These tests assert that `BASE_MAINNET_CONFIG.op_fork_activation(fork)` matches `OpChainHardforks::base_mainnet().op_fork_activation(fork)` for every `OpHardfork` variant. They should pass without changes as long as both sides consistently return `ForkCondition::Never` for an unscheduled upgrade or the same timestamp once scheduled.

If there is a known discrepancy (e.g. the cascade causes a mismatch for an unset upgrade), add a skip with an explanatory comment as done for `Regolith`:

```rust
if *fork == OpHardfork::BaseV1 {
    continue; // explanation of why the two sides differ
}
```

---

## Part 2 — Required when the upgrade changes EVM execution

Skip this section if the upgrade only affects protocol-level behavior (batch decoding, derivation rules, system config) without introducing new EVM opcodes, precompile addresses, or gas rule changes.

### 9. Add the `OpSpecId` variant

**File:** [`crates/execution/revm/src/spec.rs`](https://github.com/base/base/blob/main/crates/execution/revm/src/spec.rs)

```rust
pub enum OpSpecId {
    // ... existing variants ...
    JOVIAN,
    BASE_V1,  // <-- add
    OSAKA,
}
```

Extend `into_eth_spec()` — if no new Ethereum EL upgrade is paired, reuse the previous mapping:

```rust
Self::ISTHMUS | Self::JOVIAN | Self::BASE_V1 => SpecId::PRAGUE,
```

Add the string name and wire up `FromStr` and `From<OpSpecId> for &'static str`:

```rust
// name module
pub const BASE_V1: &str = "BaseV1";

// FromStr
name::BASE_V1 => Ok(Self::BASE_V1),

// From<OpSpecId> for &'static str
OpSpecId::BASE_V1 => name::BASE_V1,
```

---

### 10. Route precompiles

**File:** [`crates/execution/revm/src/precompiles.rs`](https://github.com/base/base/blob/main/crates/execution/revm/src/precompiles.rs)

If the upgrade introduces new precompiles, create a new `base_v1()` function. If it reuses the previous set, extend the existing arm:

```rust
// Reuse previous precompile set
OpSpecId::OSAKA | OpSpecId::JOVIAN | OpSpecId::BASE_V1 => jovian(),

// Or add a new set
OpSpecId::BASE_V1 => base_v1(),
```

Export any new precompile module from `lib.rs`.

---

### 11. Update spec resolution

**File:** [`crates/alloy/evm/src/spec_id.rs`](https://github.com/base/base/blob/main/crates/alloy/evm/src/spec_id.rs)

Add the new upgrade as the first check (newest upgrade wins):

```rust
pub fn spec_by_timestamp_after_bedrock(chain_spec: impl OpHardforks, timestamp: u64) -> OpSpecId {
    if chain_spec.is_base_v1_active_at_timestamp(timestamp) {
        OpSpecId::BASE_V1
    } else if chain_spec.is_jovian_active_at_timestamp(timestamp) {
        OpSpecId::JOVIAN
    } // ... remaining checks unchanged
}
```

**File:** [`crates/consensus/genesis/src/rollup.rs`](https://github.com/base/base/blob/main/crates/consensus/genesis/src/rollup.rs)

Same pattern in the `#[cfg(feature = "revm")] impl RollupConfig` block:

```rust
pub fn spec_id(&self, timestamp: u64) -> base_revm::OpSpecId {
    if self.is_base_v1_active(timestamp) {
        base_revm::OpSpecId::BASE_V1
    } else if self.is_jovian_active(timestamp) {
        base_revm::OpSpecId::JOVIAN
    } // ... remaining checks unchanged
}
```

---

### 12. Update the reth `ChainHardforks` builder

**File:** [`crates/execution/hardforks/src/chain.rs`](https://github.com/base/base/blob/main/crates/execution/hardforks/src/chain.rs)

Append the new upgrade in `to_chain_hardforks()`. If it pairs with a new Ethereum upgrade (like Canyon→Shanghai), push both; if not, push only the OP upgrade entry:

```rust
// No paired Ethereum hardfork
forks.push((OpHardfork::Jovian.boxed(), self[OpHardfork::Jovian]));
forks.push((OpHardfork::BaseV1.boxed(), self[OpHardfork::BaseV1]));  // <-- add
```

---

## Checklist

### Always required

- [ ] `OpHardfork` variant added in `hardfork.rs`; all four chain arrays updated
- [ ] `Index<OpHardfork>` arm added in `chain.rs`
- [ ] Config field (flat or nested struct) added to `HardForkConfig` in `hardfork.rs`; `iter()` updated; new types re-exported
- [ ] `is_X_active` + `is_first_X_block` added to `RollupConfig`; `op_fork_activation` arm added; previous terminal upgrade cascades to new one (unless standalone)
- [ ] `is_X_active_at_timestamp` added to `OpHardforks` trait
- [ ] Timestamp constants added to `mainnet.rs`, `sepolia.rs`, `devnet_0_sepolia_dev_0.rs`; re-exported from `lib.rs`
- [ ] Registry fixtures (`base_mainnet.rs`, `base_sepolia.rs`) updated
- [ ] Default rollup config updated (`defaults.rs`)
- [ ] Upgrade consistency tests pass

### Required when EVM execution changes

- [ ] `OpSpecId` variant added with `into_eth_spec`, `FromStr`, `From<&str>`, `name::X`
- [ ] Precompile match arm updated (or new precompile set added)
- [ ] `spec_by_timestamp_after_bedrock` updated (`alloy/evm/src/spec_id.rs`)
- [ ] `RollupConfig::spec_id` updated (`consensus/genesis/src/rollup.rs`)
- [ ] `to_chain_hardforks` updated (`execution/hardforks/src/chain.rs`)
