use alloc::{sync::Arc, vec::Vec};

use alloy_chains::Chain;
use alloy_consensus::{BlockHeader, EMPTY_ROOT_HASH, Header, proofs::storage_root_unhashed};
use alloy_eip2124::{ForkFilter, ForkId, Head};
use alloy_eips::{
    eip1559::{BaseFeeParams, INITIAL_BASE_FEE},
    eip7840::BlobParams,
};
use alloy_genesis::Genesis;
use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};
use alloy_primitives::{Address, B256};
use base_common_chains::{ChainConfig, ChainUpgrades, ExecutionFork, Upgrades};
use base_common_consensus::Predeploys;
use base_common_genesis::{BaseUpgrade, FeeConfig, UpgradeActivation, UpgradeActivationSink};
use base_protocol::OutputRoot;
use reth_network_peers::{NodeRecord, parse_nodes};
use reth_primitives_traits::SealedHeader;

use crate::{compute_jovian_base_fee, decode_holocene_base_fee};

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
#[derive(Debug)]
pub struct GenesisInfo {
    /// Base chain info extracted from genesis extra fields.
    pub base_chain_info: base_common_rpc_types::ChainInfo,
    /// Base fee params derived from the genesis config.
    pub fee_config: FeeConfig,
}

impl GenesisInfo {
    /// Extracts fee and upgrade boundary fields from a genesis document.
    pub fn extract_from(genesis: &Genesis) -> Self {
        let base_chain_info =
            base_common_rpc_types::ChainInfo::extract_from(&genesis.config.extra_fields)
                .unwrap_or_default();
        let mut fee_config = FeeConfig {
            eip1559_elasticity: 2,
            eip1559_denominator: 8,
            eip1559_denominator_canyon: 8,
        };
        if let Some(info) = &base_chain_info.base_fee_info {
            if let (Some(elasticity), Some(denominator)) =
                (info.eip1559_elasticity, info.eip1559_denominator)
            {
                fee_config = FeeConfig {
                    eip1559_elasticity: elasticity,
                    eip1559_denominator: denominator,
                    eip1559_denominator_canyon: info
                        .eip1559_denominator_canyon
                        .unwrap_or(denominator),
                };
            }
        }
        Self { base_chain_info, fee_config }
    }
}

/// Base chain spec type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseChainSpec {
    /// Canonical configuration shared by execution and consensus.
    pub config: ChainConfig,

    /// The genesis block.
    pub genesis: Genesis,

    /// The header corresponding to the genesis block.
    pub genesis_header: SealedHeader,
}

impl Default for BaseChainSpec {
    fn default() -> Self {
        Self {
            config: ChainConfig::default(),
            genesis: Genesis::default(),
            genesis_header: Default::default(),
        }
    }
}

impl BaseChainSpec {
    pub fn chain(&self) -> Chain {
        Chain::from_id(self.config.chain_id)
    }

    pub const fn genesis(&self) -> &Genesis {
        &self.genesis
    }

    pub fn genesis_header(&self) -> &Header {
        &self.genesis_header
    }

    pub fn sealed_genesis_header(&self) -> SealedHeader {
        SealedHeader::new(self.genesis_header().clone(), self.genesis_hash())
    }

    pub fn initial_base_fee(&self) -> Option<u64> {
        // If the base fee is set in the genesis block, we use that instead of the default.
        let genesis_base_fee =
            self.genesis.base_fee_per_gas.map(|fee| fee as u64).unwrap_or(INITIAL_BASE_FEE);

        // If London is activated at genesis, we set the initial base fee as per EIP-1559.
        self.config
            .upgrades
            .fork(EthereumHardfork::London)
            .active_at_block(0)
            .then_some(genesis_base_fee)
    }

    pub fn genesis_hash(&self) -> B256 {
        self.genesis_header.hash()
    }

    pub const fn genesis_timestamp(&self) -> u64 {
        self.genesis.timestamp
    }

    pub fn chain_id(&self) -> u64 {
        self.chain().id()
    }
}

impl BaseChainSpec {
    /// Tests the runtime-aware condition at a block number.
    pub fn is_fork_active_at_block<H: Into<ExecutionFork>>(&self, fork: H, block: u64) -> bool {
        self.fork(fork).active_at_block(block)
    }

    /// Tests the runtime-aware condition at a timestamp.
    pub fn is_fork_active_at_timestamp<H: Into<ExecutionFork>>(
        &self,
        fork: H,
        timestamp: u64,
    ) -> bool {
        self.fork(fork).active_at_timestamp(timestamp)
    }

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

        let upgrades = ChainUpgrades::new([
            (
                BaseUpgrade::Bedrock,
                genesis_info.bedrock_block.map(ForkCondition::Block).unwrap_or_default(),
            ),
            (
                BaseUpgrade::Regolith,
                genesis_info.regolith_time.map(ForkCondition::Timestamp).unwrap_or_default(),
            ),
            (
                BaseUpgrade::Canyon,
                genesis_info.canyon_time.map(ForkCondition::Timestamp).unwrap_or_default(),
            ),
            (
                BaseUpgrade::Ecotone,
                genesis_info.ecotone_time.map(ForkCondition::Timestamp).unwrap_or_default(),
            ),
            (
                BaseUpgrade::Fjord,
                genesis_info.fjord_time.map(ForkCondition::Timestamp).unwrap_or_default(),
            ),
            (
                BaseUpgrade::Granite,
                genesis_info.granite_time.map(ForkCondition::Timestamp).unwrap_or_default(),
            ),
            (
                BaseUpgrade::Holocene,
                genesis_info.holocene_time.map(ForkCondition::Timestamp).unwrap_or_default(),
            ),
            (
                BaseUpgrade::Isthmus,
                genesis_info.isthmus_time.map(ForkCondition::Timestamp).unwrap_or_default(),
            ),
            (
                BaseUpgrade::Jovian,
                genesis_info.jovian_time.map(ForkCondition::Timestamp).unwrap_or_default(),
            ),
            (
                BaseUpgrade::Azul,
                genesis_info.base.azul.map(ForkCondition::Timestamp).unwrap_or_default(),
            ),
            (
                BaseUpgrade::Beryl,
                genesis_info.base.beryl.map(ForkCondition::Timestamp).unwrap_or_default(),
            ),
            (
                BaseUpgrade::Cobalt,
                genesis_info.base.cobalt.map(ForkCondition::Timestamp).unwrap_or_default(),
            ),
            (
                BaseUpgrade::Denim,
                genesis_info.base.denim.map(ForkCondition::Timestamp).unwrap_or_default(),
            ),
            (
                BaseUpgrade::Zenith,
                genesis_info.base.zenith.map(ForkCondition::Timestamp).unwrap_or_default(),
            ),
        ]);
        let chain_id = genesis.config.chain_id;
        Self::validate_beryl_activation_admin(&upgrades, activation_admin_address, chain_id)?;
        let genesis_header =
            SealedHeader::seal_slow(Self::make_genesis_header(&genesis, &upgrades));

        Ok(Self {
            config: ChainConfig {
                chain_id,
                upgrades,
                fee_config: base_genesis_info.fee_config,
                activation_admin_address,
                genesis: base_common_genesis::ChainGenesis {
                    l2: alloy_eips::BlockNumHash {
                        number: genesis_header.number,
                        hash: genesis_header.hash(),
                    },
                    l2_time: genesis.timestamp,
                    ..ChainConfig::by_chain_id(chain_id)
                        .map(|config| config.genesis)
                        .unwrap_or_default()
                },
                ..ChainConfig::by_chain_id(chain_id).cloned().unwrap_or_default()
            },
            genesis_header,
            genesis,
            ..Default::default()
        })
    }

    /// Validates that Beryl-enabled chains have a valid activation registry admin address:
    /// present (not `None`) and non-zero.
    pub fn validate_beryl_activation_admin(
        upgrades: &ChainUpgrades,
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
    /// Applies the Base execution rules, including Isthmus withdrawals-root logic: if Isthmus is active at the genesis timestamp, the withdrawals root is set to the
    /// storage root of the `L2ToL1MessagePasser` predeploy.
    pub fn make_genesis_header(genesis: &Genesis, upgrades: &ChainUpgrades) -> Header {
        let timestamp = genesis.timestamp;
        let cancun = upgrades.fork(BaseUpgrade::Ecotone).active_at_timestamp(timestamp);
        let mut header = Header {
            number: genesis.number.unwrap_or_default(),
            parent_hash: genesis.parent_hash.unwrap_or_default(),
            gas_limit: genesis.gas_limit,
            difficulty: genesis.difficulty,
            nonce: genesis.nonce.into(),
            extra_data: genesis.extra_data.clone(),
            state_root: alloy_trie::root::state_root_ref_unhashed(&genesis.alloc),
            timestamp,
            mix_hash: genesis.mix_hash,
            beneficiary: genesis.coinbase,
            base_fee_per_gas: upgrades.fork(BaseUpgrade::Bedrock).active_at_block(0).then(|| {
                genesis.base_fee_per_gas.map(|fee| fee as u64).unwrap_or(INITIAL_BASE_FEE)
            }),
            withdrawals_root: upgrades
                .fork(BaseUpgrade::Canyon)
                .active_at_timestamp(timestamp)
                .then_some(alloy_consensus::constants::EMPTY_WITHDRAWALS),
            parent_beacon_block_root: cancun.then_some(B256::ZERO),
            blob_gas_used: cancun.then_some(genesis.blob_gas_used.unwrap_or(0)),
            excess_blob_gas: cancun.then_some(genesis.excess_blob_gas.unwrap_or(0)),
            requests_hash: upgrades
                .fork(BaseUpgrade::Isthmus)
                .active_at_timestamp(timestamp)
                .then_some(alloy_eips::eip7685::EMPTY_REQUESTS_HASH),
            ..Default::default()
        };

        if upgrades.fork(BaseUpgrade::Isthmus).active_at_timestamp(header.timestamp)
            && let Some(storage_root) = Self::l2_to_l1_message_passer_storage_root(genesis)
        {
            header.withdrawals_root = Some(storage_root);
        }

        header
    }

    /// Computes the storage root of the genesis `L2ToL1MessagePasser`, if configured.
    pub fn l2_to_l1_message_passer_storage_root(genesis: &Genesis) -> Option<B256> {
        genesis
            .alloc
            .get(&Predeploys::L2_TO_L1_MESSAGE_PASSER)
            .and_then(|account| account.storage.as_ref())
            .map(|storage| {
                storage_root_unhashed(storage.iter().filter_map(|(key, value)| {
                    if value.is_zero() { None } else { Some((*key, (*value).into())) }
                }))
            })
    }

    /// Computes the V0 output root for this chain's genesis block.
    pub fn genesis_output_root(&self) -> B256 {
        let bridge_storage_root =
            Self::l2_to_l1_message_passer_storage_root(&self.genesis).unwrap_or(EMPTY_ROOT_HASH);

        OutputRoot::from_parts(
            self.genesis_header().state_root,
            bridge_storage_root,
            self.genesis_hash(),
        )
        .hash()
    }

    /// Parses a chain name into an [`BaseChainSpec`], if recognized.
    pub fn parse_chain(s: &str) -> Option<Arc<Self>> {
        let cfg = ChainConfig::by_name(s)?;
        Some(Arc::new(
            Self::try_from(cfg).expect("recognized Base chain config must build a valid chainspec"),
        ))
    }

    /// Activates or updates the given upgrade condition in-place.
    pub fn set_fork(&mut self, fork: BaseUpgrade, condition: ForkCondition) {
        self.config.upgrades.insert(fork, condition);
    }

    /// Returns the runtime-aware activation condition for a hardfork.
    pub fn fork(&self, fork: impl Into<ExecutionFork>) -> ForkCondition {
        self.config.upgrades.activation(self.config.chain_id, fork)
    }

    /// Takes a consistent snapshot of configured and runtime upgrade conditions.
    pub fn schedule(&self) -> ChainUpgrades {
        self.config.upgrades.runtime(self.config.chain_id)
    }

    /// Iterates over all active or scheduled execution rules.
    pub fn forks_iter(&self) -> impl Iterator<Item = (ExecutionFork, ForkCondition)> {
        self.schedule().forks_iter().collect::<Vec<_>>().into_iter()
    }

    /// Returns the runtime-aware fork ID for the given head.
    pub fn fork_id(&self, head: &Head) -> ForkId {
        self.fork_filter(*head).current()
    }

    /// Returns the fork ID after every scheduled upgrade.
    pub fn latest_fork_id(&self) -> ForkId {
        self.fork_id(&Head { number: u64::MAX, timestamp: u64::MAX, ..Default::default() })
    }

    /// Builds the peer fork filter from one runtime schedule snapshot.
    pub fn fork_filter(&self, head: Head) -> ForkFilter {
        let schedule = self.schedule();
        let forks = schedule.forks_iter().filter_map(|(_, condition)| match condition {
            ForkCondition::Block(block) | ForkCondition::TTD { fork_block: Some(block), .. } => {
                Some(alloy_eip2124::ForkFilterKey::Block(block))
            }
            ForkCondition::Timestamp(timestamp) => {
                Some(alloy_eip2124::ForkFilterKey::Time(timestamp))
            }
            _ => None,
        });
        ForkFilter::new(head, self.genesis_hash(), self.genesis_timestamp(), forks)
    }

    /// Returns the fork ID at the activation of an execution rule.
    pub fn hardfork_fork_id(&self, fork: impl Into<ExecutionFork>) -> Option<ForkId> {
        let head = match self.fork(fork) {
            ForkCondition::Never => return None,
            ForkCondition::Block(number) => Head { number, ..Default::default() },
            ForkCondition::Timestamp(timestamp) => {
                Head { number: u64::MAX, timestamp, ..Default::default() }
            }
            ForkCondition::TTD { fork_block, .. } => {
                Head { number: fork_block.unwrap_or_default(), ..Default::default() }
            }
        };
        Some(self.fork_id(&head))
    }

    /// Recomputes the sealed genesis header from the current genesis and hardfork schedule.
    pub fn refresh_genesis_header(&mut self) {
        self.genesis_header = SealedHeader::seal_slow(Self::make_genesis_header(
            &self.genesis,
            &self.config.upgrades,
        ));
        self.config.genesis.l2 = alloy_eips::BlockNumHash {
            number: self.genesis_header.number,
            hash: self.genesis_header.hash(),
        };
        self.config.genesis.l2_time = self.genesis.timestamp;
    }

    /// Clears all timestamp-based Base hardfork activation conditions.
    pub fn clear_hardfork_activation_timestamps(&mut self) {
        for hardfork_id in BaseUpgrade::CONTRACT_VARIANTS {
            Self::set_hardfork_activation_condition_for(
                &mut self.config.upgrades,
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
        let mut hardforks = self.config.upgrades.clone();
        if !Self::set_hardfork_activation_condition_for(&mut hardforks, hardfork_id, condition) {
            return Ok(false);
        }

        Self::validate_beryl_activation_admin(
            &hardforks,
            self.config.activation_admin_address,
            self.config.chain_id,
        )?;
        self.config.upgrades = hardforks;

        Ok(true)
    }

    /// Sets a hardfork activation condition by contract hardfork ID on a hardfork collection.
    pub fn set_hardfork_activation_condition_for(
        hardforks: &mut ChainUpgrades,
        hardfork_id: BaseUpgrade,
        condition: ForkCondition,
    ) -> bool {
        if !hardfork_id.is_execution() {
            return false;
        }
        hardforks.insert(hardfork_id, condition);
        true
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

    fn finalize(&mut self) -> Result<bool, Self::Error> {
        self.refresh_genesis_header();
        Ok(true)
    }
}

impl TryFrom<&ChainConfig> for BaseChainSpec {
    type Error = BaseChainSpecError;
    fn try_from(cfg: &ChainConfig) -> Result<Self, Self::Error> {
        Self::try_from_config_and_genesis(cfg.clone(), serde_json::from_str(cfg.genesis_json)?)
    }
}

impl BaseChainSpec {
    /// Derives execution metadata once from canonical configuration and a parsed genesis boundary.
    pub fn try_from_config_and_genesis(
        mut cfg: ChainConfig,
        genesis: Genesis,
    ) -> Result<Self, BaseChainSpecError> {
        let upgrades = &cfg.upgrades;
        let activation_admin_address = cfg.activation_admin_address;
        Self::validate_beryl_activation_admin(&upgrades, activation_admin_address, cfg.chain_id)?;
        let genesis_header = match cfg.genesis.l2.hash {
            B256::ZERO => SealedHeader::seal_slow(Self::make_genesis_header(&genesis, &upgrades)),
            hash => SealedHeader::new(Self::make_genesis_header(&genesis, &upgrades), hash),
        };

        cfg.genesis.l2 =
            alloy_eips::BlockNumHash { number: genesis_header.number, hash: genesis_header.hash() };
        cfg.genesis.l2_time = genesis_header.timestamp;
        Ok(Self { config: cfg, genesis_header, genesis, ..Default::default() })
    }
}

impl BaseChainSpec {
    /// Get the [`BaseFeeParams`] for the chain at the given timestamp.
    pub fn base_fee_params_at_timestamp(&self, timestamp: u64) -> BaseFeeParams {
        if self.fork(BaseUpgrade::Canyon).active_at_timestamp(timestamp) {
            self.config.fee_config.post_canyon_params()
        } else {
            self.config.fee_config.pre_canyon_params()
        }
    }

    /// Get the [`BlobParams`] for the given timestamp
    pub fn blob_params_at_timestamp(&self, timestamp: u64) -> Option<BlobParams> {
        let schedule = self.schedule();
        if schedule.fork(BaseUpgrade::Azul).active_at_timestamp(timestamp) {
            Some(BlobParams::osaka())
        } else if schedule.fork(BaseUpgrade::Isthmus).active_at_timestamp(timestamp) {
            Some(BlobParams::prague())
        } else if schedule.fork(BaseUpgrade::Ecotone).active_at_timestamp(timestamp) {
            Some(BlobParams::cancun())
        } else {
            None
        }
    }

    /// Returns a string representation of the hardforks.
    pub fn display_hardforks(&self) -> crate::UpgradeDisplay {
        crate::UpgradeDisplay(self.schedule())
    }

    /// The bootnodes for the chain, if any.
    pub fn bootnodes(&self) -> Option<Vec<NodeRecord>> {
        (!self.config.bootnodes.execution.is_empty())
            .then(|| parse_nodes(self.config.bootnodes.execution))
    }

    /// Computes the next block base fee using the active Base upgrade rules.
    pub fn next_block_base_fee(&self, parent: &Header, target_timestamp: u64) -> Option<u64> {
        if Upgrades::is_jovian_active_at_timestamp(self, parent.timestamp()) {
            compute_jovian_base_fee(self, parent, target_timestamp).ok()
        } else if Upgrades::is_holocene_active_at_timestamp(self, parent.timestamp()) {
            decode_holocene_base_fee(self, parent, target_timestamp).ok()
        } else {
            parent.next_block_base_fee(self.base_fee_params_at_timestamp(target_timestamp))
        }
    }
}

impl EthereumHardforks for BaseChainSpec {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self.fork(fork)
    }
}

impl Upgrades for BaseChainSpec {
    fn fork_condition(&self, fork: BaseUpgrade) -> ForkCondition {
        self.fork(fork)
    }

    fn activation_admin_address(&self) -> Option<Address> {
        self.config.activation_admin_address
    }
}

impl From<Genesis> for BaseChainSpec {
    fn from(genesis: Genesis) -> Self {
        Self::from_genesis(genesis)
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
    use alloy_eip2124::{ForkHash, ForkId, Head};
    use alloy_genesis::{ChainConfig as AlloyChainConfig, Genesis};
    use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};
    use alloy_primitives::{Address, B256, U256, address, b256};
    use base_common_chains::{ChainConfig, Upgrades};
    use base_common_genesis::{BaseUpgrade, RuntimeUpgradeRegistry};
    use base_common_rpc_types::FeeInfo;

    use crate::{BaseChainSpec, BaseChainSpecBuilder, BaseChainSpecError, GenesisInfo};

    fn test_fork_ids(spec: &BaseChainSpec, cases: &[(Head, ForkId)]) {
        for (head, expected) in cases {
            assert_eq!(spec.fork_id(head), *expected);
        }
    }

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
    fn devnet_genesis_output_root() {
        assert_eq!(
            BaseChainSpec::devnet().genesis_output_root(),
            b256!("14dabde8a7b90e1c258c03b239c69567baf19bad7eccf4e59c6c720e6787e7fe")
        );
    }

    #[test]
    fn base_mainnet_forkids() {
        let base_mainnet_spec = BaseChainSpec::mainnet();
        let mut base_mainnet = BaseChainSpecBuilder::base_mainnet().build();
        base_mainnet.genesis_header.set_hash(base_mainnet_spec.genesis_hash());
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
                        next: ChainConfig::mainnet().upgrades
                            [base_common_chains::BaseUpgrade::Jovian]
                            .as_timestamp()
                            .unwrap_or_default(),
                    },
                ),
                (
                    Head {
                        number: 0,
                        timestamp: ChainConfig::mainnet().upgrades
                            [base_common_chains::BaseUpgrade::Jovian]
                            .as_timestamp()
                            .unwrap_or_default(),
                        ..Default::default()
                    },
                    base_mainnet_spec.hardfork_fork_id(BaseUpgrade::Jovian).unwrap(),
                ),
                (
                    Head {
                        number: 0,
                        timestamp: ChainConfig::mainnet().upgrades
                            [base_common_chains::BaseUpgrade::Azul]
                            .as_timestamp()
                            .unwrap(),
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
                        next: ChainConfig::sepolia().upgrades
                            [base_common_chains::BaseUpgrade::Jovian]
                            .as_timestamp()
                            .unwrap_or_default(),
                    },
                ),
                (
                    Head {
                        number: 0,
                        timestamp: ChainConfig::sepolia().upgrades
                            [base_common_chains::BaseUpgrade::Jovian]
                            .as_timestamp()
                            .unwrap_or_default(),
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
            .with_fork(base_common_genesis::BaseUpgrade::Azul, ForkCondition::Never)
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
    fn forks_iter_surfaces_runtime_scheduled_absent_fork() {
        let chain_id = 9_100_007;
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        let spec = BaseChainSpecBuilder::default()
            .chain(Chain::from_id(chain_id))
            .genesis(Genesis::default())
            .build();
        let chain_id = spec.chain().id();
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let cobalt_condition = |spec: &BaseChainSpec| {
            spec.forks_iter().find_map(|(fork, condition)| {
                (fork == base_common_chains::ExecutionFork::Base(BaseUpgrade::Cobalt))
                    .then_some(condition)
            })
        };

        // Cobalt is unscheduled at startup, so it is absent from the enumeration.
        assert_eq!(cobalt_condition(&spec), None);

        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Cobalt, 84);

        // Once scheduled at runtime it must appear, matching `fork()`/`schedule()`.
        assert_eq!(cobalt_condition(&spec), Some(ForkCondition::Timestamp(84)));
        assert_eq!(spec.fork(BaseUpgrade::Cobalt), ForkCondition::Timestamp(84));

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn runtime_registry_overrides_execution_fork_ids() {
        let chain_id = 9_100_004;
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        let spec = BaseChainSpecBuilder::default()
            .chain(Chain::from_id(chain_id))
            .genesis(Genesis::default())
            .with_fork(base_common_genesis::BaseUpgrade::Azul, ForkCondition::Never)
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
    fn execution_and_rollup_respect_explicit_runtime_deactivation() {
        let chain_id = 9_100_010;
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        let mut config = ChainConfig::mainnet().clone();
        config.chain_id = chain_id;
        let spec = BaseChainSpec::try_from(&config).unwrap();
        let rollup = config.rollup_config();
        assert!(spec.is_shanghai_active_at_timestamp(u64::MAX));
        assert!(rollup.is_shanghai_active_at_timestamp(u64::MAX));
        RuntimeUpgradeRegistry::clear_activation_timestamp(chain_id, BaseUpgrade::Canyon);
        assert!(!spec.is_canyon_active_at_timestamp(u64::MAX));
        assert!(!rollup.is_canyon_active_at_timestamp(u64::MAX));
        assert!(!spec.is_shanghai_active_at_timestamp(u64::MAX));
        assert!(!rollup.is_shanghai_active_at_timestamp(u64::MAX));
        assert!(spec.is_cancun_active_at_timestamp(u64::MAX));
        assert!(rollup.is_cancun_active_at_timestamp(u64::MAX));
        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn runtime_registry_overrides_execution_fee_and_blob_params() {
        let chain_id = 9_100_006;
        RuntimeUpgradeRegistry::clear_chain(chain_id);
        let mut config = ChainConfig::mainnet().clone();
        config.chain_id = chain_id;
        config.upgrades.insert(base_common_chains::BaseUpgrade::Beryl, ForkCondition::Never);
        config.upgrades.insert(base_common_chains::BaseUpgrade::Cobalt, ForkCondition::Never);
        let spec = BaseChainSpec::try_from(&config).unwrap();
        let timestamp = 42;
        let parent = spec.genesis_header();
        let static_base_fee = spec.base_fee_params_at_timestamp(timestamp);
        let static_blob_params = spec.blob_params_at_timestamp(timestamp);
        let static_next_base_fee = spec.next_block_base_fee(parent, timestamp);

        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Canyon, timestamp);
        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Ecotone, timestamp);

        assert_eq!(spec.schedule().fork(BaseUpgrade::Canyon), ForkCondition::Timestamp(timestamp));

        assert_eq!(
            spec.base_fee_params_at_timestamp(timestamp),
            config.fee_config.post_canyon_params()
        );
        assert_ne!(spec.base_fee_params_at_timestamp(timestamp), static_base_fee);
        assert_eq!(
            spec.blob_params_at_timestamp(timestamp),
            Some(alloy_eips::eip7840::BlobParams::cancun())
        );
        assert_ne!(spec.blob_params_at_timestamp(timestamp), static_blob_params);
        assert_ne!(spec.next_block_base_fee(parent, timestamp), static_next_base_fee);

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    #[test]
    fn builtin_chain_specs_never_activate_denim_or_zenith() {
        // Built-in production schedules do not configure Denim or genesis-only Zenith.
        for spec in [BaseChainSpec::mainnet(), BaseChainSpec::sepolia(), BaseChainSpec::devnet()] {
            assert_eq!(spec.fork(BaseUpgrade::Denim), ForkCondition::Never);
            assert!(!spec.is_fork_active_at_timestamp(BaseUpgrade::Denim, 0));
            assert!(!spec.is_fork_active_at_timestamp(BaseUpgrade::Denim, u64::MAX));
            assert_eq!(spec.fork(BaseUpgrade::Zenith), ForkCondition::Never);
            assert!(!spec.is_fork_active_at_timestamp(BaseUpgrade::Zenith, 0));
            assert!(!spec.is_fork_active_at_timestamp(BaseUpgrade::Zenith, u64::MAX));
        }
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
    fn beryl_chain_config_without_activation_admin_is_rejected() {
        let mut config = ChainConfig::devnet().clone();
        config.chain_id = 987_654;
        config.activation_admin_address = None;
        config.upgrades.insert(base_common_chains::BaseUpgrade::Beryl, ForkCondition::Timestamp(0));

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
        let chain_spec = BaseChainSpecBuilder::default()
            .chain(987_654.into())
            .genesis(Genesis::default())
            .with_fork(BaseUpgrade::Beryl, ForkCondition::Timestamp(0))
            .activation_admin_address(admin)
            .build();

        assert_eq!(chain_spec.config.activation_admin_address, Some(admin));
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
            b256!("0x572a15dd7e69df35913f7f2217376609fc20d59276169977de92c01684637162")
        );
    }

    #[test]
    fn embedded_genesis_matches_genesis_active_upgrade_conditions() {
        for config in [ChainConfig::mainnet(), ChainConfig::sepolia(), ChainConfig::zeronet()] {
            let genesis: Genesis = serde_json::from_str(config.genesis_json).unwrap();
            for (actual, expected) in [
                (
                    genesis.config.shanghai_time,
                    config.upgrades[base_common_chains::BaseUpgrade::Canyon]
                        .as_timestamp()
                        .unwrap_or_default(),
                ),
                (
                    genesis.config.cancun_time,
                    config.upgrades[base_common_chains::BaseUpgrade::Ecotone]
                        .as_timestamp()
                        .unwrap_or_default(),
                ),
                (
                    genesis.config.prague_time,
                    config.upgrades[base_common_chains::BaseUpgrade::Isthmus]
                        .as_timestamp()
                        .unwrap_or_default(),
                ),
            ] {
                if let Some(actual) = actual {
                    assert_eq!(
                        actual, expected,
                        "Ethereum fork timestamp drift for chain {}",
                        config.chain_id
                    );
                }
            }

            let genesis_info = GenesisInfo::extract_from(&genesis)
                .base_chain_info
                .genesis_info
                .unwrap_or_default();
            let configured = BaseChainSpec::try_from(config).unwrap();
            for (upgrade, actual) in [
                (BaseUpgrade::Regolith, genesis_info.regolith_time),
                (BaseUpgrade::Canyon, genesis_info.canyon_time),
                (BaseUpgrade::Ecotone, genesis_info.ecotone_time),
                (BaseUpgrade::Fjord, genesis_info.fjord_time),
                (BaseUpgrade::Granite, genesis_info.granite_time),
                (BaseUpgrade::Holocene, genesis_info.holocene_time),
                (BaseUpgrade::Isthmus, genesis_info.isthmus_time),
                (BaseUpgrade::Jovian, genesis_info.jovian_time),
            ] {
                let expected = configured.fork(upgrade);
                if expected == ForkCondition::Timestamp(config.genesis.l2_time) {
                    assert_eq!(
                        actual.map(ForkCondition::Timestamp).unwrap_or(ForkCondition::Never),
                        expected,
                        "genesis-active {upgrade} timestamp drift for chain {}",
                        config.chain_id
                    );
                }
            }
        }
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
        let unknown = BaseChainSpecBuilder::default()
            .genesis(Genesis::default())
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
          "v2": 60,
          "v3": 65,
          "denim": 900000,
          "zenith": 1000000
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
            chain_spec.config.fee_config,
            base_common_genesis::FeeConfig {
                eip1559_elasticity: 60,
                eip1559_denominator: 70,
                eip1559_denominator_canyon: 70
            }
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
        assert!(!chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Cobalt, 64));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Cobalt, 65));
        assert!(!chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Denim, 899_999));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Denim, 900_000));
        assert!(!chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Zenith, 999_999));
        assert!(chain_spec.is_fork_active_at_timestamp(BaseUpgrade::Zenith, 1_000_000));
    }

    #[test]
    fn set_hardfork_activation_timestamp_updates_matching_eth_fork() {
        let mut chain_spec = BaseChainSpec::devnet();

        chain_spec.set_fork(base_common_genesis::BaseUpgrade::Azul, ForkCondition::Never);
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
        let mut chain_spec = BaseChainSpec::default();

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
            chain_spec.config.fee_config,
            base_common_genesis::FeeConfig {
                eip1559_elasticity: 60,
                eip1559_denominator: 70,
                eip1559_denominator_canyon: 80
            }
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
        let chainspec = BaseChainSpec::from_genesis(genesis.clone());

        let actual_chain_id = genesis.config.chain_id;
        assert_eq!(actual_chain_id, 8453);

        assert_eq!(
            chainspec.config.upgrades.get(EthereumHardfork::Istanbul),
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
            chainspec.config.fee_config,
            base_common_genesis::FeeConfig {
                eip1559_elasticity: 6,
                eip1559_denominator: 50,
                eip1559_denominator_canyon: 50
            }
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

        let upgrades: Vec<_> = chain_spec.config.upgrades.forks_iter().map(|(h, _)| h).collect();
        let expected_upgrades: Vec<base_common_chains::ExecutionFork> = vec![
            EthereumHardfork::Frontier.into(),
            EthereumHardfork::Homestead.into(),
            EthereumHardfork::Tangerine.into(),
            EthereumHardfork::SpuriousDragon.into(),
            EthereumHardfork::Byzantium.into(),
            EthereumHardfork::Constantinople.into(),
            EthereumHardfork::Petersburg.into(),
            EthereumHardfork::Istanbul.into(),
            EthereumHardfork::MuirGlacier.into(),
            EthereumHardfork::Berlin.into(),
            EthereumHardfork::London.into(),
            EthereumHardfork::ArrowGlacier.into(),
            EthereumHardfork::GrayGlacier.into(),
            EthereumHardfork::Paris.into(),
            BaseUpgrade::Bedrock.into(),
            BaseUpgrade::Regolith.into(),
            EthereumHardfork::Shanghai.into(),
            BaseUpgrade::Canyon.into(),
            EthereumHardfork::Cancun.into(),
            BaseUpgrade::Ecotone.into(),
            BaseUpgrade::Fjord.into(),
            BaseUpgrade::Granite.into(),
            BaseUpgrade::Holocene.into(),
            EthereumHardfork::Prague.into(),
            BaseUpgrade::Isthmus.into(),
            BaseUpgrade::Jovian.into(),
            EthereumHardfork::Osaka.into(),
            BaseUpgrade::Azul.into(),
            BaseUpgrade::Beryl.into(),
            BaseUpgrade::Cobalt.into(),
        ];

        for (expected, actual) in expected_upgrades.iter().zip(upgrades.iter()) {
            assert_eq!(expected, actual);
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

    #[test]
    fn genesis_boundary_preserves_consensus_anchor() {
        let configured = ChainConfig::mainnet();
        let spec =
            BaseChainSpec::from_genesis(serde_json::from_str(configured.genesis_json).unwrap());
        assert_eq!(spec.config.genesis, configured.genesis);
        assert_eq!(spec.genesis_hash(), configured.genesis.l2.hash);
    }

    #[test]
    fn configured_upgrade_refreshes_canonical_genesis() {
        let mut spec =
            BaseChainSpecBuilder::default().genesis(Genesis::default()).bedrock_activated().build();
        let before = spec.genesis_hash();
        spec.set_fork(BaseUpgrade::Canyon, ForkCondition::Timestamp(0));
        spec.refresh_genesis_header();
        assert_ne!(spec.genesis_hash(), before);
        assert_eq!(spec.config.genesis.l2.hash, spec.genesis_hash());
        assert_eq!(spec.config.genesis.l2.number, spec.genesis_header.number);
        assert_eq!(spec.config.genesis.l2_time, spec.genesis.timestamp);
        assert!(spec.genesis_header.withdrawals_root.is_some());
    }
}
