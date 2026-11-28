//! Contains the upgrade configuration for the chain.

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
};
use core::fmt::Display;

#[cfg(feature = "hardforks")]
use alloy_hardforks::EthereumHardfork;

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
    /// `denim` sets the activation time for the Denim network upgrade.
    /// Active if `denim` != None && L2 block timestamp >= `Some(denim)`, inactive otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub denim: Option<u64>,
    /// `everest` sets the activation time for the Everest network upgrade.
    /// Active if `everest` != None && L2 block timestamp >= `Some(everest)`, inactive otherwise.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub everest: Option<u64>,
    /// `zenith` sets the activation time for the Zenith network upgrade.
    /// Active if `zenith` != None && L2 block timestamp >= `Some(zenith)`, inactive otherwise.
    #[cfg_attr(
        feature = "serde",
        serde(alias = "future", skip_serializing_if = "Option::is_none")
    )]
    pub zenith: Option<u64>,
}

impl BaseUpgradeConfig {
    /// Returns true if no Base-specific upgrades are configured.
    pub const fn is_empty(&self) -> bool {
        self.azul.is_none()
            && self.beryl.is_none()
            && self.cobalt.is_none()
            && self.denim.is_none()
            && self.everest.is_none()
            && self.zenith.is_none()
    }
}

/// The canonical Base network upgrade.
///
/// This single enum spans two domains:
/// - the **execution fork ladder** ([`BaseUpgrade::EXECUTION_VARIANTS`]) that maps onto the
///   reth/revm hardfork schedule, and
/// - the **contract-backed upgrade set** ([`BaseUpgrade::CONTRACT_VARIANTS`]) that is keyed by
///   the L1 upgrade-signal contract `hardforkId` strings and the genesis [`UpgradeConfig`]
///   timestamp fields.
///
/// Real network upgrades are listed in chronological order. [`Bedrock`](BaseUpgrade::Bedrock)
/// is execution-only (block-activated, not contract-backed), while
/// [`Delta`](BaseUpgrade::Delta) and [`PectraBlobSchedule`](BaseUpgrade::PectraBlobSchedule)
/// are contract-backed config upgrades that do not change EVM execution and therefore never
/// enter the execution fork ladder.
///
/// [`Denim`](BaseUpgrade::Denim) is the fourth Base-specific network upgrade. It is scheduled
/// on Base Sepolia and Base Mainnet. It is a first-class upgrade: contract-backed and part of
/// the execution fork ladder, so live chains can activate it once an activation time is
/// configured.
///
/// [`Everest`](BaseUpgrade::Everest) is the fifth Base-specific network upgrade. It is
/// unscheduled for now, contract-backed, and part of the execution fork ladder.
///
/// [`Zenith`](BaseUpgrade::Zenith) is a hardfork for future experimental features. It is
/// genesis-configurable but not contract-backed, since the L1 upgrade-signal contract does
/// not yet recognise it.
///
/// When building a list of upgrades for a chain, it's still expected to zip with Ethereum
/// hardforks. With the `hardforks` feature it is an `alloy_hardforks::Hardfork` and parses with
/// [`FromStr`](core::str::FromStr), as the `hardfork!` macro defines them.
#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum BaseUpgrade {
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
    Azul,
    /// Beryl: Second Base-specific network upgrade.
    #[default]
    Beryl,
    /// Cobalt: Third Base-specific network upgrade.
    Cobalt,
    /// Denim: Fourth Base-specific network upgrade.
    Denim,
    /// Everest: Fifth Base-specific network upgrade. Unscheduled for now.
    Everest,
    /// Zenith: hardfork for future experimental features.
    Zenith,
}

impl BaseUpgrade {
    /// Every variant, in declaration order.
    pub const VARIANTS: &'static [Self] = &[
        Self::Bedrock,
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
        Self::Denim,
        Self::Everest,
        Self::Zenith,
    ];

    /// Returns the variant's name (`PascalCase`), the reth/execution hardfork identity.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Bedrock => "Bedrock",
            Self::Regolith => "Regolith",
            Self::Canyon => "Canyon",
            Self::Delta => "Delta",
            Self::Ecotone => "Ecotone",
            Self::Fjord => "Fjord",
            Self::Granite => "Granite",
            Self::Holocene => "Holocene",
            Self::PectraBlobSchedule => "PectraBlobSchedule",
            Self::Isthmus => "Isthmus",
            Self::Jovian => "Jovian",
            Self::Azul => "Azul",
            Self::Beryl => "Beryl",
            Self::Cobalt => "Cobalt",
            Self::Denim => "Denim",
            Self::Everest => "Everest",
            Self::Zenith => "Zenith",
        }
    }

    /// Latest Base upgrade used by default.
    pub const LATEST: Self = Self::Beryl;

    /// The execution fork ladder, in activation order.
    ///
    /// These are the upgrades that participate in the reth/revm hardfork schedule. Excludes the
    /// contract-only [`Delta`](Self::Delta) and [`PectraBlobSchedule`](Self::PectraBlobSchedule)
    /// upgrades, and [`Zenith`](Self::Zenith), which does not yet change EVM execution.
    pub const EXECUTION_VARIANTS: [Self; 14] = [
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
        Self::Denim,
        Self::Everest,
    ];

    /// The contract-backed upgrade set, in activation order.
    ///
    /// These are the upgrades addressable by the L1 upgrade-signal contract and stored in the
    /// genesis [`UpgradeConfig`]. Excludes block-activated [`Bedrock`](Self::Bedrock) and
    /// [`Zenith`](Self::Zenith), which is activated via genesis config only until the L1
    /// upgrade-signal contract is updated to recognise it.
    ///
    /// Order is load-bearing: `map_schedule` and `ScheduleId::pin` attribute onchain
    /// schedule entries to hardforks *by position*, and the `ProtocolVersions` contract keys
    /// upgrades by ascending append-only registration id with names kept offchain. This order MUST
    /// match the contract's registration order — reordering silently misattributes every
    /// activation timestamp. Only ever append.
    pub const CONTRACT_VARIANTS: [Self; 15] = [
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
        Self::Denim,
        Self::Everest,
    ];

    /// Returns true if this upgrade participates in the execution fork ladder.
    pub const fn is_execution(self) -> bool {
        self.execution_idx().is_some()
    }

    /// Returns true if this upgrade is contract-backed (i.e. signaled by the L1 upgrade-signal
    /// contract and stored in [`UpgradeConfig`]). False for block-activated
    /// [`Bedrock`](Self::Bedrock) and for [`Zenith`](Self::Zenith), which is activated via
    /// genesis config only until the L1 upgrade-signal contract is updated.
    pub const fn is_contract_backed(self) -> bool {
        !matches!(self, Self::Bedrock | Self::Zenith)
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
            Self::Denim => 12,
            Self::Everest => 13,
            Self::Delta | Self::PectraBlobSchedule | Self::Zenith => return None,
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
            Self::Denim => "denim",
            Self::Everest => "everest",
            Self::Zenith => "zenith",
        }
    }

    /// Returns the Ethereum execution hardfork activated by this upgrade, if any.
    #[cfg(feature = "hardforks")]
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
    #[cfg(feature = "hardforks")]
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
            "denim" | "basedenim" => Self::Denim,
            "everest" | "baseeverest" => Self::Everest,
            // Zenith is not contract-backed: even though `contract_id` emits "zenith", it is
            // deliberately not resolvable here, so the L1 upgrade signal can never address it.
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

    /// Returns the active upgrade at the given timestamp for the specified chain.
    pub fn from_chain_and_timestamp(chain_id: u64, timestamp: u64) -> Option<Self> {
        let config = UpgradeConfig::for_chain_id(chain_id)?;

        #[cfg(feature = "runtime-registry")]
        let config = {
            let mut config = config;
            if let Some(overrides) = crate::RuntimeUpgradeRegistry::overrides(chain_id) {
                config.apply_activation_overrides(&overrides);
            }
            config
        };

        Self::EXECUTION_VARIANTS.into_iter().rev().find(|&upgrade| {
            upgrade == Self::Bedrock
                || config
                    .activation_timestamp(upgrade)
                    .is_some_and(|activation| timestamp >= activation)
        })
    }
}

impl Display for BaseUpgrade {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[cfg(feature = "hardforks")]
impl core::str::FromStr for BaseUpgrade {
    type Err = alloy_hardforks::ParseHardforkError;

    /// Parses a variant name, ignoring case, as `alloy_hardforks::hardfork!` does.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.to_lowercase();
        Self::VARIANTS
            .iter()
            .copied()
            .find(|upgrade| upgrade.name().to_lowercase() == lower)
            .ok_or_else(|| {
                alloy_hardforks::ParseHardforkError::new(alloc::format!("Unknown hardfork: {s}"))
            })
    }
}

#[cfg(feature = "hardforks")]
impl alloy_hardforks::Hardfork for BaseUpgrade {
    fn name(&self) -> &'static str {
        Self::name(self)
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
    ///
    /// Returns `true` when the target committed the batch and `false` when it rejected the batch
    /// without error.
    fn finalize(&mut self) -> Result<bool, Self::Error> {
        Ok(true)
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
        // Zenith is not contract-backed: it must never be set via the L1-signal-driven runtime
        // override path. Its activation comes from genesis config only.
        if matches!(upgrade_id, BaseUpgrade::Zenith) {
            return;
        }
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
    /// Base Mainnet upgrade schedule used by [`BaseUpgrade::from_chain_and_timestamp`].
    ///
    /// This schedule lives here because `base-common-chains` already depends on this crate;
    /// importing its `ChainConfig` would create a circular crate dependency.
    pub const BASE_MAINNET: Self = Self {
        regolith_time: Some(1_686_789_347),
        canyon_time: Some(1_704_992_401),
        delta_time: Some(1_708_560_000),
        ecotone_time: Some(1_710_374_401),
        fjord_time: Some(1_720_627_201),
        granite_time: Some(1_726_070_401),
        holocene_time: Some(1_736_445_601),
        pectra_blob_schedule_time: None,
        isthmus_time: Some(1_746_806_401),
        jovian_time: Some(1_764_691_201),
        base: BaseUpgradeConfig {
            azul: Some(1_779_991_200),
            beryl: Some(1_782_410_400),
            cobalt: Some(1_790_791_200),
            denim: Some(1_793_901_600),
            everest: None,
            zenith: None,
        },
    };

    /// Base Sepolia upgrade schedule used by [`BaseUpgrade::from_chain_and_timestamp`].
    ///
    /// This schedule lives here because `base-common-chains` already depends on this crate;
    /// importing its `ChainConfig` would create a circular crate dependency.
    pub const BASE_SEPOLIA: Self = Self {
        regolith_time: Some(1_695_768_288),
        canyon_time: Some(1_699_981_200),
        delta_time: Some(1_703_203_200),
        ecotone_time: Some(1_708_534_800),
        fjord_time: Some(1_716_998_400),
        granite_time: Some(1_723_478_400),
        holocene_time: Some(1_732_633_200),
        pectra_blob_schedule_time: Some(1_742_486_400),
        isthmus_time: Some(1_744_905_600),
        jovian_time: Some(1_763_568_001),
        base: BaseUpgradeConfig {
            azul: Some(1_776_708_000),
            beryl: Some(1_781_805_600),
            cobalt: Some(1_790_186_400),
            denim: Some(1_792_605_600),
            everest: None,
            zenith: None,
        },
    };

    /// Returns the upgrade schedule for a chain ID.
    ///
    /// This function is used by [`BaseUpgrade::from_chain_and_timestamp`] to determine the
    /// upgrade schedule for a given chain ID.
    pub const fn for_chain_id(chain_id: u64) -> Option<Self> {
        match chain_id {
            8453 => Some(Self::BASE_MAINNET),
            84532 => Some(Self::BASE_SEPOLIA),
            _ => None,
        }
    }

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
            // Bedrock is block-activated; it has no timestamp slot.
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
            BaseUpgrade::Denim => self.base.denim = None,
            BaseUpgrade::Everest => self.base.everest = None,
            BaseUpgrade::Zenith => self.base.zenith = None,
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
            // Bedrock is block-activated; it has no timestamp slot.
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
            BaseUpgrade::Denim => self.base.denim,
            BaseUpgrade::Everest => self.base.everest,
            BaseUpgrade::Zenith => self.base.zenith,
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
            // Bedrock is block-activated; setting a timestamp for it is a no-op.
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
            BaseUpgrade::Denim => self.base.denim = Some(timestamp),
            BaseUpgrade::Everest => self.base.everest = Some(timestamp),
            BaseUpgrade::Zenith => self.base.zenith = Some(timestamp),
        }
    }

    /// Returns an iterator of contract-backed upgrades -> their activation times (if scheduled),
    /// in [`BaseUpgrade::CONTRACT_VARIANTS`] order.
    ///
    /// Derived from `CONTRACT_VARIANTS` (the single source of ordering) via the exhaustive
    /// [`activation_timestamp`](Self::activation_timestamp) match, so this can never drift from the
    /// registration order the schedule id and metrics rely on.
    pub fn iter(&self) -> impl Iterator<Item = (BaseUpgrade, Option<u64>)> + '_ {
        BaseUpgrade::CONTRACT_VARIANTS
            .iter()
            .map(move |&upgrade| (upgrade, self.activation_timestamp(upgrade)))
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
            base: BaseUpgradeConfig {
                azul: Some(11),
                beryl: Some(12),
                cobalt: Some(13),
                denim: Some(14),
                everest: Some(15),
                zenith: None,
            },
        };

        // iter() yields entries in CONTRACT_VARIANTS order with each variant's activation time.
        let expected: Vec<(BaseUpgrade, Option<u64>)> = BaseUpgrade::CONTRACT_VARIANTS
            .iter()
            .enumerate()
            .map(|(index, &upgrade)| (upgrade, Some(index as u64 + 1)))
            .collect();

        assert_eq!(upgrades.iter().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn test_set_activation_timestamp_by_upgrade_id() {
        let mut upgrades = UpgradeConfig::default();

        upgrades.set_activation_timestamp(BaseUpgrade::Regolith, 1);
        upgrades.set_activation_timestamp(BaseUpgrade::PectraBlobSchedule, 2);
        upgrades.set_activation_timestamp(BaseUpgrade::Azul, 3);
        upgrades.set_activation_timestamp(BaseUpgrade::Beryl, 5);
        upgrades.set_activation_timestamp(BaseUpgrade::Cobalt, 6);
        upgrades.set_activation_timestamp(BaseUpgrade::Denim, 8);
        upgrades.set_activation_timestamp(BaseUpgrade::Zenith, 7);

        assert_eq!(upgrades.regolith_time, Some(1));
        assert_eq!(upgrades.pectra_blob_schedule_time, Some(2));
        assert_eq!(upgrades.base.azul, Some(3));
        assert_eq!(upgrades.base.beryl, Some(5));
        assert_eq!(upgrades.base.cobalt, Some(6));
        assert_eq!(upgrades.base.denim, Some(8));
        assert_eq!(upgrades.activation(BaseUpgrade::Zenith), UpgradeActivation::Timestamp(7));

        upgrades.clear_activation_timestamp(BaseUpgrade::Azul);
        assert_eq!(upgrades.base.azul, None);
        assert_eq!(upgrades.base.beryl, Some(5));
        assert_eq!(upgrades.base.cobalt, Some(6));
        assert_eq!(upgrades.base.denim, Some(8));
        assert_eq!(upgrades.activation(BaseUpgrade::Zenith), UpgradeActivation::Timestamp(7));

        upgrades.clear_activation_timestamps();

        assert_eq!(upgrades, UpgradeConfig::default());
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;

    #[test]
    fn variants_are_complete_and_named_as_declared() {
        for (index, &upgrade) in BaseUpgrade::VARIANTS.iter().enumerate() {
            assert_eq!(upgrade as usize, index, "{upgrade:?} out of declaration order");
            assert_eq!(upgrade.name(), alloc::format!("{upgrade:?}"));
            assert_eq!(upgrade.to_string(), upgrade.name());
            #[cfg(feature = "hardforks")]
            assert_eq!(upgrade.name().to_uppercase().parse::<BaseUpgrade>().ok(), Some(upgrade));
        }
        assert_eq!(BaseUpgrade::VARIANTS.last(), Some(&BaseUpgrade::Zenith));
        #[cfg(feature = "hardforks")]
        assert!("Zenith2".parse::<BaseUpgrade>().is_err());
    }

    #[test]
    fn upgrade_config_applies_activation_overrides() {
        let mut upgrades = UpgradeConfig { canyon_time: Some(10), ..Default::default() };
        let mut overrides = UpgradeActivationOverrides::new();

        overrides.clear_activation_timestamp(BaseUpgrade::Canyon);
        overrides.set_activation_timestamp(BaseUpgrade::Azul, 42);
        overrides.set_activation_timestamp(BaseUpgrade::Cobalt, 84);
        // Even an explicit override cannot activate Zenith, since it is not contract-backed.
        // The write side drops it entirely, so it is never even stored as an override.
        overrides.set_activation_timestamp(BaseUpgrade::Zenith, u64::MAX);
        assert_eq!(overrides.activation(BaseUpgrade::Zenith), None);

        upgrades.apply_activation_overrides(&overrides);

        assert_eq!(upgrades.canyon_time, None);
        assert_eq!(upgrades.base.azul, Some(42));
        assert_eq!(upgrades.base.cobalt, Some(84));
        assert_eq!(upgrades.activation(BaseUpgrade::Zenith), UpgradeActivation::Never);
    }

    #[test]
    fn unknown_chains_do_not_resolve() {
        for chain_id in [1, 10, 9_999_999] {
            assert_eq!(BaseUpgrade::from_chain_and_timestamp(chain_id, u64::MAX), None);
        }
    }
}
