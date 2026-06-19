//! Contains the upgrade configuration for the chain.

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
};
use core::fmt::Display;

use alloy_hardforks::{EthereumHardfork, hardfork};
use spin::{Once, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Upgrade configuration for Base-specific upgrades.
#[derive(Debug, Copy, Clone, Default, Hash, Eq, PartialEq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct BaseUpgradeConfig {
    /// `azul` sets the activation time for the Base Azul network upgrade.
    /// Active if `azul` != None && L2 block timestamp >= `Some(azul)`, inactive otherwise.
    #[cfg_attr(feature = "serde", serde(alias = "v1", skip_serializing_if = "Option::is_none"))]
    pub azul: Option<u64>,
    /// `beryl` sets the activation time for the Beryl network upgrade.
    /// Active if `beryl` != None && L2 block timestamp >= `Some(beryl)`, inactive otherwise.
    #[cfg_attr(feature = "serde", serde(alias = "v2", skip_serializing_if = "Option::is_none"))]
    pub beryl: Option<u64>,
    /// `cobalt` sets the activation time for the Cobalt network upgrade.
    /// Active if `cobalt` != None && L2 block timestamp >= `Some(cobalt)`, inactive otherwise.
    #[cfg_attr(feature = "serde", serde(alias = "v3", skip_serializing_if = "Option::is_none"))]
    pub cobalt: Option<u64>,
}

impl BaseUpgradeConfig {
    /// Returns true if no Base-specific upgrades are configured.
    pub const fn is_empty(&self) -> bool {
        self.azul.is_none() && self.beryl.is_none() && self.cobalt.is_none()
    }
}

hardfork!(
    /// The canonical Base network upgrade.
    ///
    /// This single enum spans two domains:
    /// - the **execution fork ladder** ([`BaseUpgrade::EXECUTION_VARIANTS`]) that maps onto the
    ///   reth/revm hardfork schedule, and
    /// - the **contract-backed upgrade set** ([`BaseUpgrade::CONTRACT_VARIANTS`]) that is keyed by
    ///   the L1 upgrade-signal contract `hardforkId` strings and the genesis [`UpgradeConfig`]
    ///   timestamp fields.
    ///
    /// Variants are listed in chronological order. [`Bedrock`](BaseUpgrade::Bedrock) is
    /// execution-only (block-activated, not contract-backed), while
    /// [`Delta`](BaseUpgrade::Delta) and [`PectraBlobSchedule`](BaseUpgrade::PectraBlobSchedule)
    /// are contract-backed config upgrades that do not change EVM execution and therefore never
    /// enter the execution fork ladder.
    ///
    /// When building a list of upgrades for a chain, it's still expected to zip with
    /// [`EthereumHardfork`](alloy_hardforks::EthereumHardfork).
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[derive(Default)]
    BaseUpgrade {
        /// Bedrock: <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/superchain-upgrades.md#bedrock>.
        Bedrock,
        /// Regolith: <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/superchain-upgrades.md#regolith>.
        Regolith,
        /// Canyon: <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/superchain-upgrades.md#canyon>.
        Canyon,
        /// Delta: <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/superchain-upgrades.md#delta>.
        Delta,
        /// Ecotone: <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/superchain-upgrades.md#ecotone>.
        Ecotone,
        /// Fjord: <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/superchain-upgrades.md#fjord>
        Fjord,
        /// Granite: <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/superchain-upgrades.md#granite>
        Granite,
        /// Holocene: <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/superchain-upgrades.md#holocene>
        Holocene,
        /// Pectra blob schedule: an optional fork present on Base Sepolia chains that observed the
        /// L1 Pectra network upgrade with the reference node `<=v1.11.1` sequencing the network.
        PectraBlobSchedule,
        /// Isthmus: <https://github.com/ethereum-optimism/specs/blob/main/specs/protocol/isthmus/overview.md>
        Isthmus,
        /// Jovian: Base network upgrade.
        Jovian,
        /// Azul: First Base-specific network upgrade.
        #[default]
        Azul,
        /// Beryl: Second Base-specific network upgrade.
        Beryl,
        /// Cobalt: Third Base-specific network upgrade.
        Cobalt,
    }
);

impl BaseUpgrade {
    /// Latest Base upgrade used by default.
    pub const LATEST: Self = Self::Azul;

    /// The execution fork ladder, in activation order.
    ///
    /// These are the upgrades that participate in the reth/revm hardfork schedule. Excludes the
    /// contract-only [`Delta`](Self::Delta) and [`PectraBlobSchedule`](Self::PectraBlobSchedule)
    /// upgrades, which do not change EVM execution.
    pub const EXECUTION_VARIANTS: [Self; 12] = [
        Self::Bedrock,
        Self::Regolith,
        Self::Canyon,
        Self::Ecotone,
        Self::Fjord,
        Self::Granite,
        Self::Holocene,
        Self::Isthmus,
        Self::Jovian,
        Self::Azul,
        Self::Beryl,
        Self::Cobalt,
    ];

    /// The contract-backed upgrade set, in activation order.
    ///
    /// These are the upgrades addressable by the L1 upgrade-signal contract and stored in the
    /// genesis [`UpgradeConfig`]. Excludes block-activated [`Bedrock`](Self::Bedrock).
    pub const CONTRACT_VARIANTS: [Self; 13] = [
        Self::Regolith,
        Self::Canyon,
        Self::Delta,
        Self::Ecotone,
        Self::Fjord,
        Self::Granite,
        Self::Holocene,
        Self::PectraBlobSchedule,
        Self::Isthmus,
        Self::Jovian,
        Self::Azul,
        Self::Beryl,
        Self::Cobalt,
    ];

    /// Returns true if this upgrade participates in the execution fork ladder.
    pub const fn is_execution(self) -> bool {
        self.execution_idx().is_some()
    }

    /// Returns true if this upgrade is contract-backed (i.e. signaled by the L1 upgrade-signal
    /// contract and stored in [`UpgradeConfig`]). False only for [`Bedrock`](Self::Bedrock).
    pub const fn is_contract_backed(self) -> bool {
        !matches!(self, Self::Bedrock)
    }

    /// Returns this upgrade's index within [`EXECUTION_VARIANTS`](Self::EXECUTION_VARIANTS), or
    /// `None` for contract-only upgrades that are absent from the execution fork ladder.
    pub const fn execution_idx(self) -> Option<usize> {
        Some(match self {
            Self::Bedrock => 0,
            Self::Regolith => 1,
            Self::Canyon => 2,
            Self::Ecotone => 3,
            Self::Fjord => 4,
            Self::Granite => 5,
            Self::Holocene => 6,
            Self::Isthmus => 7,
            Self::Jovian => 8,
            Self::Azul => 9,
            Self::Beryl => 10,
            Self::Cobalt => 11,
            Self::Delta | Self::PectraBlobSchedule => return None,
        })
    }

    /// Returns the canonical `snake_case` contract upgrade ID used by the L1 upgrade-signal
    /// contract and metrics.
    ///
    /// Note this differs from [`name`](Self::name) (`PascalCase`), which is the reth/execution
    /// hardfork identity.
    pub const fn contract_id(self) -> &'static str {
        match self {
            Self::Bedrock => "bedrock",
            Self::Regolith => "regolith",
            Self::Canyon => "canyon",
            Self::Delta => "delta",
            Self::Ecotone => "ecotone",
            Self::Fjord => "fjord",
            Self::Granite => "granite",
            Self::Holocene => "holocene",
            Self::PectraBlobSchedule => "pectra_blob_schedule",
            Self::Isthmus => "isthmus",
            Self::Jovian => "jovian",
            Self::Azul => "azul",
            Self::Beryl => "beryl",
            Self::Cobalt => "cobalt",
        }
    }

    /// Returns the Ethereum execution hardfork activated by this upgrade, if any.
    pub const fn execution_hardfork(self) -> Option<EthereumHardfork> {
        match self {
            Self::Canyon => Some(EthereumHardfork::Shanghai),
            Self::Ecotone => Some(EthereumHardfork::Cancun),
            Self::Isthmus => Some(EthereumHardfork::Prague),
            Self::Azul => Some(EthereumHardfork::Osaka),
            _ => None,
        }
    }

    /// Returns the Base upgrade that carries the given Ethereum hardfork on Base.
    pub const fn from_ethereum_hardfork(fork: EthereumHardfork) -> Option<Self> {
        match fork {
            EthereumHardfork::Shanghai => Some(Self::Canyon),
            EthereumHardfork::Cancun => Some(Self::Ecotone),
            EthereumHardfork::Prague => Some(Self::Isthmus),
            EthereumHardfork::Osaka => Some(Self::Azul),
            _ => None,
        }
    }

    /// Returns the contract-backed upgrade represented by an execution, Base, or contract alias
    /// name. Returns `None` for unknown names and for non-contract-backed upgrades (Bedrock).
    pub fn from_contract_fork_name(name: &str) -> Option<Self> {
        let upgrade = match Self::normalized_hardfork_id(name).as_str() {
            "regolith" => Self::Regolith,
            "shanghai" | "canyon" => Self::Canyon,
            "delta" => Self::Delta,
            "cancun" | "ecotone" => Self::Ecotone,
            "fjord" => Self::Fjord,
            "granite" => Self::Granite,
            "holocene" => Self::Holocene,
            "pectrablobschedule" => Self::PectraBlobSchedule,
            "prague" | "isthmus" => Self::Isthmus,
            "jovian" => Self::Jovian,
            "osaka" | "azul" | "baseazul" | "v1" => Self::Azul,
            "beryl" | "baseberyl" | "v2" => Self::Beryl,
            "cobalt" | "basecobalt" | "v3" => Self::Cobalt,
            _ => return None,
        };
        Some(upgrade)
    }

    /// Normalizes a contract upgrade ID for matching (lowercase, stripping whitespace, `_`, `-`).
    pub fn normalized_hardfork_id(upgrade_id: &str) -> String {
        upgrade_id
            .bytes()
            .filter(|b| !b.is_ascii_whitespace() && !matches!(b, b'_' | b'-'))
            .map(|b| b.to_ascii_lowercase() as char)
            .collect()
    }
}

/// Runtime upgrade activation override.
#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
pub enum UpgradeActivation {
    /// The upgrade is not activated.
    Never,
    /// The upgrade activates at the given L2 timestamp.
    Timestamp(u64),
}

impl UpgradeActivation {
    /// Converts an optional timestamp into an upgrade activation.
    pub const fn from_timestamp(timestamp: Option<u64>) -> Self {
        match timestamp {
            Some(timestamp) => Self::Timestamp(timestamp),
            None => Self::Never,
        }
    }

    /// Returns the activation timestamp, if the upgrade is timestamp-activated.
    pub const fn timestamp(self) -> Option<u64> {
        match self {
            Self::Never => None,
            Self::Timestamp(timestamp) => Some(timestamp),
        }
    }
}

/// A target that can receive contract-backed upgrade activation updates.
///
/// Implemented by every schedule destination (rollup config, execution chain spec, runtime
/// registry) so a single applier can drive them all without per-target apply loops.
pub trait UpgradeActivationSink {
    /// Error returned when an activation cannot be applied to this target.
    type Error;

    /// Applies `activation` for the canonical contract upgrade.
    ///
    /// Returns `true` when the upgrade is supported by this target, `false` when it is unknown
    /// and was ignored.
    fn apply_activation(
        &mut self,
        upgrade_id: BaseUpgrade,
        activation: UpgradeActivation,
    ) -> Result<bool, Self::Error>;

    /// Finalizes the target after a batch of activations (e.g. recompute derived state).
    fn finalize(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Runtime upgrade activation overrides for one chain.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct UpgradeActivationOverrides {
    /// Upgrade activations keyed by canonical contract upgrade ID.
    pub activations: BTreeMap<BaseUpgrade, UpgradeActivation>,
}

impl UpgradeActivationOverrides {
    /// Creates empty runtime upgrade activation overrides.
    pub const fn new() -> Self {
        Self { activations: BTreeMap::new() }
    }

    /// Returns true if no runtime overrides are configured.
    pub fn is_empty(&self) -> bool {
        self.activations.is_empty()
    }

    /// Returns the runtime activation override for a contract upgrade ID.
    pub fn activation(&self, upgrade_id: BaseUpgrade) -> Option<UpgradeActivation> {
        self.activations.get(&upgrade_id).copied()
    }

    /// Removes the runtime activation override for a contract upgrade ID.
    pub fn remove_activation(&mut self, upgrade_id: BaseUpgrade) -> bool {
        self.activations.remove(&upgrade_id).is_some()
    }

    /// Sets the runtime activation override for a contract upgrade ID.
    pub fn set_activation(&mut self, upgrade_id: BaseUpgrade, activation: UpgradeActivation) {
        self.activations.insert(upgrade_id, activation);
    }

    /// Sets a runtime timestamp activation override for a contract upgrade ID.
    pub fn set_activation_timestamp(&mut self, upgrade_id: BaseUpgrade, timestamp: u64) {
        self.set_activation(upgrade_id, UpgradeActivation::Timestamp(timestamp))
    }

    /// Sets a runtime override that clears an upgrade activation.
    pub fn clear_activation_timestamp(&mut self, upgrade_id: BaseUpgrade) {
        self.set_activation(upgrade_id, UpgradeActivation::Never)
    }
}

/// Process-local runtime upgrade activation registry.
///
/// The runtime upgrade signal treats the L1 contract as the authoritative source for these
/// overrides, so schedule application may replace the entire override set for a chain rather than
/// merging with previously stored entries.
///
/// Internally this registry uses `spin::RwLock`, so access is routed through the helper methods on
/// [`RuntimeUpgradeRegistry`] rather than exposing the raw lock to callers.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeUpgradeRegistry;

impl RuntimeUpgradeRegistry {
    /// Returns the global runtime upgrade activation registry.
    fn registry() -> &'static RwLock<BTreeMap<u64, UpgradeActivationOverrides>> {
        static REGISTRY: Once<RwLock<BTreeMap<u64, UpgradeActivationOverrides>>> = Once::new();
        REGISTRY.call_once(|| RwLock::new(BTreeMap::new()))
    }

    /// Returns a registry read guard.
    fn read_registry() -> RwLockReadGuard<'static, BTreeMap<u64, UpgradeActivationOverrides>> {
        Self::registry().read()
    }

    /// Returns a registry write guard.
    fn write_registry() -> RwLockWriteGuard<'static, BTreeMap<u64, UpgradeActivationOverrides>> {
        Self::registry().write()
    }

    /// Returns the runtime activation override for a chain and contract upgrade ID.
    pub fn activation(chain_id: u64, upgrade_id: BaseUpgrade) -> Option<UpgradeActivation> {
        Self::read_registry().get(&chain_id).and_then(|overrides| overrides.activation(upgrade_id))
    }

    /// Returns all runtime activation overrides for a chain.
    pub fn overrides(chain_id: u64) -> Option<UpgradeActivationOverrides> {
        Self::read_registry().get(&chain_id).cloned()
    }

    /// Replaces all runtime activation overrides for a chain.
    pub fn replace_overrides(chain_id: u64, overrides: UpgradeActivationOverrides) {
        Self::write_registry().insert(chain_id, overrides);
    }

    /// Clears all runtime activation overrides for a chain.
    pub fn clear_chain(chain_id: u64) {
        Self::write_registry().remove(&chain_id);
    }

    /// Removes one runtime activation override for a chain and contract upgrade ID.
    pub fn remove_activation_override(chain_id: u64, upgrade_id: BaseUpgrade) -> bool {
        let mut registry = Self::write_registry();
        let Some(overrides) = registry.get_mut(&chain_id) else {
            return false;
        };

        overrides.remove_activation(upgrade_id)
    }

    /// Sets one runtime activation override for a chain and contract upgrade ID.
    pub fn set_activation(chain_id: u64, upgrade_id: BaseUpgrade, activation: UpgradeActivation) {
        let mut registry = Self::write_registry();
        let overrides = registry.entry(chain_id).or_default();
        overrides.set_activation(upgrade_id, activation)
    }

    /// Sets one runtime timestamp activation override for a chain and contract upgrade ID.
    pub fn set_activation_timestamp(chain_id: u64, upgrade_id: BaseUpgrade, timestamp: u64) {
        Self::set_activation(chain_id, upgrade_id, UpgradeActivation::Timestamp(timestamp))
    }

    /// Sets one runtime override that clears a chain upgrade activation.
    pub fn clear_activation_timestamp(chain_id: u64, upgrade_id: BaseUpgrade) {
        Self::set_activation(chain_id, upgrade_id, UpgradeActivation::Never)
    }
}

/// Upgrade configuration.
///
/// See: <https://github.com/ethereum-optimism/superchain-registry/blob/8ff62ada16e14dd59d0fb94ffb47761c7fa96e01/ops/internal/config/chain.go#L102-L110>
#[derive(Debug, Copy, Clone, Default, Hash, Eq, PartialEq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct UpgradeConfig {
    /// `regolith_time` sets the activation time of the Regolith network-upgrade:
    /// a pre-mainnet Bedrock change that addresses findings of the Sherlock contest related to
    /// deposit attributes. "Regolith" is the loose deposited rock that sits on top of Bedrock.
    /// Active if `regolith_time` != None && L2 block timestamp >= `Some(regolith_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub regolith_time: Option<u64>,
    /// `canyon_time` sets the activation time of the Canyon network upgrade.
    /// Active if `canyon_time` != None && L2 block timestamp >= `Some(canyon_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub canyon_time: Option<u64>,
    /// `delta_time` sets the activation time of the Delta network upgrade.
    /// Active if `delta_time` != None && L2 block timestamp >= `Some(delta_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub delta_time: Option<u64>,
    /// `ecotone_time` sets the activation time of the Ecotone network upgrade.
    /// Active if `ecotone_time` != None && L2 block timestamp >= `Some(ecotone_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub ecotone_time: Option<u64>,
    /// `fjord_time` sets the activation time of the Fjord network upgrade.
    /// Active if `fjord_time` != None && L2 block timestamp >= `Some(fjord_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub fjord_time: Option<u64>,
    /// `granite_time` sets the activation time for the Granite network upgrade.
    /// Active if `granite_time` != None && L2 block timestamp >= `Some(granite_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub granite_time: Option<u64>,
    /// `holocene_time` sets the activation time for the Holocene network upgrade.
    /// Active if `holocene_time` != None && L2 block timestamp >= `Some(holocene_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub holocene_time: Option<u64>,
    /// `pectra_blob_schedule_time` sets the activation time for the activation of the Pectra blob
    /// fee schedule for the L1 block info transaction. This is an optional fork, only present
    /// on Base sepolia chains that observed the L1 Pectra network upgrade with the reference node
    /// <=v1.11.1 sequencing the network.
    ///
    /// Active if `pectra_blob_schedule_time` != None && L2 block timestamp >=
    /// `Some(pectra_blob_schedule_time)`, inactive otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub pectra_blob_schedule_time: Option<u64>,
    /// `isthmus_time` sets the activation time for the Isthmus network upgrade.
    /// Active if `isthmus_time` != None && L2 block timestamp >= `Some(isthmus_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub isthmus_time: Option<u64>,
    /// `jovian_time` sets the activation time for the Jovian network upgrade.
    /// Active if `jovian_time` != None && L2 block timestamp >= `Some(jovian_time)`, inactive
    /// otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub jovian_time: Option<u64>,
    /// `base` contains Base-specific upgrade activation times.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BaseUpgradeConfig::is_empty")
    )]
    pub base: BaseUpgradeConfig,
}

impl Display for UpgradeConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        #[inline(always)]
        fn fmt_time(t: Option<u64>) -> String {
            t.map(|t| t.to_string()).unwrap_or_else(|| "Not scheduled".to_string())
        }

        writeln!(f, "🍴 Scheduled Upgrades:")?;
        for (name, time) in self.iter() {
            writeln!(f, "-> {} Activation Time: {}", name, fmt_time(time))?;
        }
        Ok(())
    }
}

impl UpgradeConfig {
    /// Clears all timestamp-based upgrade activation times.
    pub fn clear_activation_timestamps(&mut self) {
        self.regolith_time = None;
        self.canyon_time = None;
        self.delta_time = None;
        self.ecotone_time = None;
        self.fjord_time = None;
        self.granite_time = None;
        self.holocene_time = None;
        self.pectra_blob_schedule_time = None;
        self.isthmus_time = None;
        self.jovian_time = None;
        self.base = BaseUpgradeConfig::default();
    }

    /// Clears a timestamp-based activation time by contract upgrade ID.
    pub const fn clear_activation_timestamp(&mut self, upgrade_id: BaseUpgrade) {
        match upgrade_id {
            BaseUpgrade::Bedrock => {}
            BaseUpgrade::Regolith => self.regolith_time = None,
            BaseUpgrade::Canyon => self.canyon_time = None,
            BaseUpgrade::Delta => self.delta_time = None,
            BaseUpgrade::Ecotone => self.ecotone_time = None,
            BaseUpgrade::Fjord => self.fjord_time = None,
            BaseUpgrade::Granite => self.granite_time = None,
            BaseUpgrade::Holocene => self.holocene_time = None,
            BaseUpgrade::PectraBlobSchedule => self.pectra_blob_schedule_time = None,
            BaseUpgrade::Isthmus => self.isthmus_time = None,
            BaseUpgrade::Jovian => self.jovian_time = None,
            BaseUpgrade::Azul => self.base.azul = None,
            BaseUpgrade::Beryl => self.base.beryl = None,
            BaseUpgrade::Cobalt => self.base.cobalt = None,
        }
    }

    /// Applies an upgrade activation override by contract upgrade ID.
    pub const fn set_activation(&mut self, upgrade_id: BaseUpgrade, activation: UpgradeActivation) {
        match activation {
            UpgradeActivation::Never => self.clear_activation_timestamp(upgrade_id),
            UpgradeActivation::Timestamp(timestamp) => {
                self.set_activation_timestamp(upgrade_id, timestamp)
            }
        }
    }

    /// Applies all upgrade activation overrides.
    pub fn apply_activation_overrides(&mut self, overrides: &UpgradeActivationOverrides) {
        for (upgrade_id, activation) in &overrides.activations {
            self.set_activation(*upgrade_id, *activation);
        }
    }

    /// Returns the activation for a timestamp-based contract upgrade ID.
    pub const fn activation(&self, upgrade_id: BaseUpgrade) -> UpgradeActivation {
        let timestamp = match upgrade_id {
            BaseUpgrade::Bedrock => None,
            BaseUpgrade::Regolith => self.regolith_time,
            BaseUpgrade::Canyon => self.canyon_time,
            BaseUpgrade::Delta => self.delta_time,
            BaseUpgrade::Ecotone => self.ecotone_time,
            BaseUpgrade::Fjord => self.fjord_time,
            BaseUpgrade::Granite => self.granite_time,
            BaseUpgrade::Holocene => self.holocene_time,
            BaseUpgrade::PectraBlobSchedule => self.pectra_blob_schedule_time,
            BaseUpgrade::Isthmus => self.isthmus_time,
            BaseUpgrade::Jovian => self.jovian_time,
            BaseUpgrade::Azul => self.base.azul,
            BaseUpgrade::Beryl => self.base.beryl,
            BaseUpgrade::Cobalt => self.base.cobalt,
        };

        UpgradeActivation::from_timestamp(timestamp)
    }

    /// Returns the activation timestamp for a timestamp-based contract upgrade ID.
    pub const fn activation_timestamp(&self, upgrade_id: BaseUpgrade) -> Option<u64> {
        self.activation(upgrade_id).timestamp()
    }

    /// Sets a timestamp-based activation time by contract upgrade ID.
    pub const fn set_activation_timestamp(&mut self, upgrade_id: BaseUpgrade, timestamp: u64) {
        match upgrade_id {
            BaseUpgrade::Bedrock => {}
            BaseUpgrade::Regolith => self.regolith_time = Some(timestamp),
            BaseUpgrade::Canyon => self.canyon_time = Some(timestamp),
            BaseUpgrade::Delta => self.delta_time = Some(timestamp),
            BaseUpgrade::Ecotone => self.ecotone_time = Some(timestamp),
            BaseUpgrade::Fjord => self.fjord_time = Some(timestamp),
            BaseUpgrade::Granite => self.granite_time = Some(timestamp),
            BaseUpgrade::Holocene => self.holocene_time = Some(timestamp),
            BaseUpgrade::PectraBlobSchedule => self.pectra_blob_schedule_time = Some(timestamp),
            BaseUpgrade::Isthmus => self.isthmus_time = Some(timestamp),
            BaseUpgrade::Jovian => self.jovian_time = Some(timestamp),
            BaseUpgrade::Azul => self.base.azul = Some(timestamp),
            BaseUpgrade::Beryl => self.base.beryl = Some(timestamp),
            BaseUpgrade::Cobalt => self.base.cobalt = Some(timestamp),
        }
    }

    /// Returns an iterator of upgrade names -> their activation times (if scheduled.)
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, Option<u64>)> {
        [
            ("Regolith", self.regolith_time),
            ("Canyon", self.canyon_time),
            ("Delta", self.delta_time),
            ("Ecotone", self.ecotone_time),
            ("Fjord", self.fjord_time),
            ("Granite", self.granite_time),
            ("Holocene", self.holocene_time),
            ("Pectra Blob Schedule", self.pectra_blob_schedule_time),
            ("Isthmus", self.isthmus_time),
            ("Jovian", self.jovian_time),
            ("Azul", self.base.azul),
            ("Beryl", self.base.beryl),
            ("Cobalt", self.base.cobalt),
        ]
        .into_iter()
    }
}

#[cfg(test)]
#[cfg(feature = "serde")]
mod tests {
    use super::*;

    #[test]
    fn test_upgrades_deserialize_json() {
        let raw: &str = r#"
        {
            "canyon_time": 1699981200,
            "delta_time": 1703203200,
            "ecotone_time": 1708534800,
            "fjord_time": 1716998400,
            "granite_time": 1723478400,
            "holocene_time":1732633200
        }
        "#;

        let upgrades = UpgradeConfig {
            regolith_time: None,
            canyon_time: Some(1699981200),
            delta_time: Some(1703203200),
            ecotone_time: Some(1708534800),
            fjord_time: Some(1716998400),
            granite_time: Some(1723478400),
            holocene_time: Some(1732633200),
            pectra_blob_schedule_time: None,
            isthmus_time: None,
            jovian_time: None,
            base: BaseUpgradeConfig::default(),
        };

        let deserialized: UpgradeConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(upgrades, deserialized);
    }

    #[test]
    fn test_upgrades_deserialize_new_field_fail_json() {
        let raw: &str = r#"
        {
            "canyon_time": 1704992401,
            "delta_time": 1708560000,
            "ecotone_time": 1710374401,
            "fjord_time": 1720627201,
            "granite_time": 1726070401,
            "holocene_time": 1736445601,
            "new_field": 0
        }
        "#;

        let err = serde_json::from_str::<UpgradeConfig>(raw).unwrap_err();
        assert_eq!(err.classify(), serde_json::error::Category::Data);
    }

    #[test]
    fn test_upgrades_deserialize_toml() {
        let raw: &str = r#"
        canyon_time =  1699981200 # Tue 14 Nov 2023 17:00:00 UTC
        delta_time =   1703203200 # Fri 22 Dec 2023 00:00:00 UTC
        ecotone_time = 1708534800 # Wed 21 Feb 2024 17:00:00 UTC
        fjord_time =   1716998400 # Wed 29 May 2024 16:00:00 UTC
        granite_time = 1723478400 # Mon Aug 12 16:00:00 UTC 2024
        holocene_time = 1732633200 # Tue Nov 26 15:00:00 UTC 2024
        "#;

        let upgrades = UpgradeConfig {
            regolith_time: None,
            canyon_time: Some(1699981200),
            delta_time: Some(1703203200),
            ecotone_time: Some(1708534800),
            fjord_time: Some(1716998400),
            granite_time: Some(1723478400),
            holocene_time: Some(1732633200),
            pectra_blob_schedule_time: None,
            isthmus_time: None,
            jovian_time: None,
            base: BaseUpgradeConfig::default(),
        };

        let deserialized: UpgradeConfig = toml::from_str(raw).unwrap();
        assert_eq!(upgrades, deserialized);
    }

    #[test]
    fn test_upgrades_deserialize_new_field_fail_toml() {
        let raw: &str = r#"
        canyon_time =  1699981200 # Tue 14 Nov 2023 17:00:00 UTC
        delta_time =   1703203200 # Fri 22 Dec 2023 00:00:00 UTC
        ecotone_time = 1708534800 # Wed 21 Feb 2024 17:00:00 UTC
        fjord_time =   1716998400 # Wed 29 May 2024 16:00:00 UTC
        granite_time = 1723478400 # Mon Aug 12 16:00:00 UTC 2024
        holocene_time = 1732633200 # Tue Nov 26 15:00:00 UTC 2024
        new_field_time = 1732633200 # Tue Nov 26 15:00:00 UTC 2024
        "#;
        toml::from_str::<UpgradeConfig>(raw).unwrap_err();
    }

    #[test]
    fn test_upgrades_iter() {
        let upgrades = UpgradeConfig {
            regolith_time: Some(1),
            canyon_time: Some(2),
            delta_time: Some(3),
            ecotone_time: Some(4),
            fjord_time: Some(5),
            granite_time: Some(6),
            holocene_time: Some(7),
            pectra_blob_schedule_time: Some(8),
            isthmus_time: Some(9),
            jovian_time: Some(10),
            base: BaseUpgradeConfig { azul: Some(11), beryl: Some(12), cobalt: Some(13) },
        };

        let mut iter = upgrades.iter();
        assert_eq!(iter.next(), Some(("Regolith", Some(1))));
        assert_eq!(iter.next(), Some(("Canyon", Some(2))));
        assert_eq!(iter.next(), Some(("Delta", Some(3))));
        assert_eq!(iter.next(), Some(("Ecotone", Some(4))));
        assert_eq!(iter.next(), Some(("Fjord", Some(5))));
        assert_eq!(iter.next(), Some(("Granite", Some(6))));
        assert_eq!(iter.next(), Some(("Holocene", Some(7))));
        assert_eq!(iter.next(), Some(("Pectra Blob Schedule", Some(8))));
        assert_eq!(iter.next(), Some(("Isthmus", Some(9))));
        assert_eq!(iter.next(), Some(("Jovian", Some(10))));
        assert_eq!(iter.next(), Some(("Azul", Some(11))));
        assert_eq!(iter.next(), Some(("Beryl", Some(12))));
        assert_eq!(iter.next(), Some(("Cobalt", Some(13))));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_set_activation_timestamp_by_upgrade_id() {
        let mut upgrades = UpgradeConfig::default();

        upgrades.set_activation_timestamp(BaseUpgrade::Regolith, 1);
        upgrades.set_activation_timestamp(BaseUpgrade::PectraBlobSchedule, 2);
        upgrades.set_activation_timestamp(BaseUpgrade::Azul, 3);
        upgrades.set_activation_timestamp(BaseUpgrade::Beryl, 4);
        upgrades.set_activation_timestamp(BaseUpgrade::Cobalt, 5);

        assert_eq!(upgrades.regolith_time, Some(1));
        assert_eq!(upgrades.pectra_blob_schedule_time, Some(2));
        assert_eq!(upgrades.base.azul, Some(3));
        assert_eq!(upgrades.base.beryl, Some(4));
        assert_eq!(upgrades.base.cobalt, Some(5));

        upgrades.clear_activation_timestamp(BaseUpgrade::Azul);
        assert_eq!(upgrades.base.azul, None);
        assert_eq!(upgrades.base.beryl, Some(4));
        assert_eq!(upgrades.base.cobalt, Some(5));

        upgrades.clear_activation_timestamps();

        assert_eq!(upgrades, UpgradeConfig::default());
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;

    #[test]
    fn runtime_registry_tracks_timestamp_and_never_overrides() {
        let chain_id = 9_100_001;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Azul, 42);
        RuntimeUpgradeRegistry::clear_activation_timestamp(chain_id, BaseUpgrade::Beryl);
        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Cobalt, 84);

        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(42))
        );
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Beryl),
            Some(UpgradeActivation::Never)
        );
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Cobalt),
            Some(UpgradeActivation::Timestamp(84))
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn upgrade_config_applies_activation_overrides() {
        let mut upgrades = UpgradeConfig { canyon_time: Some(10), ..Default::default() };
        let mut overrides = UpgradeActivationOverrides::new();

        overrides.clear_activation_timestamp(BaseUpgrade::Canyon);
        overrides.set_activation_timestamp(BaseUpgrade::Azul, 42);
        overrides.set_activation_timestamp(BaseUpgrade::Cobalt, 84);

        upgrades.apply_activation_overrides(&overrides);

        assert_eq!(upgrades.canyon_time, None);
        assert_eq!(upgrades.base.azul, Some(42));
        assert_eq!(upgrades.base.cobalt, Some(84));
    }
}
