use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use alloy_chains::Chain;
use alloy_consensus::{BlockHeader, Header, proofs::storage_root_unhashed};
use alloy_eips::eip7840::BlobParams;
use alloy_genesis::Genesis;
use alloy_hardforks::Hardfork;
use alloy_primitives::{Address, B256, U256};
use base_common_chains::{BaseUpgradeExt, ChainConfig, Upgrades};
use base_common_consensus::Predeploys;
use base_common_genesis::{
    BaseUpgrade, RuntimeUpgradeRegistry, UpgradeActivation, UpgradeActivationSink,
};
use derive_more::{Constructor, Deref, Into};
use reth_chainspec::{
    BaseFeeParams, BaseFeeParamsKind, ChainSpec, DepositContract, DisplayHardforks, EthChainSpec,
    EthereumHardforks, ForkFilter, ForkId, Hardforks, Head,
};
use reth_ethereum_forks::{ChainHardforks, EthereumHardfork, ForkCondition};
use reth_network_peers::{NodeRecord, parse_nodes};
use reth_primitives_traits::SealedHeader;

use crate::{ChainUpgradesExt, compute_jovian_base_fee, decode_holocene_base_fee};

/// Error constructing a [`BaseChainSpec`].
#[derive(Debug, thiserror::Error)]
pub enum BaseChainSpecError {
    /// Genesis JSON failed to deserialize.
    #[error("invalid genesis JSON: {0}")]
    GenesisJson(#[from] serde_json::Error),
    /// Beryl is scheduled but no activation registry admin address is configured.
    #[error("missing activation admin address for Beryl-enabled chain ID: {chain_id}")]
    MissingActivationAdminAddress {
        /// Chain ID whose Beryl-enabled configuration lacks an activation admin address.
        chain_id: u64,
    },
    /// Beryl is scheduled but the activation registry admin address is `Address::ZERO`.
    #[error("activation admin address must not be zero for Beryl-enabled chain ID: {chain_id}")]
    ZeroActivationAdminAddress {
        /// Chain ID whose Beryl-enabled configuration has a zero activation admin address.
        chain_id: u64,
    },
}

/// Genesis info extracted from a Base genesis config.
#[derive(Default, Debug)]
pub struct GenesisInfo {
    /// Base chain info extracted from genesis extra fields.
    pub base_chain_info: base_common_rpc_types::ChainInfo,
    /// Base fee params derived from the genesis config.
    pub base_fee_params: BaseFeeParamsKind,
}

impl GenesisInfo {
    /// Extracts Base genesis info from an [`alloy_genesis::Genesis`].
    pub fn extract_from(genesis: &Genesis) -> Self {
        let mut info = Self {
            base_chain_info: base_common_rpc_types::ChainInfo::extract_from(
                &genesis.config.extra_fields,
            )
            .unwrap_or_default(),
            ..Default::default()
        };
        if let Some(base_fee_info) = &info.base_chain_info.base_fee_info
            && let (Some(elasticity), Some(denominator)) =
                (base_fee_info.eip1559_elasticity, base_fee_info.eip1559_denominator)
        {
            let base_fee_params = base_fee_info.eip1559_denominator_canyon.map_or_else(
                || BaseFeeParams::new(denominator as u128, elasticity as u128).into(),
                |canyon_denominator| {
                    BaseFeeParamsKind::Variable(
                        vec![
                            (
                                EthereumHardfork::London.boxed(),
                                BaseFeeParams::new(denominator as u128, elasticity as u128),
                            ),
                            (
                                BaseUpgrade::Canyon.boxed(),
                                BaseFeeParams::new(canyon_denominator as u128, elasticity as u128),
                            ),
                        ]
                        .into(),
                    )
                },
            );

            info.base_fee_params = base_fee_params;
        }

        info
    }
}

/// Base chain spec type.
#[derive(Debug, Clone, Deref, Into, Constructor, PartialEq, Eq)]
pub struct BaseChainSpec {
    /// [`ChainSpec`].
    #[deref]
    pub inner: ChainSpec,
    /// Activation registry admin address.
    #[deref(ignore)]
    pub activation_admin_address: Option<Address>,
}

impl BaseChainSpec {
    /// Builds the Base Mainnet chain spec from [`ChainConfig::mainnet`].
    pub fn mainnet() -> Self {
        Self::try_from(ChainConfig::mainnet()).expect("Base mainnet chain config must be valid")
    }

    /// Builds the Base Sepolia chain spec from [`ChainConfig::sepolia`].
    pub fn sepolia() -> Self {
        Self::try_from(ChainConfig::sepolia()).expect("Base Sepolia chain config must be valid")
    }

    /// Builds the Base Zeronet chain spec from [`ChainConfig::zeronet`].
    pub fn zeronet() -> Self {
        Self::try_from(ChainConfig::zeronet()).expect("Base Zeronet chain config must be valid")
    }

    /// Builds the local dev chain spec from [`ChainConfig::devnet`].
    pub fn devnet() -> Self {
        Self::try_from(ChainConfig::devnet()).expect("Base devnet chain config must be valid")
    }

    /// Converts the given [`Genesis`] into an [`BaseChainSpec`].
    pub fn from_genesis(genesis: Genesis) -> Self {
        Self::try_from_genesis(genesis)
            .expect("Beryl-enabled genesis must configure activationAdminAddress")
    }

    /// Tries to convert the given [`Genesis`] into a [`BaseChainSpec`].
    pub fn try_from_genesis(genesis: Genesis) -> Result<Self, BaseChainSpecError> {
        let base_genesis_info = GenesisInfo::extract_from(&genesis);
        let genesis_info = base_genesis_info.base_chain_info.genesis_info.unwrap_or_default();
        let activation_admin_address = genesis_info.activation_admin_address;

        // Block-based upgrades in canonical fork ID order.
        let block_upgrade_opts = [
            (EthereumHardfork::Frontier.boxed(), Some(0)),
            (EthereumHardfork::Homestead.boxed(), genesis.config.homestead_block),
            (EthereumHardfork::Tangerine.boxed(), genesis.config.eip150_block),
            (EthereumHardfork::SpuriousDragon.boxed(), genesis.config.eip155_block),
            (EthereumHardfork::Byzantium.boxed(), genesis.config.byzantium_block),
            (EthereumHardfork::Constantinople.boxed(), genesis.config.constantinople_block),
            (EthereumHardfork::Petersburg.boxed(), genesis.config.petersburg_block),
            (EthereumHardfork::Istanbul.boxed(), genesis.config.istanbul_block),
            (EthereumHardfork::MuirGlacier.boxed(), genesis.config.muir_glacier_block),
            (EthereumHardfork::Berlin.boxed(), genesis.config.berlin_block),
            (EthereumHardfork::London.boxed(), genesis.config.london_block),
            (EthereumHardfork::ArrowGlacier.boxed(), genesis.config.arrow_glacier_block),
            (EthereumHardfork::GrayGlacier.boxed(), genesis.config.gray_glacier_block),
        ];
        let mut upgrades = block_upgrade_opts
            .into_iter()
            .filter_map(|(upgrade, opt)| opt.map(|block| (upgrade, ForkCondition::Block(block))))
            .collect::<Vec<_>>();

        // We set the paris upgrade for Base networks to zero
        upgrades.push((
            EthereumHardfork::Paris.boxed(),
            ForkCondition::TTD {
                activation_block_number: 0,
                total_difficulty: U256::ZERO,
                fork_block: genesis.config.merge_netsplit_block,
            },
        ));

        if let Some(block) = genesis_info.bedrock_block {
            upgrades.push((BaseUpgrade::Bedrock.boxed(), ForkCondition::Block(block)));
        }

        // Time-based upgrades
        // L1 upgrades are mapped to the activation timestamps of the corresponding Base upgrades
        let azul_time = genesis_info.base.azul;
        let beryl_time = genesis_info.base.beryl;
        let cobalt_time = genesis_info.base.cobalt;
        let time_upgrade_opts = [
            (BaseUpgrade::Regolith.boxed(), genesis_info.regolith_time),
            (EthereumHardfork::Shanghai.boxed(), genesis_info.canyon_time),
            (BaseUpgrade::Canyon.boxed(), genesis_info.canyon_time),
            (EthereumHardfork::Cancun.boxed(), genesis_info.ecotone_time),
            (BaseUpgrade::Ecotone.boxed(), genesis_info.ecotone_time),
            (BaseUpgrade::Fjord.boxed(), genesis_info.fjord_time),
            (BaseUpgrade::Granite.boxed(), genesis_info.granite_time),
            (BaseUpgrade::Holocene.boxed(), genesis_info.holocene_time),
            (EthereumHardfork::Prague.boxed(), genesis_info.isthmus_time),
            (BaseUpgrade::Isthmus.boxed(), genesis_info.isthmus_time),
            (BaseUpgrade::Jovian.boxed(), genesis_info.jovian_time),
            (EthereumHardfork::Osaka.boxed(), azul_time),
            (BaseUpgrade::Azul.boxed(), azul_time),
            (BaseUpgrade::Beryl.boxed(), beryl_time),
            (BaseUpgrade::Cobalt.boxed(), cobalt_time),
        ];

        let mut time_upgrades = time_upgrade_opts
            .into_iter()
            .filter_map(|(upgrade, opt)| opt.map(|time| (upgrade, ForkCondition::Timestamp(time))))
            .collect::<Vec<_>>();

        upgrades.append(&mut time_upgrades);

        let upgrades = ChainHardforks::new(upgrades);
        let chain_id = genesis.config.chain_id;
        Self::validate_beryl_activation_admin(&upgrades, activation_admin_address, chain_id)?;
        let genesis_header =
            SealedHeader::seal_slow(Self::make_genesis_header(&genesis, &upgrades));

        Ok(Self {
            inner: ChainSpec {
                chain: chain_id.into(),
                genesis_header,
                genesis,
                hardforks: upgrades,
                paris_block_and_final_difficulty: Some((0, U256::ZERO)),
                base_fee_params: base_genesis_info.base_fee_params,
                ..Default::default()
            },
            activation_admin_address,
        })
    }

    /// Tries to convert the given [`ChainSpec`] into a [`BaseChainSpec`].
    pub fn try_from_chainspec(
        value: ChainSpec,
        activation_admin_address: Option<Address>,
    ) -> Result<Self, BaseChainSpecError> {
        Self::validate_beryl_activation_admin(
            &value.hardforks,
            activation_admin_address,
            value.chain.id(),
        )?;
        Ok(Self { inner: value, activation_admin_address })
    }

    /// Validates that Beryl-enabled chains have a valid activation registry admin address:
    /// present (not `None`) and non-zero.
    pub fn validate_beryl_activation_admin(
        upgrades: &ChainHardforks,
        activation_admin_address: Option<Address>,
        chain_id: u64,
    ) -> Result<(), BaseChainSpecError> {
        let beryl_scheduled = !matches!(upgrades.fork(BaseUpgrade::Beryl), ForkCondition::Never);

        if activation_admin_address.is_none() && beryl_scheduled {
            return Err(BaseChainSpecError::MissingActivationAdminAddress { chain_id });
        }

        if matches!(activation_admin_address, Some(addr) if addr.is_zero()) && beryl_scheduled {
            return Err(BaseChainSpecError::ZeroActivationAdminAddress { chain_id });
        }

        Ok(())
    }

    /// Builds a [`Header`] for the genesis block of a Base chain.
    ///
    /// Extends [`reth_chainspec::make_genesis_header`] with Isthmus-specific withdrawals root
    /// logic: if Isthmus is active at the genesis timestamp, the withdrawals root is set to the
    /// storage root of the `L2ToL1MessagePasser` predeploy.
    pub fn make_genesis_header(genesis: &Genesis, upgrades: &ChainHardforks) -> Header {
        let mut header = reth_chainspec::make_genesis_header(genesis, upgrades);

        if upgrades.fork(BaseUpgrade::Isthmus).active_at_timestamp(header.timestamp)
            && let Some(predeploy) = genesis.alloc.get(&Predeploys::L2_TO_L1_MESSAGE_PASSER)
            && let Some(storage) = &predeploy.storage
        {
            header.withdrawals_root =
                Some(storage_root_unhashed(storage.iter().filter_map(|(k, v)| {
                    if v.is_zero() { None } else { Some((*k, (*v).into())) }
                })));
        }

        header
    }

    /// Parses a chain name into an [`BaseChainSpec`], if recognized.
    pub fn parse_chain(s: &str) -> Option<Arc<Self>> {
        let cfg = ChainConfig::by_name(s)?;
        Some(Arc::new(
            Self::try_from(cfg).expect("recognized Base chain config must build a valid chainspec"),
        ))
    }

    /// Activates or updates the given upgrade condition in-place.
    pub fn set_fork<H: Hardfork>(&mut self, fork: H, condition: ForkCondition) {
        self.inner.hardforks.insert(fork, condition);
    }

    /// Returns the runtime-aware activation condition for a hardfork.
    pub fn fork<H: Hardfork>(&self, fork: H) -> ForkCondition {
        self.runtime_fork_condition(&fork).unwrap_or_else(|| self.inner.fork(fork))
    }

    /// Returns a runtime upgrade override for an execution fork condition.
    pub fn runtime_fork_condition<H: Hardfork + ?Sized>(&self, fork: &H) -> Option<ForkCondition> {
        let upgrade_id = BaseUpgrade::from_contract_fork_name(fork.name())?;
        RuntimeUpgradeRegistry::activation(self.chain().id(), upgrade_id).map(|activation| {
            match activation {
                UpgradeActivation::Never => ForkCondition::Never,
                UpgradeActivation::Timestamp(timestamp) => ForkCondition::Timestamp(timestamp),
            }
        })
    }

    /// Returns hardforks with runtime overrides materialized into the schedule.
    pub fn runtime_hardforks(&self) -> ChainHardforks {
        let mut hardforks = self.inner.hardforks.clone();
        if let Some(overrides) = RuntimeUpgradeRegistry::overrides(self.chain().id()) {
            for (hardfork_id, activation) in overrides.activations {
                let condition = match activation {
                    UpgradeActivation::Never => ForkCondition::Never,
                    UpgradeActivation::Timestamp(timestamp) => ForkCondition::Timestamp(timestamp),
                };
                Self::set_hardfork_activation_condition_for(&mut hardforks, hardfork_id, condition);
            }
        }

        hardforks
    }

    /// Returns the inner chain spec with runtime hardfork overrides materialized.
    pub fn runtime_chain_spec(&self) -> ChainSpec {
        let mut inner = self.inner.clone();
        inner.hardforks = self.runtime_hardforks();
        inner
    }

    /// Get an iterator of all hardforks with runtime-aware activation conditions.
    pub fn forks_iter(&self) -> impl Iterator<Item = (&dyn Hardfork, ForkCondition)> {
        self.inner.forks_iter().map(|(fork, condition)| {
            let condition = self.runtime_fork_condition(fork).unwrap_or(condition);
            (fork, condition)
        })
    }

    /// Returns the runtime-aware fork ID for the given head.
    pub fn fork_id(&self, head: &Head) -> ForkId {
        self.runtime_chain_spec().fork_id(head)
    }

    /// Returns the runtime-aware fork ID for the latest fork.
    pub fn latest_fork_id(&self) -> ForkId {
        self.runtime_chain_spec().latest_fork_id()
    }

    /// Creates a runtime-aware fork filter for the block described by `head`.
    pub fn fork_filter(&self, head: Head) -> ForkFilter {
        self.runtime_chain_spec().fork_filter(head)
    }

    /// Returns the runtime-aware fork ID for the given hardfork.
    pub fn hardfork_fork_id<HF: Hardfork + Clone>(&self, fork: HF) -> Option<ForkId> {
        self.runtime_chain_spec().hardfork_fork_id(fork)
    }

    /// Recomputes the sealed genesis header from the current genesis and hardfork schedule.
    pub fn refresh_genesis_header(&mut self) {
        self.inner.genesis_header = SealedHeader::seal_slow(Self::make_genesis_header(
            &self.inner.genesis,
            &self.inner.hardforks,
        ));
    }

    /// Clears all timestamp-based Base hardfork activation conditions.
    pub fn clear_hardfork_activation_timestamps(&mut self) {
        for hardfork_id in BaseUpgrade::CONTRACT_VARIANTS {
            Self::set_hardfork_activation_condition_for(
                &mut self.inner.hardforks,
                hardfork_id,
                ForkCondition::Never,
            );
        }
    }

    /// Clears a timestamp-based hardfork activation condition by contract hardfork ID.
    pub fn clear_hardfork_activation_timestamp(&mut self, hardfork_id: BaseUpgrade) -> bool {
        self.try_clear_hardfork_activation_timestamp(hardfork_id).unwrap_or(false)
    }

    /// Clears a timestamp-based hardfork activation condition by contract hardfork ID.
    pub fn try_clear_hardfork_activation_timestamp(
        &mut self,
        hardfork_id: BaseUpgrade,
    ) -> Result<bool, BaseChainSpecError> {
        self.try_set_hardfork_activation_condition(hardfork_id, ForkCondition::Never)
    }

    /// Sets a timestamp-based hardfork activation condition by contract hardfork ID.
    pub fn set_hardfork_activation_timestamp(
        &mut self,
        hardfork_id: BaseUpgrade,
        timestamp: u64,
    ) -> bool {
        self.try_set_hardfork_activation_timestamp(hardfork_id, timestamp).unwrap_or(false)
    }

    /// Sets a timestamp-based hardfork activation condition by contract hardfork ID.
    pub fn try_set_hardfork_activation_timestamp(
        &mut self,
        hardfork_id: BaseUpgrade,
        timestamp: u64,
    ) -> Result<bool, BaseChainSpecError> {
        self.try_set_hardfork_activation_condition(hardfork_id, ForkCondition::Timestamp(timestamp))
    }

    /// Sets a hardfork activation condition by contract hardfork ID.
    pub fn set_hardfork_activation_condition(
        &mut self,
        hardfork_id: BaseUpgrade,
        condition: ForkCondition,
    ) -> bool {
        self.try_set_hardfork_activation_condition(hardfork_id, condition).unwrap_or(false)
    }

    /// Sets a hardfork activation condition by contract hardfork ID after validating invariants.
    pub fn try_set_hardfork_activation_condition(
        &mut self,
        hardfork_id: BaseUpgrade,
        condition: ForkCondition,
    ) -> Result<bool, BaseChainSpecError> {
        let mut hardforks = self.inner.hardforks.clone();
        if !Self::set_hardfork_activation_condition_for(&mut hardforks, hardfork_id, condition) {
            return Ok(false);
        }

        Self::validate_beryl_activation_admin(
            &hardforks,
            self.activation_admin_address,
            self.inner.chain.id(),
        )?;
        self.inner.hardforks = hardforks;

        Ok(true)
    }

    /// Sets a hardfork activation condition by contract hardfork ID on a hardfork collection.
    pub fn set_hardfork_activation_condition_for(
        hardforks: &mut ChainHardforks,
        hardfork_id: BaseUpgrade,
        condition: ForkCondition,
    ) -> bool {
        let mut inserted = false;

        if let Some(execution_hardfork) = hardfork_id.execution_hardfork() {
            hardforks.insert(execution_hardfork, condition);
            inserted = true;
        }
        // Only execution-ladder upgrades enter the reth hardfork schedule; contract-only
        // upgrades (Delta, PectraBlobSchedule) are ignored here.
        if hardfork_id.is_execution() {
            hardforks.insert(hardfork_id, condition);
            inserted = true;
        }

        inserted
    }
}

impl UpgradeActivationSink for BaseChainSpec {
    type Error = BaseChainSpecError;

    fn apply_activation(
        &mut self,
        hardfork_id: BaseUpgrade,
        activation: UpgradeActivation,
    ) -> Result<bool, Self::Error> {
        match activation {
            UpgradeActivation::Timestamp(timestamp) => {
                self.try_set_hardfork_activation_timestamp(hardfork_id, timestamp)
            }
            UpgradeActivation::Never => self.try_clear_hardfork_activation_timestamp(hardfork_id),
        }
    }

    fn finalize(&mut self) -> Result<(), Self::Error> {
        self.refresh_genesis_header();
        Ok(())
    }
}

impl TryFrom<&ChainConfig> for BaseChainSpec {
    type Error = BaseChainSpecError;

    fn try_from(cfg: &ChainConfig) -> Result<Self, Self::Error> {
        let genesis = serde_json::from_str(cfg.genesis_json)?;
        let upgrades =
            base_common_chains::ChainUpgrades::new(BaseUpgrade::forks_for(cfg)).to_chain_upgrades();
        let activation_admin_address = cfg.beryl_activation_admin_address();
        Self::validate_beryl_activation_admin(&upgrades, activation_admin_address, cfg.chain_id)?;
        let genesis_header = match cfg.genesis_l2_hash {
            B256::ZERO => SealedHeader::seal_slow(Self::make_genesis_header(&genesis, &upgrades)),
            hash => SealedHeader::new(Self::make_genesis_header(&genesis, &upgrades), hash),
        };
        let fee_config = cfg.fee_config();
        let base_fee_params = BaseFeeParamsKind::Variable(
            vec![
                (
                    EthereumHardfork::London.boxed(),
                    BaseFeeParams::new(
                        fee_config.eip1559_denominator as u128,
                        fee_config.eip1559_elasticity as u128,
                    ),
                ),
                (
                    BaseUpgrade::Canyon.boxed(),
                    BaseFeeParams::new(
                        fee_config.eip1559_denominator_canyon as u128,
                        fee_config.eip1559_elasticity as u128,
                    ),
                ),
            ]
            .into(),
        );

        Ok(Self {
            inner: ChainSpec {
                chain: cfg.chain_id.into(),
                genesis_header,
                genesis,
                paris_block_and_final_difficulty: Some((0, U256::ZERO)),
                hardforks: upgrades,
                base_fee_params,
                prune_delete_limit: cfg.prune_delete_limit,
                ..Default::default()
            },
            activation_admin_address,
        })
    }
}

impl EthChainSpec for BaseChainSpec {
    type Header = Header;

    fn chain(&self) -> Chain {
        self.inner.chain()
    }

    fn base_fee_params_at_timestamp(&self, timestamp: u64) -> BaseFeeParams {
        self.runtime_chain_spec().base_fee_params_at_timestamp(timestamp)
    }

    fn blob_params_at_timestamp(&self, timestamp: u64) -> Option<BlobParams> {
        self.runtime_chain_spec().blob_params_at_timestamp(timestamp)
    }

    fn deposit_contract(&self) -> Option<&DepositContract> {
        self.inner.deposit_contract()
    }

    fn genesis_hash(&self) -> B256 {
        self.inner.genesis_hash()
    }

    fn prune_delete_limit(&self) -> usize {
        self.inner.prune_delete_limit()
    }

    fn display_hardforks(&self) -> Box<dyn core::fmt::Display> {
        let hardforks = self.runtime_hardforks();
        let base_forks = hardforks.forks_iter().filter(|(fork, _)| {
            !EthereumHardfork::VARIANTS.iter().any(|h| h.name() == (*fork).name())
        });

        Box::new(DisplayHardforks::new(base_forks))
    }

    fn genesis_header(&self) -> &Self::Header {
        self.inner.genesis_header()
    }

    fn genesis(&self) -> &Genesis {
        self.inner.genesis()
    }

    fn bootnodes(&self) -> Option<Vec<NodeRecord>> {
        ChainConfig::by_chain_id(self.chain().id()).map(|cfg| parse_nodes(cfg.bootnodes.execution))
    }

    fn is_optimism(&self) -> bool {
        true
    }

    fn final_paris_total_difficulty(&self) -> Option<U256> {
        self.inner.final_paris_total_difficulty()
    }

    fn next_block_base_fee(&self, parent: &Header, target_timestamp: u64) -> Option<u64> {
        if Upgrades::is_jovian_active_at_timestamp(self, parent.timestamp()) {
            compute_jovian_base_fee(self, parent, target_timestamp).ok()
        } else if Upgrades::is_holocene_active_at_timestamp(self, parent.timestamp()) {
            decode_holocene_base_fee(self, parent, target_timestamp).ok()
        } else {
            self.runtime_chain_spec().next_block_base_fee(parent, target_timestamp)
        }
    }
}

impl Hardforks for BaseChainSpec {
    fn fork<H: Hardfork>(&self, fork: H) -> ForkCondition {
        Self::fork(self, fork)
    }

    fn forks_iter(&self) -> impl Iterator<Item = (&dyn Hardfork, ForkCondition)> {
        Self::forks_iter(self)
    }

    fn fork_id(&self, head: &Head) -> ForkId {
        self.runtime_chain_spec().fork_id(head)
    }

    fn latest_fork_id(&self) -> ForkId {
        self.runtime_chain_spec().latest_fork_id()
    }

    fn fork_filter(&self, head: Head) -> ForkFilter {
        self.runtime_chain_spec().fork_filter(head)
    }
}

impl EthereumHardforks for BaseChainSpec {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self.fork(fork)
    }
}

impl Upgrades for BaseChainSpec {
    fn upgrade_activation(&self, fork: BaseUpgrade) -> ForkCondition {
        self.fork(fork)
    }

    fn activation_admin_address(&self) -> Option<Address> {
        self.activation_admin_address
    }
}

impl From<Genesis> for BaseChainSpec {
    fn from(genesis: Genesis) -> Self {
        Self::from_genesis(genesis)
    }
}

impl From<ChainSpec> for BaseChainSpec {
    fn from(value: ChainSpec) -> Self {
        Self::try_from_chainspec(value, None)
            .expect("Beryl-enabled chain spec requires activation admin")
    }
}

#[cfg(test)]
mod tests {
    use alloc::{
        string::{String, ToString},
        vec,
        vec::Vec,
    };
    use core::str::FromStr;

    use alloy_chains::Chain;
    use alloy_consensus::proofs::storage_root_unhashed;
    use alloy_genesis::{ChainConfig as AlloyChainConfig, Genesis};
    use alloy_hardforks::Hardfork;
    use alloy_primitives::{Address, B256, U256, address, b256};
    use base_common_chains::{ChainConfig, Upgrades};
    use base_common_genesis::{BaseUpgrade, RuntimeUpgradeRegistry};
    use base_common_rpc_types::FeeInfo;
    use reth_chainspec::{
        BaseFeeParams, BaseFeeParamsKind, ChainSpec, EthChainSpec, EthereumHardforks, test_fork_ids,
    };
    use reth_ethereum_forks::{EthereumHardfork, ForkCondition, ForkHash, ForkId, Head};

    use crate::{BaseChainSpec, BaseChainSpecBuilder, BaseChainSpecError};

    #[test]
    fn test_storage_root_consistency() {
        let k1 =
            B256::from_str("0x0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let v1 =
            U256::from_str("0x0000000000000000000000000000000000000000000000000000000000000000")
                .unwrap();
        let k2 =
            B256::from_str("0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc")
                .unwrap();
        let v2 =
            U256::from_str("0x000000000000000000000000c0d3c0d3c0d3c0d3c0d3c0d3c0d3c0d3c0d30016")
                .unwrap();
        let k3 =
            B256::from_str("0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103")
                .unwrap();
        let v3 =
            U256::from_str("0x0000000000000000000000004200000000000000000000000000000000000018")
                .unwrap();
        let origin_root =
            B256::from_str("0x5d5ba3a8093ede3901ad7a569edfb7b9aecafa54730ba0bf069147cbcc00e345")
                .unwrap();
        let expected_root =
            B256::from_str("0x8ed4baae3a927be3dea54996b4d5899f8c01e7594bf50b17dc1e741388ce3d12")
                .unwrap();

        let storage_origin = vec![(k1, v1), (k2, v2), (k3, v3)];
        let storage_fix = vec![(k2, v2), (k3, v3)];
        let root_origin = storage_root_unhashed(storage_origin);
        let root_fix = storage_root_unhashed(storage_fix);
        assert_ne!(root_origin, root_fix);
        assert_eq!(root_origin, origin_root);
        assert_eq!(root_fix, expected_root);
    }

    #[test]
    fn base_mainnet_forkids() {
        let base_mainnet_spec = BaseChainSpec::mainnet();
        let mut base_mainnet = BaseChainSpecBuilder::base_mainnet().build();
        base_mainnet.inner.genesis_header.set_hash(base_mainnet_spec.genesis_hash());
        test_fork_ids(
            &base_mainnet_spec,
            &[
                (
                    Head { number: 0, ..Default::default() },
                    ForkId { hash: ForkHash([0x67, 0xda, 0x02, 0x60]), next: 1704992401 },
                ),
                (
                    Head { number: 0, timestamp: 1704992400, ..Default::default() },
                    ForkId { hash: ForkHash([0x67, 0xda, 0x02, 0x60]), next: 1704992401 },
                ),
                (
                    Head { number: 0, timestamp: 1704992401, ..Default::default() },
                    ForkId { hash: ForkHash([0x3c, 0x28, 0x3c, 0xb3]), next: 1710374401 },
                ),
                (
                    Head { number: 0, timestamp: 1710374400, ..Default::default() },
                    ForkId { hash: ForkHash([0x3c, 0x28, 0x3c, 0xb3]), next: 1710374401 },
                ),
                (
                    Head { number: 0, timestamp: 1710374401, ..Default::default() },
                    ForkId { hash: ForkHash([0x51, 0xcc, 0x98, 0xb3]), next: 1720627201 },
                ),
                (
                    Head { number: 0, timestamp: 1720627200, ..Default::default() },
                    ForkId { hash: ForkHash([0x51, 0xcc, 0x98, 0xb3]), next: 1720627201 },
                ),
                (
                    Head { number: 0, timestamp: 1720627201, ..Default::default() },
                    ForkId { hash: ForkHash([0xe4, 0x01, 0x0e, 0xb9]), next: 1726070401 },
                ),
                (
                    Head { number: 0, timestamp: 1726070401, ..Default::default() },
                    ForkId { hash: ForkHash([0xbc, 0x38, 0xf9, 0xca]), next: 1736445601 },
                ),
                (
                    Head { number: 0, timestamp: 1736445601, ..Default::default() },
                    ForkId { hash: ForkHash([0x3a, 0x2a, 0xf1, 0x83]), next: 1746806401 },
                ),
                (
                    Head { number: 0, timestamp: 1746806401, ..Default::default() },
                    ForkId {
                        hash: ForkHash([0x86, 0x72, 0x8b, 0x4e]),
                        next: ChainConfig::mainnet().jovian_timestamp,
                    },
                ),
                (
                    Head {
                        number: 0,
                        timestamp: ChainConfig::mainnet().jovian_timestamp,
                        ..Default::default()
                    },
                    base_mainnet_spec.hardfork_fork_id(BaseUpgrade::Jovian).unwrap(),
                ),
                (
                    Head {
                        number: 0,
                        timestamp: ChainConfig::mainnet().azul_timestamp.unwrap(),
                        ..Default::default()
                    },
                    base_mainnet_spec.hardfork_fork_id(BaseUpgrade::Azul).unwrap(),
                ),
            ],
        );
    }

    #[test]
    fn base_sepolia_forkids() {
        let base_sepolia_spec = BaseChainSpec::sepolia();
        test_fork_ids(
            &base_sepolia_spec,
            &[
                (
                    Head { number: 0, ..Default::default() },
                    ForkId { hash: ForkHash([0xb9, 0x59, 0xb9, 0xf7]), next: 1699981200 },
                ),
                (
                    Head { number: 0, timestamp: 1699981199, ..Default::default() },
                    ForkId { hash: ForkHash([0xb9, 0x59, 0xb9, 0xf7]), next: 1699981200 },
                ),
                (
                    Head { number: 0, timestamp: 1699981200, ..Default::default() },
                    ForkId { hash: ForkHash([0x60, 0x7c, 0xd5, 0xa1]), next: 1708534800 },
                ),
                (
                    Head { number: 0, timestamp: 1708534799, ..Default::default() },
                    ForkId { hash: ForkHash([0x60, 0x7c, 0xd5, 0xa1]), next: 1708534800 },
                ),
                (
                    Head { number: 0, timestamp: 1708534800, ..Default::default() },
                    ForkId { hash: ForkHash([0xbe, 0x96, 0x9b, 0x17]), next: 1716998400 },
                ),
                (
                    Head { number: 0, timestamp: 1716998399, ..Default::default() },
                    ForkId { hash: ForkHash([0xbe, 0x96, 0x9b, 0x17]), next: 1716998400 },
                ),
                (
                    Head { number: 0, timestamp: 1716998400, ..Default::default() },
                    ForkId { hash: ForkHash([0x4e, 0x45, 0x7a, 0x49]), next: 1723478400 },
                ),
                (
                    Head { number: 0, timestamp: 1723478399, ..Default::default() },
                    ForkId { hash: ForkHash([0x4e, 0x45, 0x7a, 0x49]), next: 1723478400 },
                ),
                (
                    Head { number: 0, timestamp: 1723478400, ..Default::default() },
                    ForkId { hash: ForkHash([0x5e, 0xdf, 0xa3, 0xb6]), next: 1732633200 },
                ),
                (
                    Head { number: 0, timestamp: 1732633200, ..Default::default() },
                    ForkId { hash: ForkHash([0x8b, 0x5e, 0x76, 0x29]), next: 1744905600 },
                ),
                (
                    Head { number: 0, timestamp: 1744905600, ..Default::default() },
                    ForkId {
                        hash: ForkHash([0x06, 0x0a, 0x4d, 0x1d]),
                        next: ChainConfig::sepolia().jovian_timestamp,
                    },
                ),
                (
                    Head {
                        number: 0,
                        timestamp: ChainConfig::sepolia().jovian_timestamp,
                        ..Default::default()
                    },
                    base_sepolia_spec.hardfork_fork_id(BaseUpgrade::Jovian).unwrap(),
                ),
            ],
        );
    }

    #[test]
    fn runtime_registry_overrides_execution_fork_conditions() {
        let chain_id = 9_100_003;
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        let spec = BaseChainSpecBuilder::default()
            .chain(Chain::from_id(chain_id))
            .genesis(Genesis::default())
            .with_fork(EthereumHardfork::Osaka, ForkCondition::Never)
            .with_fork(BaseUpgrade::Azul, ForkCondition::Never)
            .build();
        let chain_id = spec.chain().id();
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        assert_eq!(spec.fork(EthereumHardfork::Osaka), ForkCondition::Never);
        assert_eq!(spec.fork(BaseUpgrade::Azul), ForkCondition::Never);
        assert_eq!(spec.fork(BaseUpgrade::Cobalt), ForkCondition::Never);

        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Azul, 42);
        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Cobalt, 84);

        assert_eq!(spec.fork(EthereumHardfork::Osaka), ForkCondition::Timestamp(42));
        assert_eq!(spec.fork(BaseUpgrade::Azul), ForkCondition::Timestamp(42));
        assert_eq!(spec.fork(BaseUpgrade::Cobalt), ForkCondition::Timestamp(84));

        RuntimeUpgradeRegistry::clear_activation_timestamp(chain_id, BaseUpgrade::Azul);
        RuntimeUpgradeRegistry::clear_activation_timestamp(chain_id, BaseUpgrade::Cobalt);

        assert_eq!(spec.fork(EthereumHardfork::Osaka), ForkCondition::Never);
        assert_eq!(spec.fork(BaseUpgrade::Azul), ForkCondition::Never);
        assert_eq!(spec.fork(BaseUpgrade::Cobalt), ForkCondition::Never);

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn runtime_registry_overrides_execution_fork_ids() {
        let chain_id = 9_100_004;
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        let spec = BaseChainSpecBuilder::default()
            .chain(Chain::from_id(chain_id))
            .genesis(Genesis::default())
            .with_fork(EthereumHardfork::Osaka, ForkCondition::Never)
            .with_fork(BaseUpgrade::Azul, ForkCondition::Never)
            .with_fork(BaseUpgrade::Cobalt, ForkCondition::Never)
            .build();

        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Azul, 42);
        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Cobalt, 84);

        assert_eq!(spec.fork_id(&Head { number: 0, timestamp: 41, ..Default::default() }).next, 42);
        assert_eq!(spec.fork(BaseUpgrade::Cobalt), ForkCondition::Timestamp(84));

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn runtime_registry_overrides_regolith_execution_paths() {
        let chain_id = 9_100_005;
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        let spec = BaseChainSpecBuilder::default()
            .chain(Chain::from_id(chain_id))
            .genesis(Genesis::default())
            .with_fork(BaseUpgrade::Regolith, ForkCondition::Never)
            .build();

        assert_eq!(spec.fork(BaseUpgrade::Regolith), ForkCondition::Never);
        assert!(!spec.is_regolith_active_at_timestamp(42));

        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Regolith, 42);

        assert_eq!(spec.fork(BaseUpgrade::Regolith), ForkCondition::Timestamp(42));
        assert!(!spec.is_regolith_active_at_timestamp(41));
        assert!(spec.is_regolith_active_at_timestamp(42));

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn runtime_registry_overrides_execution_fee_and_blob_params() {
        let chain_id = 9_100_006;
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        let mut config = ChainConfig::mainnet().clone();
        config.chain_id = chain_id;
        config.beryl_timestamp = None;
        config.cobalt_timestamp = None;
        let spec = BaseChainSpec::try_from(&config).unwrap();
        let timestamp = 42;
        let parent = spec.genesis_header();
        let static_base_fee = spec.inner.base_fee_params_at_timestamp(timestamp);
        let static_blob_params = spec.inner.blob_params_at_timestamp(timestamp);
        let static_next_base_fee = spec.inner.next_block_base_fee(parent, timestamp);

        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Canyon, timestamp);
        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Ecotone, timestamp);

        let runtime_chain_spec = spec.runtime_chain_spec();

        assert_eq!(
            spec.base_fee_params_at_timestamp(timestamp),
            runtime_chain_spec.base_fee_params_at_timestamp(timestamp)
        );
        assert_ne!(spec.base_fee_params_at_timestamp(timestamp), static_base_fee);
        assert_eq!(
            spec.blob_params_at_timestamp(timestamp),
            runtime_chain_spec.blob_params_at_timestamp(timestamp)
        );
        assert_ne!(spec.blob_params_at_timestamp(timestamp), static_blob_params);
        assert_eq!(
            spec.next_block_base_fee(parent, timestamp),
            runtime_chain_spec.next_block_base_fee(parent, timestamp)
        );
        assert_ne!(spec.next_block_base_fee(parent, timestamp), static_next_base_fee);

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn base_mainnet_genesis() {
        let base_mainnet_spec = BaseChainSpec::mainnet();
        let genesis = base_mainnet_spec.genesis_header();
        assert_eq!(
            genesis.hash_slow(),
            b256!("0xf712aa9241cc24369b143cf6dce85f0902a9731e70d66818a3a5845b296c73dd")
        );
        let base_fee = base_mainnet_spec.next_block_base_fee(genesis, genesis.timestamp).unwrap();
        assert_eq!(base_fee, 980000000);
    }

    #[test]
    fn activation_admin_matches_beryl_constants() {
        assert_eq!(
            BaseChainSpec::mainnet().activation_admin_address(),
            Some(base_common_chains::MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS)
        );
        assert_eq!(
            BaseChainSpec::sepolia().activation_admin_address(),
            Some(base_common_chains::SEPOLIA_BERYL_ACTIVATION_ADMIN_ADDRESS)
        );
        assert_eq!(
            BaseChainSpec::zeronet().activation_admin_address(),
            Some(base_common_chains::ZERONET_BERYL_ACTIVATION_ADMIN_ADDRESS)
        );
    }

    #[test]
    fn activation_admin_is_unset_for_default_genesis() {
        assert_eq!(
            BaseChainSpec::from_genesis(Genesis::default()).activation_admin_address(),
            None
        );
    }

    #[test]
    fn activation_admin_can_be_read_from_genesis() {
        let mut genesis = Genesis::default();
        let admin = address!("0xcb00000000000000000000000000000000000000");
        genesis
            .config
            .extra_fields
            .insert("activationAdminAddress".to_string(), serde_json::json!(admin));

        assert_eq!(BaseChainSpec::from_genesis(genesis).activation_admin_address(), Some(admin));
    }

    #[test]
    fn beryl_genesis_without_activation_admin_is_rejected() {
        let chain_id = 987_654;
        let mut genesis = Genesis::default();
        genesis.config.chain_id = chain_id;
        genesis.config.extra_fields.insert("base".to_string(), serde_json::json!({ "beryl": 0 }));

        let err = BaseChainSpec::try_from_genesis(genesis)
            .expect_err("Beryl genesis without activation admin should be rejected");
        assert!(
            matches!(err, BaseChainSpecError::MissingActivationAdminAddress { chain_id: id } if id == chain_id)
        );
    }

    #[test]
    fn beryl_builder_without_activation_admin_is_rejected() {
        let chain_id = ChainConfig::mainnet().chain_id;
        let err = BaseChainSpecBuilder::base_mainnet()
            .optional_activation_admin_address(None)
            .beryl_activated()
            .try_build()
            .expect_err("Beryl builder without activation admin should be rejected");

        assert!(
            matches!(err, BaseChainSpecError::MissingActivationAdminAddress { chain_id: id } if id == chain_id)
        );
    }

    #[test]
    fn beryl_genesis_with_zero_activation_admin_is_rejected() {
        let chain_id = 987_654;
        let mut genesis = Genesis::default();
        genesis.config.chain_id = chain_id;
        genesis.config.extra_fields.insert("base".to_string(), serde_json::json!({ "beryl": 0 }));
        genesis.config.extra_fields.insert(
            "activationAdminAddress".to_string(),
            serde_json::json!("0x0000000000000000000000000000000000000000"),
        );

        let err = BaseChainSpec::try_from_genesis(genesis)
            .expect_err("Beryl genesis with zero activation admin should be rejected");
        assert!(
            matches!(err, BaseChainSpecError::ZeroActivationAdminAddress { chain_id: id } if id == chain_id)
        );
    }

    #[test]
    fn beryl_chain_config_without_known_activation_admin_is_rejected() {
        let mut config = ChainConfig::devnet().clone();
        config.chain_id = 987_654;
        config.beryl_timestamp = Some(0);

        let err = BaseChainSpec::try_from(&config)
            .expect_err("Beryl chain config without activation admin should be rejected");
        assert!(
            matches!(err, BaseChainSpecError::MissingActivationAdminAddress { chain_id } if chain_id == config.chain_id)
        );
    }

    #[test]
    fn beryl_builder_with_zero_activation_admin_is_rejected() {
        let chain_id = ChainConfig::mainnet().chain_id;
        let err = BaseChainSpecBuilder::base_mainnet()
            .optional_activation_admin_address(Some(Address::ZERO))
            .beryl_activated()
            .try_build()
            .expect_err("Beryl builder with zero activation admin should be rejected");

        assert!(
            matches!(err, BaseChainSpecError::ZeroActivationAdminAddress { chain_id: id } if id == chain_id)
        );
    }

    #[test]
    fn beryl_chainspec_can_be_built_with_activation_admin() {
        let admin = address!("0xcb00000000000000000000000000000000000000");
        let inner = ChainSpec::builder()
            .chain(987_654.into())
            .genesis(Genesis::default())
            .with_fork(BaseUpgrade::Beryl, ForkCondition::Timestamp(0))
            .build();

        let chain_spec = BaseChainSpec::try_from_chainspec(inner, Some(admin))
            .expect("Beryl chain spec with activation admin should build");

        assert_eq!(chain_spec.activation_admin_address(), Some(admin));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Beryl, 0));
    }

    #[test]
    fn base_sepolia_genesis() {
        let base_sepolia_spec = BaseChainSpec::sepolia();
        let genesis = base_sepolia_spec.genesis_header();
        assert_eq!(
            genesis.hash_slow(),
            b256!("0x0dcc9e089e30b90ddfc55be9a37dd15bc551aeee999d2e2b51414c54eaf934e4")
        );
        let base_fee = base_sepolia_spec.next_block_base_fee(genesis, genesis.timestamp).unwrap();
        assert_eq!(base_fee, 980000000);
    }

    #[test]
    fn base_zeronet_genesis() {
        let base_zeronet_spec = BaseChainSpec::zeronet();
        let genesis = base_zeronet_spec.genesis_header();
        assert_eq!(
            genesis.hash_slow(),
            b256!("0x1842d6ef4c40e2a4794458e167f6d327269df919b626979111c37ad3a96047bf")
        );
    }

    #[test]
    fn el_bootnodes_count_matches_config() {
        // `bootnodes()` must surface every EL entry from `ChainConfig.bootnodes.execution`.
        // A mismatch means `parse_nodes` silently dropped a malformed entry.
        for (spec, cfg) in [
            (BaseChainSpec::mainnet(), ChainConfig::mainnet()),
            (BaseChainSpec::sepolia(), ChainConfig::sepolia()),
            (BaseChainSpec::zeronet(), ChainConfig::zeronet()),
        ] {
            let parsed = spec.bootnodes().expect("known chain returns Some");
            assert_eq!(
                parsed.len(),
                cfg.bootnodes.execution.len(),
                "EL bootnode parse drop on chain {}",
                cfg.chain_id,
            );
        }
    }

    #[test]
    fn el_bootnodes_have_no_consensus_entries() {
        // The EL chainspec must never expose CL ENRs — they belong to a different
        // discv5 network (different protocol ID / port) and bricked discovery in the past.
        for (spec, cfg) in [
            (BaseChainSpec::mainnet(), ChainConfig::mainnet()),
            (BaseChainSpec::sepolia(), ChainConfig::sepolia()),
            (BaseChainSpec::zeronet(), ChainConfig::zeronet()),
        ] {
            assert!(
                cfg.bootnodes.execution.iter().all(|s| s.starts_with("enode://")),
                "non-enode entry in EL list for chain {}",
                cfg.chain_id,
            );
            let parsed = spec.bootnodes().unwrap();
            for record in &parsed {
                assert_ne!(record.tcp_port, 0, "EL bootnode missing TCP port: {record:?}");
                assert_ne!(record.udp_port, 0, "EL bootnode missing UDP port: {record:?}");
            }
        }
    }

    #[test]
    fn el_bootnodes_unknown_chain_returns_none() {
        let unknown = BaseChainSpecBuilder::base_mainnet()
            .chain(alloy_chains::Chain::from_id(99_999))
            .build();
        assert!(unknown.bootnodes().is_none());
    }

    #[test]
    fn latest_base_mainnet_fork_id() {
        let base_mainnet_spec = BaseChainSpec::mainnet();
        assert_eq!(
            base_mainnet_spec.hardfork_fork_id(BaseUpgrade::Beryl).unwrap(),
            base_mainnet_spec.latest_fork_id()
        )
    }

    #[test]
    fn latest_base_mainnet_fork_id_with_builder() {
        let base_mainnet_spec = BaseChainSpec::mainnet();
        let base_mainnet = BaseChainSpecBuilder::base_mainnet().build();
        assert_eq!(
            base_mainnet_spec.hardfork_fork_id(BaseUpgrade::Beryl).unwrap(),
            base_mainnet.latest_fork_id()
        )
    }

    #[test]
    fn parse_base_upgrades() {
        let geth_genesis = r#"
    {
      "config": {
        "bedrockBlock": 10,
        "regolithTime": 20,
        "canyonTime": 30,
        "ecotoneTime": 40,
        "fjordTime": 50,
        "graniteTime": 51,
        "holoceneTime": 52,
        "isthmusTime": 53,
        "jovianTime": 54,
        "base": {
          "v1": 55,
          "v2": 60
        },
        "activationAdminAddress": "0xcb00000000000000000000000000000000000000",
        "optimism": {
          "eip1559Elasticity": 60,
          "eip1559Denominator": 70
        }
      }
    }
    "#;
        let genesis: Genesis = serde_json::from_str(geth_genesis).unwrap();
        let chain_spec: BaseChainSpec = genesis.into();

        assert_eq!(
            chain_spec.base_fee_params,
            BaseFeeParamsKind::Constant(BaseFeeParams::new(70, 60))
        );

        assert!(!chain_spec.is_fork_active_at_block(BaseUpgrade::Bedrock, 0));
        assert!(!chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Regolith, 0));
        assert!(!chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Canyon, 0));
        assert!(!chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Ecotone, 0));
        assert!(!chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Fjord, 0));
        assert!(!chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Granite, 0));
        assert!(!chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Holocene, 0));

        assert!(chain_spec.is_fork_active_at_block(BaseUpgrade::Bedrock, 10));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Regolith, 20));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Canyon, 30));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Ecotone, 40));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Fjord, 50));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Granite, 51));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Holocene, 52));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Jovian, 54));
        assert!(!chain_spec.is_fork_active_at_timestamp(EthereumHardfork::Osaka, 54));
        assert!(chain_spec.is_fork_active_at_timestamp(EthereumHardfork::Osaka, 55));
        assert!(chain_spec.is_fork_active_at_timestamp(EthereumHardfork::Osaka, 98));
        assert!(!chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Azul, 54));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Azul, 55));
        assert!(!chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Beryl, 59));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Beryl, 60));
    }

    #[test]
    fn set_hardfork_activation_timestamp_updates_matching_eth_fork() {
        let mut chain_spec = BaseChainSpec::devnet();

        chain_spec.set_fork(EthereumHardfork::Osaka, ForkCondition::Never);
        chain_spec.set_fork(BaseUpgrade::Azul, ForkCondition::Never);
        chain_spec.set_fork(BaseUpgrade::Cobalt, ForkCondition::Never);
        assert!(chain_spec.set_hardfork_activation_timestamp(BaseUpgrade::Azul, 42));
        assert!(chain_spec.set_hardfork_activation_timestamp(BaseUpgrade::Cobalt, 84));

        assert_eq!(chain_spec.fork(EthereumHardfork::Osaka), ForkCondition::Timestamp(42));
        assert_eq!(chain_spec.fork(BaseUpgrade::Azul), ForkCondition::Timestamp(42));
        assert_eq!(chain_spec.fork(BaseUpgrade::Cobalt), ForkCondition::Timestamp(84));

        chain_spec.clear_hardfork_activation_timestamps();

        assert_eq!(chain_spec.fork(EthereumHardfork::Osaka), ForkCondition::Never);
        assert_eq!(chain_spec.fork(BaseUpgrade::Azul), ForkCondition::Never);
        assert_eq!(chain_spec.fork(BaseUpgrade::Cobalt), ForkCondition::Never);
    }

    #[test]
    fn set_hardfork_activation_timestamp_ignores_rollup_only_contract_ids() {
        let mut chain_spec = BaseChainSpec::devnet();
        let ecotone = chain_spec.fork(BaseUpgrade::Ecotone);

        assert!(!chain_spec.set_hardfork_activation_timestamp(BaseUpgrade::Delta, 42));
        assert!(
            !chain_spec.set_hardfork_activation_timestamp(BaseUpgrade::PectraBlobSchedule, 84,)
        );
        assert!(!chain_spec.clear_hardfork_activation_timestamp(BaseUpgrade::Delta));
        assert!(!chain_spec.clear_hardfork_activation_timestamp(BaseUpgrade::PectraBlobSchedule));

        assert_eq!(chain_spec.fork(BaseUpgrade::Ecotone), ecotone);
    }

    #[test]
    fn set_beryl_activation_timestamp_without_activation_admin_is_rejected() {
        let mut chain_spec = BaseChainSpec::from(ChainSpec::default());

        let err = chain_spec
            .try_set_hardfork_activation_timestamp(BaseUpgrade::Beryl, 42)
            .expect_err("Beryl schedule without activation admin should be rejected");

        assert!(matches!(err, BaseChainSpecError::MissingActivationAdminAddress { .. }));
        assert!(!chain_spec.set_hardfork_activation_timestamp(BaseUpgrade::Beryl, 42));
        assert_eq!(chain_spec.fork(BaseUpgrade::Beryl), ForkCondition::Never);
    }

    #[test]
    fn parse_base_hardforks_variable_base_fee_params() {
        let geth_genesis = r#"
    {
      "config": {
        "bedrockBlock": 10,
        "regolithTime": 20,
        "canyonTime": 30,
        "ecotoneTime": 40,
        "fjordTime": 50,
        "graniteTime": 51,
        "holoceneTime": 52,
        "isthmusTime": 53,
        "optimism": {
          "eip1559Elasticity": 60,
          "eip1559Denominator": 70,
          "eip1559DenominatorCanyon": 80
        }
      }
    }
    "#;
        let genesis: Genesis = serde_json::from_str(geth_genesis).unwrap();

        let actual_bedrock_block = genesis.config.extra_fields.get("bedrockBlock");
        assert_eq!(actual_bedrock_block, Some(serde_json::Value::from(10)).as_ref());
        let actual_regolith_timestamp = genesis.config.extra_fields.get("regolithTime");
        assert_eq!(actual_regolith_timestamp, Some(serde_json::Value::from(20)).as_ref());
        let actual_canyon_timestamp = genesis.config.extra_fields.get("canyonTime");
        assert_eq!(actual_canyon_timestamp, Some(serde_json::Value::from(30)).as_ref());
        let actual_ecotone_timestamp = genesis.config.extra_fields.get("ecotoneTime");
        assert_eq!(actual_ecotone_timestamp, Some(serde_json::Value::from(40)).as_ref());
        let actual_fjord_timestamp = genesis.config.extra_fields.get("fjordTime");
        assert_eq!(actual_fjord_timestamp, Some(serde_json::Value::from(50)).as_ref());
        let actual_granite_timestamp = genesis.config.extra_fields.get("graniteTime");
        assert_eq!(actual_granite_timestamp, Some(serde_json::Value::from(51)).as_ref());
        let actual_holocene_timestamp = genesis.config.extra_fields.get("holoceneTime");
        assert_eq!(actual_holocene_timestamp, Some(serde_json::Value::from(52)).as_ref());
        let actual_isthmus_timestamp = genesis.config.extra_fields.get("isthmusTime");
        assert_eq!(actual_isthmus_timestamp, Some(serde_json::Value::from(53)).as_ref());

        let base_fee_object = genesis.config.extra_fields.get("optimism").unwrap();
        assert_eq!(
            base_fee_object,
            &serde_json::json!({
                "eip1559Elasticity": 60,
                "eip1559Denominator": 70,
                "eip1559DenominatorCanyon": 80
            })
        );

        let chain_spec: BaseChainSpec = genesis.into();

        assert_eq!(
            chain_spec.base_fee_params,
            BaseFeeParamsKind::Variable(
                vec![
                    (EthereumHardfork::London.boxed(), BaseFeeParams::new(70, 60)),
                    (BaseUpgrade::Canyon.boxed(), BaseFeeParams::new(80, 60)),
                ]
                .into()
            )
        );

        assert!(!chain_spec.is_fork_active_at_block(BaseUpgrade::Bedrock, 0));
        assert!(!chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Regolith, 0));
        assert!(!chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Canyon, 0));
        assert!(!chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Ecotone, 0));
        assert!(!chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Fjord, 0));
        assert!(!chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Granite, 0));
        assert!(!chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Holocene, 0));

        assert!(chain_spec.is_fork_active_at_block(BaseUpgrade::Bedrock, 10));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Regolith, 20));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Canyon, 30));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Ecotone, 40));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Fjord, 50));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Granite, 51));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Holocene, 52));
    }

    #[test]
    fn parse_genesis_base_with_variable_base_fee_params() {
        let geth_genesis = r#"
    {
      "config": {
        "chainId": 8453,
        "homesteadBlock": 0,
        "eip150Block": 0,
        "eip155Block": 0,
        "eip158Block": 0,
        "byzantiumBlock": 0,
        "constantinopleBlock": 0,
        "petersburgBlock": 0,
        "istanbulBlock": 0,
        "muirGlacierBlock": 0,
        "berlinBlock": 0,
        "londonBlock": 0,
        "arrowGlacierBlock": 0,
        "grayGlacierBlock": 0,
        "mergeNetsplitBlock": 0,
        "bedrockBlock": 0,
        "regolithTime": 15,
        "terminalTotalDifficulty": 0,
        "terminalTotalDifficultyPassed": true,
        "optimism": {
          "eip1559Elasticity": 6,
          "eip1559Denominator": 50
        }
      }
    }
    "#;
        let genesis: Genesis = serde_json::from_str(geth_genesis).unwrap();
        let chainspec = BaseChainSpec::from(genesis.clone());

        let actual_chain_id = genesis.config.chain_id;
        assert_eq!(actual_chain_id, 8453);

        assert_eq!(
            chainspec.hardforks.get(EthereumHardfork::Istanbul),
            Some(ForkCondition::Block(0))
        );

        let actual_bedrock_block = genesis.config.extra_fields.get("bedrockBlock");
        assert_eq!(actual_bedrock_block, Some(serde_json::Value::from(0)).as_ref());
        let actual_canyon_timestamp = genesis.config.extra_fields.get("canyonTime");
        assert_eq!(actual_canyon_timestamp, None);

        assert!(genesis.config.terminal_total_difficulty_passed);

        let base_fee_object = genesis.config.extra_fields.get("optimism").unwrap();
        let base_fee_info = serde_json::from_value::<FeeInfo>(base_fee_object.clone()).unwrap();

        assert_eq!(
            base_fee_info,
            FeeInfo {
                eip1559_elasticity: Some(6),
                eip1559_denominator: Some(50),
                eip1559_denominator_canyon: None,
            }
        );
        assert_eq!(
            chainspec.base_fee_params,
            BaseFeeParamsKind::Constant(BaseFeeParams {
                max_change_denominator: 50,
                elasticity_multiplier: 6,
            })
        );

        assert!(chainspec.is_fork_active_at_block(BaseUpgrade::Bedrock, 0));
        assert!(chainspec.is_fork_active_at_timestamp(BaseUpgrade::Regolith, 20));
    }

    #[test]
    fn test_fork_order_base_upgrades() {
        let genesis = Genesis {
            config: AlloyChainConfig {
                chain_id: 0,
                homestead_block: Some(0),
                dao_fork_block: Some(0),
                dao_fork_support: false,
                eip150_block: Some(0),
                eip155_block: Some(0),
                eip158_block: Some(0),
                byzantium_block: Some(0),
                constantinople_block: Some(0),
                petersburg_block: Some(0),
                istanbul_block: Some(0),
                muir_glacier_block: Some(0),
                berlin_block: Some(0),
                london_block: Some(0),
                arrow_glacier_block: Some(0),
                gray_glacier_block: Some(0),
                merge_netsplit_block: Some(0),
                shanghai_time: Some(0),
                cancun_time: Some(0),
                prague_time: Some(0),
                osaka_time: Some(0),
                terminal_total_difficulty: Some(U256::ZERO),
                extra_fields: [
                    (String::from("bedrockBlock"), 0.into()),
                    (String::from("regolithTime"), 0.into()),
                    (String::from("canyonTime"), 0.into()),
                    (String::from("ecotoneTime"), 0.into()),
                    (String::from("fjordTime"), 0.into()),
                    (String::from("graniteTime"), 0.into()),
                    (String::from("holoceneTime"), 0.into()),
                    (String::from("isthmusTime"), 0.into()),
                    (String::from("jovianTime"), 0.into()),
                    (String::from("base"), serde_json::json!({ "v1": 0, "v2": 0, "v3": 0 })),
                    (
                        String::from("activationAdminAddress"),
                        serde_json::json!(address!("0xcb00000000000000000000000000000000000000")),
                    ),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            },
            ..Default::default()
        };

        let chain_spec: BaseChainSpec = genesis.into();

        let upgrades: Vec<_> = chain_spec.hardforks.forks_iter().map(|(h, _)| h).collect();
        let expected_upgrades = vec![
            EthereumHardfork::Frontier.boxed(),
            EthereumHardfork::Homestead.boxed(),
            EthereumHardfork::Tangerine.boxed(),
            EthereumHardfork::SpuriousDragon.boxed(),
            EthereumHardfork::Byzantium.boxed(),
            EthereumHardfork::Constantinople.boxed(),
            EthereumHardfork::Petersburg.boxed(),
            EthereumHardfork::Istanbul.boxed(),
            EthereumHardfork::MuirGlacier.boxed(),
            EthereumHardfork::Berlin.boxed(),
            EthereumHardfork::London.boxed(),
            EthereumHardfork::ArrowGlacier.boxed(),
            EthereumHardfork::GrayGlacier.boxed(),
            EthereumHardfork::Paris.boxed(),
            BaseUpgrade::Bedrock.boxed(),
            BaseUpgrade::Regolith.boxed(),
            EthereumHardfork::Shanghai.boxed(),
            BaseUpgrade::Canyon.boxed(),
            EthereumHardfork::Cancun.boxed(),
            BaseUpgrade::Ecotone.boxed(),
            BaseUpgrade::Fjord.boxed(),
            BaseUpgrade::Granite.boxed(),
            BaseUpgrade::Holocene.boxed(),
            EthereumHardfork::Prague.boxed(),
            BaseUpgrade::Isthmus.boxed(),
            BaseUpgrade::Jovian.boxed(),
            EthereumHardfork::Osaka.boxed(),
            BaseUpgrade::Azul.boxed(),
            BaseUpgrade::Beryl.boxed(),
            BaseUpgrade::Cobalt.boxed(),
        ];

        for (expected, actual) in expected_upgrades.iter().zip(upgrades.iter()) {
            assert_eq!(&**expected, &**actual);
        }
        assert_eq!(expected_upgrades.len(), upgrades.len());
    }

    #[test]
    fn json_genesis() {
        let geth_genesis = r#"
{
    "config": {
        "chainId": 1301,
        "homesteadBlock": 0,
        "eip150Block": 0,
        "eip155Block": 0,
        "eip158Block": 0,
        "byzantiumBlock": 0,
        "constantinopleBlock": 0,
        "petersburgBlock": 0,
        "istanbulBlock": 0,
        "muirGlacierBlock": 0,
        "berlinBlock": 0,
        "londonBlock": 0,
        "arrowGlacierBlock": 0,
        "grayGlacierBlock": 0,
        "mergeNetsplitBlock": 0,
        "shanghaiTime": 0,
        "cancunTime": 0,
        "bedrockBlock": 0,
        "regolithTime": 0,
        "canyonTime": 0,
        "ecotoneTime": 0,
        "fjordTime": 0,
        "graniteTime": 0,
        "holoceneTime": 1732633200,
        "terminalTotalDifficulty": 0,
        "terminalTotalDifficultyPassed": true,
        "optimism": {
            "eip1559Elasticity": 6,
            "eip1559Denominator": 50,
            "eip1559DenominatorCanyon": 250
        }
    },
    "nonce": "0x0",
    "timestamp": "0x66edad4c",
    "extraData": "0x424544524f434b",
    "gasLimit": "0x1c9c380",
    "difficulty": "0x0",
    "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "coinbase": "0x4200000000000000000000000000000000000011",
    "alloc": {},
    "number": "0x0",
    "gasUsed": "0x0",
    "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "baseFeePerGas": "0x3b9aca00",
    "excessBlobGas": "0x0",
    "blobGasUsed": "0x0"
}
        "#;

        let genesis: Genesis = serde_json::from_str(geth_genesis).unwrap();
        let chainspec = BaseChainSpec::from_genesis(genesis);
        assert!(Upgrades::is_holocene_active_at_timestamp(&chainspec, 1732633200));
    }

    #[test]
    fn json_genesis_mapped_l1_timestamps() {
        let geth_genesis = r#"
{
    "config": {
        "chainId": 1301,
        "homesteadBlock": 0,
        "eip150Block": 0,
        "eip155Block": 0,
        "eip158Block": 0,
        "byzantiumBlock": 0,
        "constantinopleBlock": 0,
        "petersburgBlock": 0,
        "istanbulBlock": 0,
        "muirGlacierBlock": 0,
        "berlinBlock": 0,
        "londonBlock": 0,
        "arrowGlacierBlock": 0,
        "grayGlacierBlock": 0,
        "mergeNetsplitBlock": 0,
        "bedrockBlock": 0,
        "regolithTime": 0,
        "canyonTime": 0,
        "ecotoneTime": 1712633200,
        "fjordTime": 0,
        "graniteTime": 0,
        "holoceneTime": 1732633200,
        "isthmusTime": 1742633200,
        "terminalTotalDifficulty": 0,
        "terminalTotalDifficultyPassed": true,
        "optimism": {
            "eip1559Elasticity": 6,
            "eip1559Denominator": 50,
            "eip1559DenominatorCanyon": 250
        }
    },
    "nonce": "0x0",
    "timestamp": "0x66edad4c",
    "extraData": "0x424544524f434b",
    "gasLimit": "0x1c9c380",
    "difficulty": "0x0",
    "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "coinbase": "0x4200000000000000000000000000000000000011",
    "alloc": {},
    "number": "0x0",
    "gasUsed": "0x0",
    "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "baseFeePerGas": "0x3b9aca00",
    "excessBlobGas": "0x0",
    "blobGasUsed": "0x0"
}
        "#;

        let genesis: Genesis = serde_json::from_str(geth_genesis).unwrap();
        let chainspec = BaseChainSpec::from_genesis(genesis);
        assert!(chainspec.is_holocene_active_at_timestamp(1732633200));

        assert!(chainspec.is_shanghai_active_at_timestamp(0));
        assert!(chainspec.is_canyon_active_at_timestamp(0));

        assert!(chainspec.is_ecotone_active_at_timestamp(1712633200));
        assert!(chainspec.is_cancun_active_at_timestamp(1712633200));

        assert!(chainspec.is_prague_active_at_timestamp(1742633200));
        assert!(chainspec.is_isthmus_active_at_timestamp(1742633200));
    }

    #[test]
    fn display_hardorks() {
        let content = BaseChainSpec::mainnet().display_hardforks().to_string();
        for eth_hf in EthereumHardfork::VARIANTS {
            assert!(!content.contains(eth_hf.name()));
        }
    }
}
