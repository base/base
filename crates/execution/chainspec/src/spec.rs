use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use alloy_chains::Chain;
use alloy_consensus::{BlockHeader, Header, proofs::storage_root_unhashed};
use alloy_eips::eip7840::BlobParams;
use alloy_genesis::Genesis;
use alloy_hardforks::Hardfork;
use alloy_primitives::{B256, U256};
use base_common_chains::{BaseUpgrade, Upgrades};
use base_common_consensus::Predeploys;
use derive_more::{Constructor, Deref, Into};
use reth_chainspec::{
    BaseFeeParams, BaseFeeParamsKind, ChainSpec, DepositContract, DisplayHardforks, EthChainSpec,
    EthereumHardforks, ForkFilter, ForkId, Hardforks, Head,
};
use reth_ethereum_forks::{ChainHardforks, EthereumHardfork, ForkCondition};
use reth_network_peers::NodeRecord;
use reth_primitives_traits::SealedHeader;

use crate::{
    BASE_DEV, BASE_MAINNET, BASE_MAINNET_UPGRADES, BASE_SEPOLIA, BASE_ZERONET,
    compute_jovian_base_fee, decode_holocene_base_fee,
};

/// All supported chain names for the CLI.
pub const SUPPORTED_CHAINS: &[&str] =
    &["base", "base_sepolia", "base-sepolia", "base-zeronet", "dev"];

/// Genesis info extracted from a Base genesis config.
#[derive(Default, Debug)]
pub struct GenesisInfo {
    /// Base chain info extracted from genesis extra fields.
    pub optimism_chain_info: base_common_rpc_types::ChainInfo,
    /// Base fee params derived from the genesis config.
    pub base_fee_params: BaseFeeParamsKind,
}

impl GenesisInfo {
    /// Extracts Base genesis info from an [`alloy_genesis::Genesis`].
    pub fn extract_from(genesis: &Genesis) -> Self {
        let mut info = Self {
            optimism_chain_info: base_common_rpc_types::ChainInfo::extract_from(
                &genesis.config.extra_fields,
            )
            .unwrap_or_default(),
            ..Default::default()
        };
        if let Some(optimism_base_fee_info) = &info.optimism_chain_info.base_fee_info
            && let (Some(elasticity), Some(denominator)) = (
                optimism_base_fee_info.eip1559_elasticity,
                optimism_base_fee_info.eip1559_denominator,
            )
        {
            let base_fee_params = optimism_base_fee_info.eip1559_denominator_canyon.map_or_else(
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
    pub inner: ChainSpec,
}

impl BaseChainSpec {
    /// Converts the given [`Genesis`] into an [`BaseChainSpec`].
    pub fn from_genesis(genesis: Genesis) -> Self {
        genesis.into()
    }

    /// Builds a [`Header`] for the genesis block of a Base chain.
    ///
    /// Extends [`reth_chainspec::make_genesis_header`] with Isthmus-specific withdrawals root
    /// logic: if Isthmus is active at the genesis timestamp, the withdrawals root is set to the
    /// storage root of the `L2ToL1MessagePasser` predeploy.
    pub fn make_genesis_header(genesis: &Genesis, hardforks: &ChainHardforks) -> Header {
        let mut header = reth_chainspec::make_genesis_header(genesis, hardforks);

        if hardforks.fork(BaseUpgrade::Isthmus).active_at_timestamp(header.timestamp)
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
        match s {
            "dev" => Some(BASE_DEV.clone()),
            "base" => Some(BASE_MAINNET.clone()),
            "base_sepolia" | "base-sepolia" => Some(BASE_SEPOLIA.clone()),
            "base-zeronet" => Some(BASE_ZERONET.clone()),
            _ => None,
        }
    }

    /// Activates or updates the given hardfork condition in-place.
    pub fn set_fork<H: Hardfork>(&mut self, fork: H, condition: ForkCondition) {
        self.inner.hardforks.insert(fork, condition);
    }
}

impl EthChainSpec for BaseChainSpec {
    type Header = Header;

    fn chain(&self) -> Chain {
        self.inner.chain()
    }

    fn base_fee_params_at_timestamp(&self, timestamp: u64) -> BaseFeeParams {
        self.inner.base_fee_params_at_timestamp(timestamp)
    }

    fn blob_params_at_timestamp(&self, timestamp: u64) -> Option<BlobParams> {
        self.inner.blob_params_at_timestamp(timestamp)
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
        let op_forks = self.inner.hardforks.forks_iter().filter(|(fork, _)| {
            !EthereumHardfork::VARIANTS.iter().any(|h| h.name() == (*fork).name())
        });

        Box::new(DisplayHardforks::new(op_forks))
    }

    fn genesis_header(&self) -> &Self::Header {
        self.inner.genesis_header()
    }

    fn genesis(&self) -> &Genesis {
        self.inner.genesis()
    }

    fn bootnodes(&self) -> Option<Vec<NodeRecord>> {
        self.inner.bootnodes()
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
            self.inner.next_block_base_fee(parent, target_timestamp)
        }
    }
}

impl Hardforks for BaseChainSpec {
    fn fork<H: Hardfork>(&self, fork: H) -> ForkCondition {
        self.inner.fork(fork)
    }

    fn forks_iter(&self) -> impl Iterator<Item = (&dyn Hardfork, ForkCondition)> {
        self.inner.forks_iter()
    }

    fn fork_id(&self, head: &Head) -> ForkId {
        self.inner.fork_id(head)
    }

    fn latest_fork_id(&self) -> ForkId {
        self.inner.latest_fork_id()
    }

    fn fork_filter(&self, head: Head) -> ForkFilter {
        self.inner.fork_filter(head)
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
}

impl From<Genesis> for BaseChainSpec {
    fn from(genesis: Genesis) -> Self {
        let optimism_genesis_info = GenesisInfo::extract_from(&genesis);
        let genesis_info =
            optimism_genesis_info.optimism_chain_info.genesis_info.unwrap_or_default();

        // Block-based hardforks
        let hardfork_opts = [
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
            (BaseUpgrade::Bedrock.boxed(), genesis_info.bedrock_block),
        ];
        let mut block_hardforks = hardfork_opts
            .into_iter()
            .filter_map(|(hardfork, opt)| opt.map(|block| (hardfork, ForkCondition::Block(block))))
            .collect::<Vec<_>>();

        // We set the paris hardfork for Base networks to zero
        block_hardforks.push((
            EthereumHardfork::Paris.boxed(),
            ForkCondition::TTD {
                activation_block_number: 0,
                total_difficulty: U256::ZERO,
                fork_block: genesis.config.merge_netsplit_block,
            },
        ));

        // Time-based hardforks
        // L1 hardforks are mapped to the activation timestamps of the corresponding Base hardforks
        let azul_time = genesis_info.base.azul;
        let time_hardfork_opts = [
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
        ];

        let mut time_hardforks = time_hardfork_opts
            .into_iter()
            .filter_map(|(hardfork, opt)| {
                opt.map(|time| (hardfork, ForkCondition::Timestamp(time)))
            })
            .collect::<Vec<_>>();

        block_hardforks.append(&mut time_hardforks);

        // Order hardforks to match mainnet ordering
        let mainnet_hardforks = BASE_MAINNET_UPGRADES.clone();
        let mainnet_order = mainnet_hardforks.forks_iter();

        let mut ordered_hardforks = Vec::with_capacity(block_hardforks.len());
        for (hardfork, _) in mainnet_order {
            if let Some(pos) = block_hardforks.iter().position(|(e, _)| **e == *hardfork) {
                ordered_hardforks.push(block_hardforks.remove(pos));
            }
        }
        ordered_hardforks.append(&mut block_hardforks);

        let hardforks = ChainHardforks::new(ordered_hardforks);
        let genesis_header =
            SealedHeader::seal_slow(Self::make_genesis_header(&genesis, &hardforks));

        Self {
            inner: ChainSpec {
                chain: genesis.config.chain_id.into(),
                genesis_header,
                genesis,
                hardforks,
                paris_block_and_final_difficulty: Some((0, U256::ZERO)),
                base_fee_params: optimism_genesis_info.base_fee_params,
                ..Default::default()
            },
        }
    }
}

impl From<ChainSpec> for BaseChainSpec {
    fn from(value: ChainSpec) -> Self {
        Self { inner: value }
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

    use alloy_consensus::proofs::storage_root_unhashed;
    use alloy_genesis::{ChainConfig as AlloyChainConfig, Genesis};
    use alloy_hardforks::Hardfork;
    use alloy_primitives::{B256, U256, b256};
    use base_common_chains::{BaseUpgrade, ChainConfig, Upgrades};
    use base_common_rpc_types::FeeInfo;
    use reth_chainspec::{
        BaseFeeParams, BaseFeeParamsKind, EthChainSpec, EthereumHardforks, test_fork_ids,
    };
    use reth_ethereum_forks::{EthereumHardfork, ForkCondition, ForkHash, ForkId, Head};

    use crate::{BASE_MAINNET, BASE_SEPOLIA, BASE_ZERONET, BaseChainSpec, BaseChainSpecBuilder};

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
        let mut base_mainnet = BaseChainSpecBuilder::base_mainnet().build();
        base_mainnet.inner.genesis_header.set_hash(BASE_MAINNET.genesis_hash());
        test_fork_ids(
            &BASE_MAINNET,
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
                    BASE_MAINNET.hardfork_fork_id(BaseUpgrade::Jovian).unwrap(),
                ),
                (
                    Head {
                        number: 0,
                        timestamp: ChainConfig::mainnet().azul_timestamp.unwrap(),
                        ..Default::default()
                    },
                    BASE_MAINNET.hardfork_fork_id(BaseUpgrade::Azul).unwrap(),
                ),
            ],
        );
    }

    #[test]
    fn base_sepolia_forkids() {
        test_fork_ids(
            &BASE_SEPOLIA,
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
                    BASE_SEPOLIA.hardfork_fork_id(BaseUpgrade::Jovian).unwrap(),
                ),
            ],
        );
    }

    #[test]
    fn base_mainnet_genesis() {
        let genesis = BASE_MAINNET.genesis_header();
        assert_eq!(
            genesis.hash_slow(),
            b256!("0xf712aa9241cc24369b143cf6dce85f0902a9731e70d66818a3a5845b296c73dd")
        );
        let base_fee = BASE_MAINNET.next_block_base_fee(genesis, genesis.timestamp).unwrap();
        assert_eq!(base_fee, 980000000);
    }

    #[test]
    fn base_sepolia_genesis() {
        let genesis = BASE_SEPOLIA.genesis_header();
        assert_eq!(
            genesis.hash_slow(),
            b256!("0x0dcc9e089e30b90ddfc55be9a37dd15bc551aeee999d2e2b51414c54eaf934e4")
        );
        let base_fee = BASE_SEPOLIA.next_block_base_fee(genesis, genesis.timestamp).unwrap();
        assert_eq!(base_fee, 980000000);
    }

    #[test]
    fn base_zeronet_genesis() {
        let genesis = BASE_ZERONET.genesis_header();
        assert_eq!(
            genesis.hash_slow(),
            b256!("0x1842d6ef4c40e2a4794458e167f6d327269df919b626979111c37ad3a96047bf")
        );
    }

    #[test]
    fn latest_base_mainnet_fork_id() {
        assert_eq!(
            BASE_MAINNET.hardfork_fork_id(BaseUpgrade::Azul).unwrap(),
            BASE_MAINNET.latest_fork_id()
        )
    }

    #[test]
    fn latest_base_mainnet_fork_id_with_builder() {
        let base_mainnet = BaseChainSpecBuilder::base_mainnet().build();
        assert_eq!(
            BASE_MAINNET.hardfork_fork_id(BaseUpgrade::Azul).unwrap(),
            base_mainnet.latest_fork_id()
        )
    }

    #[test]
    fn parse_base_hardforks() {
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
          "v1": 55
        },
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

        let optimism_object = genesis.config.extra_fields.get("optimism").unwrap();
        assert_eq!(
            optimism_object,
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

        let optimism_object = genesis.config.extra_fields.get("optimism").unwrap();
        let optimism_base_fee_info =
            serde_json::from_value::<FeeInfo>(optimism_object.clone()).unwrap();

        assert_eq!(
            optimism_base_fee_info,
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
    fn test_fork_order_base_hardforks() {
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
                    (String::from("base"), serde_json::json!({ "v1": 0 })),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            },
            ..Default::default()
        };

        let chain_spec: BaseChainSpec = genesis.into();

        let hardforks: Vec<_> = chain_spec.hardforks.forks_iter().map(|(h, _)| h).collect();
        let expected_hardforks = vec![
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
        ];

        for (expected, actual) in expected_hardforks.iter().zip(hardforks.iter()) {
            assert_eq!(&**expected, &**actual);
        }
        assert_eq!(expected_hardforks.len(), hardforks.len());
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
        let content = BASE_MAINNET.display_hardforks().to_string();
        for eth_hf in EthereumHardfork::VARIANTS {
            assert!(!content.contains(eth_hf.name()));
        }
    }
}
