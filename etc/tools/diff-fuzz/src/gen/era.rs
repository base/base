//! Base mainnet fork eras and the block number and timestamp schedule.

use std::sync::OnceLock;

use base_common_chains::ChainConfig;
use base_common_genesis::{BaseFeature, BaseUpgrade, RollupConfig};
use serde::{Deserialize, Serialize};

/// A Base mainnet fork era: the span of blocks from one execution-relevant upgrade activation to
/// the next. Delta changed only batch derivation, so its blocks belong to the Canyon era.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Era {
    /// Bedrock with Regolith, active from genesis.
    Regolith,
    /// Canyon (and Delta).
    Canyon,
    /// Ecotone.
    Ecotone,
    /// Fjord.
    Fjord,
    /// Granite.
    Granite,
    /// Holocene.
    Holocene,
    /// Isthmus.
    Isthmus,
    /// Jovian.
    Jovian,
    /// Azul network upgrade.
    Azul,
    /// Beryl network upgrade.
    Beryl,
    /// Cobalt network upgrade.
    Cobalt,
    /// Denim network upgrade (200 ms blocks).
    Denim,
    /// Everest network upgrade (Base transactions). Not scheduled on mainnet: it exists only
    /// when [`Era::EVEREST_TIMESTAMP_ENV`] gives it an activation time.
    Everest,
}

impl Era {
    /// Every era of mainnet's own schedule, oldest first.
    const SCHEDULED: [Self; 12] = [
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
    ];

    /// Environment variable giving Everest an activation timestamp (unix seconds, at or after
    /// Denim's) on top of the mainnet schedule. Every adapter must honor it the same way.
    pub const EVEREST_TIMESTAMP_ENV: &str = "BASE_DIFF_EVEREST_TIMESTAMP";

    /// Everest's activation timestamp from [`Self::EVEREST_TIMESTAMP_ENV`], if set.
    ///
    /// # Panics
    ///
    /// When the value is not a number or comes before Denim.
    pub fn everest_timestamp() -> Option<u64> {
        let value = std::env::var(Self::EVEREST_TIMESTAMP_ENV).ok()?;
        let timestamp: u64 = value
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("{} is not a number: {value}", Self::EVEREST_TIMESTAMP_ENV));
        let denim = ChainConfig::mainnet().denim_timestamp.expect("denim is scheduled on mainnet");
        assert!(timestamp >= denim, "{} is before Denim", Self::EVEREST_TIMESTAMP_ENV);
        Some(timestamp)
    }

    /// The Base mainnet chain config, with Everest from [`Self::EVEREST_TIMESTAMP_ENV`].
    pub fn chain_config() -> &'static ChainConfig {
        static CONFIG: OnceLock<ChainConfig> = OnceLock::new();
        CONFIG.get_or_init(|| {
            let mut config = ChainConfig::mainnet().clone();
            config.everest_timestamp = Self::everest_timestamp();
            config
        })
    }

    /// Every era, oldest first: mainnet's schedule, plus Everest when it has an activation time.
    pub fn all() -> &'static [Self] {
        static ALL: OnceLock<Vec<Era>> = OnceLock::new();
        ALL.get_or_init(|| {
            let mut all = Self::SCHEDULED.to_vec();
            if Self::everest_timestamp().is_some() {
                all.push(Self::Everest);
            }
            all
        })
    }

    /// Environment variable naming the newest era generators target, such as `azul`, for
    /// clients that do not implement later upgrades yet. Unset means every era.
    pub const MAX_ERA_ENV: &str = "BASE_DIFF_MAX_ERA";

    /// The eras generators target: [`Self::all`] up to [`Self::MAX_ERA_ENV`].
    ///
    /// # Panics
    ///
    /// When the variable names no era.
    pub fn targeted() -> Vec<Self> {
        let Ok(name) = std::env::var(Self::MAX_ERA_ENV) else { return Self::all().to_vec() };
        let last = Self::all()
            .iter()
            .copied()
            .find(|era| era.name() == name.trim())
            .unwrap_or_else(|| panic!("{} names no era: {name}", Self::MAX_ERA_ENV));
        Self::all().iter().copied().filter(|era| *era <= last).collect()
    }

    /// Mainnet activation timestamp of the era's upgrade.
    ///
    /// # Panics
    ///
    /// For Everest without an activation time; such an era is not in [`Self::all`].
    pub fn activation_timestamp(self) -> u64 {
        let c = Self::chain_config();
        match self {
            Self::Regolith => c.regolith_timestamp,
            Self::Canyon => c.canyon_timestamp,
            Self::Ecotone => c.ecotone_timestamp,
            Self::Fjord => c.fjord_timestamp,
            Self::Granite => c.granite_timestamp,
            Self::Holocene => c.holocene_timestamp,
            Self::Isthmus => c.isthmus_timestamp,
            Self::Jovian => c.jovian_timestamp,
            Self::Azul => c.azul_timestamp.expect("azul is scheduled on mainnet"),
            Self::Beryl => c.beryl_timestamp.expect("beryl is scheduled on mainnet"),
            Self::Cobalt => c.cobalt_timestamp.expect("cobalt is scheduled on mainnet"),
            Self::Denim => c.denim_timestamp.expect("denim is scheduled on mainnet"),
            Self::Everest => c.everest_timestamp.expect("everest has an activation time"),
        }
    }

    /// The upgrade whose rules apply in this era.
    pub const fn upgrade(self) -> BaseUpgrade {
        match self {
            Self::Regolith => BaseUpgrade::Regolith,
            Self::Canyon => BaseUpgrade::Canyon,
            Self::Ecotone => BaseUpgrade::Ecotone,
            Self::Fjord => BaseUpgrade::Fjord,
            Self::Granite => BaseUpgrade::Granite,
            Self::Holocene => BaseUpgrade::Holocene,
            Self::Isthmus => BaseUpgrade::Isthmus,
            Self::Jovian => BaseUpgrade::Jovian,
            Self::Azul => BaseUpgrade::Azul,
            Self::Beryl => BaseUpgrade::Beryl,
            Self::Cobalt => BaseUpgrade::Cobalt,
            Self::Denim => BaseUpgrade::Denim,
            Self::Everest => BaseUpgrade::Everest,
        }
    }

    /// The era in effect at a timestamp.
    pub fn at(timestamp: u64) -> Self {
        Self::all()
            .iter()
            .copied()
            .rev()
            .find(|era| era.activation_timestamp() <= timestamp)
            .unwrap_or(Self::Regolith)
    }

    /// The previous era, if any.
    pub fn previous(self) -> Option<Self> {
        let index = Self::all().iter().position(|era| *era == self)?;
        index.checked_sub(1).map(|i| Self::all()[i])
    }

    /// Lower-case era name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Regolith => "regolith",
            Self::Canyon => "canyon",
            Self::Ecotone => "ecotone",
            Self::Fjord => "fjord",
            Self::Granite => "granite",
            Self::Holocene => "holocene",
            Self::Isthmus => "isthmus",
            Self::Jovian => "jovian",
            Self::Azul => "azul",
            Self::Beryl => "beryl",
            Self::Cobalt => "cobalt",
            Self::Denim => "denim",
            Self::Everest => "everest",
        }
    }
}

/// The Base mainnet rollup schedule: block timestamps and feature activation.
#[derive(Debug)]
pub struct Schedule {
    /// Base mainnet rollup configuration.
    pub rollup: RollupConfig,
}

impl Schedule {
    /// The shared Base mainnet schedule.
    pub fn mainnet() -> &'static Self {
        static SCHEDULE: OnceLock<Schedule> = OnceLock::new();
        SCHEDULE.get_or_init(|| Self { rollup: Era::chain_config().rollup_config() })
    }

    /// The `(seconds, millis part)` timestamp of a block number.
    pub fn timestamp_parts(&self, number: u64) -> (u64, u16) {
        self.rollup.l2_block_timestamp_parts(number)
    }

    /// The first block whose timestamp is at or after `timestamp` (the activation block of an
    /// upgrade activating at `timestamp`).
    pub fn first_block_at(&self, timestamp: u64) -> u64 {
        let target = timestamp.saturating_mul(1_000);
        let (mut lo, mut hi) = (0u64, 1u64 << 40);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.rollup.l2_block_timestamp_millis(mid) >= target {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo
    }

    /// Whether a feature is active at a timestamp.
    pub fn active(&self, feature: BaseFeature, timestamp: u64) -> bool {
        self.rollup.is_feature_active_cascading(feature, timestamp)
    }

    /// The upgrade whose rules apply at `timestamp`.
    pub fn upgrade_at(&self, timestamp: u64) -> BaseUpgrade {
        Era::at(timestamp).upgrade()
    }
}

#[cfg(test)]
mod tests {
    use super::{Era, Schedule};

    #[test]
    fn activation_blocks_reach_their_timestamps() {
        let schedule = Schedule::mainnet();
        for era in Era::all().iter().copied().skip(1) {
            let block = schedule.first_block_at(era.activation_timestamp());
            assert!(schedule.timestamp_parts(block).0 >= era.activation_timestamp());
            assert!(schedule.timestamp_parts(block - 1).0 < era.activation_timestamp());
            assert_eq!(Era::at(schedule.timestamp_parts(block).0), era);
            assert_eq!(Era::at(schedule.timestamp_parts(block - 1).0), era.previous().unwrap());
        }
        // Denim blocks advance by 200 ms.
        let denim = schedule.first_block_at(Era::Denim.activation_timestamp());
        assert_eq!(schedule.timestamp_parts(denim + 1).1, 200);
    }
}
