//! L1 system config updates: well-formed and malformed `ConfigUpdate` logs, the
//! `systemConfig` requests that compare how clients apply them, and the updates synthetic chains
//! derive their system config changes from.

use alloy_primitives::{Address, B256, U256, keccak256};
use base_common_genesis::{SystemConfig, SystemConfigUpdate};

use crate::{
    L1Receipt, Log, Mode, Request, SystemConfigInput, SystemConfigValues,
    r#gen::{
        block::SysConfig,
        era::{Era, Schedule},
        rng::Rng,
    },
};

/// One system config update as the L1 `SystemConfig` contract emits it, in
/// `ConfigUpdate(uint256 indexed version, uint8 indexed updateType, bytes data)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigUpdate {
    /// Kind 0: batcher address.
    Batcher(Address),
    /// Kind 1: fee overhead and scalar.
    GasConfig {
        /// Overhead word.
        overhead: B256,
        /// Scalar word.
        scalar: B256,
    },
    /// Kind 2: gas limit.
    GasLimit(u64),
    /// Kind 3: unsafe block signer, which derivation ignores.
    UnsafeBlockSigner(Address),
    /// Kind 4: EIP-1559 denominator and elasticity.
    Eip1559 {
        /// Denominator.
        denominator: u32,
        /// Elasticity.
        elasticity: u32,
    },
    /// Kind 5: operator fee scalar and constant.
    OperatorFee {
        /// Scalar.
        scalar: u32,
        /// Constant.
        constant: u64,
    },
    /// Kind 6: minimum base fee.
    MinBaseFee(u64),
    /// Kind 7: DA footprint gas scalar.
    DaFootprintGasScalar(u16),
}

impl ConfigUpdate {
    /// Number of update kinds the protocol defines.
    pub const KINDS: u64 = 8;

    /// The update kind (the low byte of the update type topic).
    pub const fn kind(&self) -> u64 {
        match self {
            Self::Batcher(_) => 0,
            Self::GasConfig { .. } => 1,
            Self::GasLimit(_) => 2,
            Self::UnsafeBlockSigner(_) => 3,
            Self::Eip1559 { .. } => 4,
            Self::OperatorFee { .. } => 5,
            Self::MinBaseFee(_) => 6,
            Self::DaFootprintGasScalar(_) => 7,
        }
    }

    /// The ABI words of the `bytes data` payload, after its pointer and length.
    pub fn payload(&self) -> Vec<[u8; 32]> {
        let uint = |value: u64| U256::from(value).to_be_bytes::<32>();
        match *self {
            Self::Batcher(address) | Self::UnsafeBlockSigner(address) => {
                vec![address.into_word().0]
            }
            Self::GasConfig { overhead, scalar } => vec![overhead.0, scalar.0],
            Self::GasLimit(value) | Self::MinBaseFee(value) => vec![uint(value)],
            Self::Eip1559 { denominator, elasticity } => {
                vec![uint((u64::from(denominator) << 32) | u64::from(elasticity))]
            }
            Self::OperatorFee { scalar, constant } => {
                let mut word = [0u8; 32];
                word[20..24].copy_from_slice(&scalar.to_be_bytes());
                word[24..32].copy_from_slice(&constant.to_be_bytes());
                vec![word]
            }
            Self::DaFootprintGasScalar(value) => vec![uint(u64::from(value))],
        }
    }

    /// The well-formed log of this update.
    pub fn log(&self) -> Log {
        let payload = self.payload();
        let mut data = Vec::with_capacity(64 + 32 * payload.len());
        data.extend_from_slice(&U256::from(32).to_be_bytes::<32>());
        data.extend_from_slice(&U256::from(32 * payload.len()).to_be_bytes::<32>());
        for word in &payload {
            data.extend_from_slice(word);
        }
        Log {
            address: ConfigLogs::address(),
            topics: vec![
                SystemConfigUpdate::TOPIC,
                SystemConfigUpdate::EVENT_VERSION_0,
                ConfigLogs::kind_topic(self.kind()),
            ],
            data: data.into(),
        }
    }

    /// A random update of `kind` (`0..KINDS`), biased towards edge values.
    pub fn random(rng: &mut Rng, kind: u64) -> Self {
        match kind {
            0 => Self::Batcher(ConfigLogs::address_value(rng)),
            1 => Self::GasConfig {
                overhead: B256::from(if rng.chance(25) {
                    rng.edge_u256()
                } else {
                    U256::from(ConfigLogs::edge(rng, &[0, 188, 2100]))
                }),
                scalar: ConfigLogs::scalar(rng),
            },
            2 => Self::GasLimit(ConfigLogs::edge(
                rng,
                &[0, 1, 5_000_000, 30_000_000, 400_000_000, u64::from(u32::MAX), u64::MAX],
            )),
            3 => Self::UnsafeBlockSigner(ConfigLogs::address_value(rng)),
            4 => {
                let (denominator, elasticity) = if rng.chance(15) {
                    (rng.next_u64() as u32, rng.next_u64() as u32)
                } else {
                    *rng.pick(&[
                        (0, 0),
                        (0, 6),
                        (250, 0),
                        (1, 1),
                        (50, 6),
                        (250, 6),
                        (u32::MAX, u32::MAX),
                    ])
                };
                Self::Eip1559 { denominator, elasticity }
            }
            5 => Self::OperatorFee {
                scalar: ConfigLogs::edge(rng, &[0, 1, 1_000, u64::from(u32::MAX)]) as u32,
                constant: ConfigLogs::edge(rng, &[0, 1, 1_000_000_000, u64::MAX]),
            },
            6 => Self::MinBaseFee(ConfigLogs::edge(rng, &[0, 1, 1_000_000, 1 << 40, u64::MAX])),
            _ => Self::DaFootprintGasScalar(
                ConfigLogs::edge(rng, &[0, 1, 400, u64::from(u16::MAX)]) as u16,
            ),
        }
    }

    /// A random update of a random kind.
    pub fn any(rng: &mut Rng) -> Self {
        let kind = rng.below(Self::KINDS);
        Self::random(rng, kind)
    }

    /// The updates that turn `from` into `to`: one per kind whose values differ.
    pub fn towards(from: &SystemConfig, to: &SystemConfig) -> Vec<Self> {
        let mut updates = Vec::new();
        if from.batcher_address != to.batcher_address {
            updates.push(Self::Batcher(to.batcher_address));
        }
        if (from.overhead, from.scalar) != (to.overhead, to.scalar) {
            updates
                .push(Self::GasConfig { overhead: to.overhead.into(), scalar: to.scalar.into() });
        }
        if from.gas_limit != to.gas_limit {
            updates.push(Self::GasLimit(to.gas_limit));
        }
        if (from.eip1559_denominator, from.eip1559_elasticity)
            != (to.eip1559_denominator, to.eip1559_elasticity)
        {
            updates.push(Self::Eip1559 {
                denominator: to.eip1559_denominator.unwrap_or_default(),
                elasticity: to.eip1559_elasticity.unwrap_or_default(),
            });
        }
        if (from.operator_fee_scalar, from.operator_fee_constant)
            != (to.operator_fee_scalar, to.operator_fee_constant)
        {
            updates.push(Self::OperatorFee {
                scalar: to.operator_fee_scalar.unwrap_or_default(),
                constant: to.operator_fee_constant.unwrap_or_default(),
            });
        }
        if from.min_base_fee != to.min_base_fee {
            updates.push(Self::MinBaseFee(to.min_base_fee.unwrap_or_default()));
        }
        if from.da_footprint_gas_scalar != to.da_footprint_gas_scalar {
            updates
                .push(Self::DaFootprintGasScalar(to.da_footprint_gas_scalar.unwrap_or_default()));
        }
        updates
    }

    /// An update that changes a single parameter of `from` to its value in `to` (one half of a
    /// two-value kind), or sets the DA footprint gas scalar to zero (its default).
    pub fn single(rng: &mut Rng, from: &SystemConfig, to: &SystemConfig) -> Self {
        let half = rng.chance(50);
        match rng.below(9) {
            0 => Self::Batcher(to.batcher_address),
            1 => Self::GasConfig {
                overhead: if half { to.overhead } else { from.overhead }.into(),
                scalar: if half { from.scalar } else { to.scalar }.into(),
            },
            2 | 3 => Self::GasLimit(to.gas_limit),
            4 => Self::Eip1559 {
                denominator: if half { to.eip1559_denominator } else { from.eip1559_denominator }
                    .unwrap_or_default(),
                elasticity: if half { from.eip1559_elasticity } else { to.eip1559_elasticity }
                    .unwrap_or_default(),
            },
            5 => Self::OperatorFee {
                scalar: if half { to.operator_fee_scalar } else { from.operator_fee_scalar }
                    .unwrap_or_default(),
                constant: if half { from.operator_fee_constant } else { to.operator_fee_constant }
                    .unwrap_or_default(),
            },
            6 => Self::MinBaseFee(to.min_base_fee.unwrap_or_default()),
            7 => Self::DaFootprintGasScalar(if half {
                0
            } else {
                to.da_footprint_gas_scalar.unwrap_or_default()
            }),
            _ => Self::UnsafeBlockSigner(to.batcher_address),
        }
    }
}

/// Builders of `ConfigUpdate` logs and `systemConfig` requests.
#[derive(Debug)]
pub struct ConfigLogs;

impl ConfigLogs {
    /// Base mainnet's first L2 block timestamp, a lower bound for generated L1 timestamps.
    pub const BEDROCK_TIMESTAMP: u64 = 1_686_789_347;

    /// The Base mainnet L1 system config contract, the only emitter derivation reads.
    pub fn address() -> Address {
        Schedule::mainnet().rollup.l1_system_config_address
    }

    /// The update type topic of a kind.
    pub fn kind_topic(kind: u64) -> B256 {
        B256::from(U256::from(kind))
    }

    /// One of `edges`, or now and then a uniformly random value.
    pub fn edge(rng: &mut Rng, edges: &[u64]) -> u64 {
        if rng.below(edges.len() as u64 + 1) == 0 { rng.next_u64() } else { *rng.pick(edges) }
    }

    fn address_value(rng: &mut Rng) -> Address {
        match rng.below(4) {
            0 => Address::ZERO,
            1 => Address::repeat_byte(0xff),
            _ => rng.address(),
        }
    }

    /// A gas config scalar: valid version 0 and 1 encodings, and invalid ones that an Ecotone
    /// update must ignore.
    fn scalar(rng: &mut Rng) -> B256 {
        let mut word = [0u8; 32];
        match rng.below(8) {
            // Version 0: only the low four bytes may be set.
            0 => word[28..].copy_from_slice(&(rng.next_u64() as u32).to_be_bytes()),
            // Version 1: blob base fee scalar in 24..28, base fee scalar in 28..32.
            1..=3 => {
                word[0] = 1;
                word[24..28].copy_from_slice(&rng.pick(&[0u32, 810_949, u32::MAX]).to_be_bytes());
                word[28..].copy_from_slice(&rng.pick(&[0u32, 1368, u32::MAX]).to_be_bytes());
            }
            // Version 0 with a dirty padding byte.
            4 => {
                word[28..].copy_from_slice(&1368u32.to_be_bytes());
                word[rng.range(1, 27) as usize] = 1;
            }
            // Version 1 with a dirty padding byte.
            5 => {
                word[0] = 1;
                word[31] = 1;
                word[rng.range(1, 23) as usize] = 1;
            }
            // Unknown version.
            6 => {
                word[0] = *rng.pick(&[2u8, 0xff]);
                word[31] = 1;
            }
            _ => return rng.b256(),
        }
        B256::from(word)
    }

    /// A log of `update` broken in one way: data pointer, data length, data size, value padding,
    /// version, update type, or topic count.
    pub fn malformed(rng: &mut Rng, update: &ConfigUpdate) -> Log {
        let mut log = update.log();
        let mut data = log.data.to_vec();
        let payload_len = 32 * update.payload().len() as u64;
        let bad_word = |rng: &mut Rng, good: u64| -> [u8; 32] {
            let value = Self::edge(rng, &[0, 31, 33, 64, 96, u64::MAX]);
            let mut word = U256::from(if value == good { 0 } else { value }).to_be_bytes::<32>();
            if rng.chance(25) {
                // A word with the right low bytes but a nonzero high byte.
                word = U256::from(good).to_be_bytes::<32>();
                word[rng.below(24) as usize] = 1;
            }
            word
        };
        match rng.below(10) {
            0 => data[..32].copy_from_slice(&bad_word(rng, 32)),
            1 => data[32..64].copy_from_slice(&bad_word(rng, payload_len)),
            2 => {
                let cut = (*rng.pick(&[1usize, 31, 32, 64])).min(data.len());
                data.truncate(if rng.chance(15) { 0 } else { data.len() - cut });
            }
            3 => {
                let extra = *rng.pick(&[1usize, 31, 32, 64]);
                let bytes = if rng.chance(50) { vec![0; extra] } else { rng.bytes(extra) };
                data.extend_from_slice(&bytes);
            }
            4 => {
                // A nonzero byte in the padding of the value word. Operator fee words keep
                // bytes 0..20 unread, and gas config overhead words have no padding.
                let padding = match update {
                    ConfigUpdate::Batcher(_) | ConfigUpdate::UnsafeBlockSigner(_) => 12,
                    ConfigUpdate::GasLimit(_)
                    | ConfigUpdate::Eip1559 { .. }
                    | ConfigUpdate::MinBaseFee(_) => 24,
                    ConfigUpdate::OperatorFee { .. } => 20,
                    ConfigUpdate::DaFootprintGasScalar(_) => 30,
                    ConfigUpdate::GasConfig { .. } => 32,
                };
                data[64 + rng.below(padding) as usize] = rng.range(1, 255) as u8;
            }
            5 => {
                log.topics[1] = match rng.below(3) {
                    0 => B256::with_last_byte(1),
                    1 => B256::from(U256::from(1) << 255),
                    _ => rng.b256(),
                };
            }
            6 => {
                log.topics[2] = if rng.chance(20) {
                    B256::repeat_byte(0xff)
                } else {
                    Self::kind_topic(*rng.pick(&Self::KIND_UNKNOWN))
                };
            }
            7 => {
                // The kind in the low byte with a nonzero byte above the low eight bytes.
                log.topics[2].0[rng.below(24) as usize] = rng.range(1, 255) as u8;
            }
            8 => log.topics.truncate(rng.range(1, 2) as usize),
            _ => {
                for _ in 0..rng.range(1, 2) {
                    log.topics.push(rng.b256());
                }
            }
        }
        log.data = data.into();
        log
    }

    /// Update types that are not kinds.
    const KIND_UNKNOWN: [u64; 5] = [8, 9, 255, 256, u64::MAX];

    /// A log derivation must not treat as a config update: another emitter, another event, or
    /// no topics.
    pub fn unrelated(rng: &mut Rng, update: &ConfigUpdate) -> Log {
        let mut log = update.log();
        match rng.below(3) {
            0 => {
                log.address = if rng.chance(30) { Address::ZERO } else { rng.address() };
            }
            1 => {
                log.topics[0] = if rng.chance(50) {
                    keccak256("TransactionDeposited(address,address,uint256,bytes)")
                } else {
                    rng.b256()
                };
            }
            _ => log.topics.clear(),
        }
        log
    }

    /// A random log: mostly well-formed updates, then malformed ones, then unrelated ones.
    pub fn random_log(rng: &mut Rng) -> Log {
        let update = ConfigUpdate::any(rng);
        match rng.weighted(&[50, 35, 15]) {
            0 => update.log(),
            1 => Self::malformed(rng, &update),
            _ => Self::unrelated(rng, &update),
        }
    }

    /// An L1 timestamp, often within a few blocks of an upgrade activation.
    fn l1_timestamp(rng: &mut Rng) -> u64 {
        let near = |rng: &mut Rng, era: Era| era.activation_timestamp() - 36 + rng.below(73);
        match rng.below(10) {
            0..=3 => near(rng, Era::Ecotone),
            4..=6 => {
                let era = *rng.pick(&Era::ALL[1..]);
                near(rng, era)
            }
            _ => {
                rng.range(Self::BEDROCK_TIMESTAMP, Era::Denim.activation_timestamp() + 365 * 86_400)
            }
        }
    }

    /// The upgrades active at a timestamp, oldest first.
    pub fn forks(timestamp: u64) -> Vec<String> {
        Era::ALL
            .into_iter()
            .filter(|era| era.activation_timestamp() <= timestamp)
            .map(|era| era.name().to_owned())
            .collect()
    }

    /// A starting config: a plausible chain config with edge values now and then.
    fn config(rng: &mut Rng, timestamp: u64) -> SystemConfigValues {
        let mut config = SystemConfigValues::from(&SysConfig::random(rng, timestamp).config);
        let edge = |rng: &mut Rng| Self::edge(rng, &[0, 1, u64::from(u32::MAX), u64::MAX]);
        if rng.chance(50) {
            config.batcher_address = rng.address();
        }
        if rng.chance(25) {
            config.overhead = B256::from(rng.edge_u256());
        }
        if rng.chance(25) {
            config.scalar = Self::scalar(rng);
        }
        if rng.chance(15) {
            config.gas_limit = edge(rng);
        }
        if rng.chance(15) {
            config.eip1559_denominator = edge(rng) as u32;
            config.eip1559_elasticity = edge(rng) as u32;
        }
        if rng.chance(15) {
            config.operator_fee_scalar = edge(rng) as u32;
            config.operator_fee_constant = edge(rng);
        }
        if rng.chance(15) {
            config.min_base_fee = edge(rng);
        }
        if rng.chance(15) {
            config.da_footprint_gas_scalar = edge(rng) as u16;
        }
        config
    }

    /// The `systemConfig` request `index` of a seed: a starting config, an L1 timestamp, and
    /// one to four receipts of mixed logs, or (one in five) a single well-formed update.
    pub fn request(seed: u64, index: u64) -> Request {
        let mut rng = Rng::new(&[seed, index, u64::from_be_bytes(*b"sysconfg")]);
        let l1_timestamp = Self::l1_timestamp(&mut rng);
        let config = Self::config(&mut rng, l1_timestamp);
        let receipts = if rng.chance(20) {
            let update = ConfigUpdate::any(&mut rng);
            vec![L1Receipt { status: 1, logs: vec![update.log()] }]
        } else {
            (0..rng.range(1, 4))
                .map(|_| L1Receipt {
                    status: u8::from(rng.chance(85)),
                    logs: (0..rng.range(0, 5)).map(|_| Self::random_log(&mut rng)).collect(),
                })
                .collect()
        };
        Request {
            id: format!("sysconfig-{seed:x}-{index}"),
            chain: "base-mainnet".to_owned(),
            mode: Mode::SystemConfig,
            ancestors: Vec::new(),
            header: Default::default(),
            transactions: Vec::new(),
            witness: Default::default(),
            trace: None,
            system_config: Some(SystemConfigInput {
                l1_timestamp,
                forks: Self::forks(l1_timestamp),
                config,
                receipts,
            }),
        }
    }

    /// The L1 receipt of a chain's system config change at an L2 block at `timestamp`: a full
    /// change to a fresh random config, a single-parameter change, or several updates of one
    /// kind (the last wins), now and then with a malformed log that derivation skips.
    pub fn chain_update(rng: &mut Rng, current: &SystemConfig, timestamp: u64) -> L1Receipt {
        let target = SysConfig::random(rng, timestamp).config;
        let mut updates = match rng.below(20) {
            0..=7 => ConfigUpdate::towards(current, &target),
            8..=16 => vec![ConfigUpdate::single(rng, current, &target)],
            _ => {
                let other = SysConfig::random(rng, timestamp).config;
                let first = ConfigUpdate::single(rng, current, &other);
                let mut last = ConfigUpdate::single(rng, current, &target);
                while last.kind() != first.kind() {
                    last = ConfigUpdate::single(rng, current, &target);
                }
                vec![first, last]
            }
        };
        // Order does not matter across kinds; shuffle so no kind always lands first.
        for i in (1..updates.len()).rev() {
            updates.swap(i, rng.index(i + 1));
        }
        let mut logs: Vec<Log> = updates.iter().map(ConfigUpdate::log).collect();
        if rng.chance(15) {
            let update = ConfigUpdate::any(rng);
            logs.insert(rng.index(logs.len() + 1), Self::malformed(rng, &update));
        }
        L1Receipt { status: 1, logs }
    }
}

#[cfg(test)]
mod tests {
    use base_common_genesis::SystemConfigUpdateKind;

    use super::{ConfigLogs, ConfigUpdate};
    use crate::r#gen::rng::Rng;

    /// Every well-formed update applies with base's parser and sets the values it encodes.
    #[test]
    fn well_formed_updates_apply_with_their_values() {
        let mut rng = Rng::new(&[7]);
        for _ in 0..400 {
            let kind = rng.below(ConfigUpdate::KINDS);
            let update = ConfigUpdate::random(&mut rng, kind);
            let receipt = crate::L1Receipt { status: 1, logs: vec![update.log()] };
            let mut config = base_common_genesis::SystemConfig::default();
            let (applied, errors) =
                config.update_with_receipts(&[(&receipt).into()], ConfigLogs::address(), false);
            assert!(errors.is_empty(), "{update:?}: {errors:?}");
            assert_eq!(applied, vec![SystemConfigUpdateKind::try_from(kind).unwrap()]);
            match update {
                ConfigUpdate::Batcher(address) => assert_eq!(config.batcher_address, address),
                ConfigUpdate::GasConfig { overhead, scalar } => {
                    assert_eq!((config.overhead, config.scalar), (overhead.into(), scalar.into()));
                }
                ConfigUpdate::GasLimit(value) => assert_eq!(config.gas_limit, value),
                ConfigUpdate::UnsafeBlockSigner(_) => {}
                ConfigUpdate::Eip1559 { denominator, elasticity } => assert_eq!(
                    (config.eip1559_denominator, config.eip1559_elasticity),
                    (Some(denominator), Some(elasticity))
                ),
                ConfigUpdate::OperatorFee { scalar, constant } => assert_eq!(
                    (config.operator_fee_scalar, config.operator_fee_constant),
                    (Some(scalar), Some(constant))
                ),
                ConfigUpdate::MinBaseFee(value) => assert_eq!(config.min_base_fee, Some(value)),
                ConfigUpdate::DaFootprintGasScalar(value) => assert_eq!(
                    config.da_footprint_gas_scalar,
                    Some(if value == 0 { 400 } else { value })
                ),
            }
        }
    }

    /// Applying the updates `towards` a target reaches the target (before Ecotone, where gas
    /// config updates keep the overhead).
    #[test]
    fn towards_reaches_the_target() {
        let mut rng = Rng::new(&[8]);
        for _ in 0..50 {
            let mut config = crate::SysConfig::random(&mut rng, 0).config;
            let target = crate::SysConfig::random(&mut rng, 0).config;
            // An update to zero sets the DA footprint gas scalar to its default.
            let mut expected = target;
            if config.da_footprint_gas_scalar != target.da_footprint_gas_scalar
                && target.da_footprint_gas_scalar == Some(0)
            {
                expected.da_footprint_gas_scalar = Some(400);
            }
            let receipt = crate::L1Receipt {
                status: 1,
                logs: ConfigUpdate::towards(&config, &target)
                    .iter()
                    .map(ConfigUpdate::log)
                    .collect(),
            };
            let (_, errors) =
                config.update_with_receipts(&[(&receipt).into()], ConfigLogs::address(), false);
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(config, expected);
        }
    }
}
