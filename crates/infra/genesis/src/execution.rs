//! Execution genesis and contract input assembly.

use std::collections::BTreeMap;

use alloy_eips::BlockNumHash;
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, U256, address, keccak256};
use base_common_genesis::{
    BaseUpgrade, ChainGenesis, FeeConfig, RollupConfig, SystemConfig, UpgradeConfig,
};
use base_execution_chainspec::BaseChainSpec;
use eyre::{Result, ensure};
use reth_chainspec::ChainSpec;
use serde_json::{Value, json};

use crate::GenesisConfig;

/// Execution state generation and serialization.
#[derive(Debug)]
pub struct ExecutionGenesis;

impl ExecutionGenesis {
    /// Resolve the configured network upgrade schedule.
    pub fn upgrades(config: &GenesisConfig) -> Result<UpgradeConfig> {
        let mut upgrades = UpgradeConfig::default();
        for upgrade in [
            BaseUpgrade::Regolith,
            BaseUpgrade::Canyon,
            BaseUpgrade::Delta,
            BaseUpgrade::Ecotone,
            BaseUpgrade::Fjord,
            BaseUpgrade::Granite,
            BaseUpgrade::Holocene,
            BaseUpgrade::Isthmus,
            BaseUpgrade::Jovian,
        ] {
            upgrades.set_activation_timestamp(upgrade, 0);
        }
        for (upgrade, timestamp) in
            config.upgrades.timestamps(config.timestamp.unwrap_or_default())?
        {
            upgrades.set_activation_timestamp(upgrade, timestamp);
        }
        // Jovian must not activate before a delayed Isthmus.
        if config.upgrades.isthmus.is_some_and(|block| block > 0) {
            upgrades.jovian_time = None;
        }
        Ok(upgrades)
    }

    /// The ordered schedule imported into the real `ProtocolVersions` contract.
    pub fn schedule(config: &GenesisConfig) -> Result<Vec<u64>> {
        let upgrades = Self::upgrades(config)?;
        let schedule: Vec<_> = BaseUpgrade::CONTRACT_VARIANTS
            .into_iter()
            .map(|upgrade| {
                upgrades
                    .activation_timestamp(upgrade)
                    .map(|time| if time == 0 { config.timestamp.unwrap_or_default() } else { time })
                    .unwrap_or_default()
            })
            .collect();
        let mut previous = 0;
        for &time in &schedule {
            if time != 0 {
                ensure!(time >= previous, "upgrade activations must follow contract ID order");
                previous = time;
            }
        }
        Ok(schedule)
    }

    /// Create the single configuration consumed by both Solidity adapters.
    pub fn deploy_config(config: &GenesisConfig) -> Result<Value> {
        let mut input: Value = serde_json::from_str(include_str!("../assets/deploy-config.json"))?;
        for name in [
            "finalSystemOwner",
            "proxyAdminOwner",
            "superchainConfigGuardian",
            "superchainConfigIncidentResponder",
            "baseFeeVaultRecipient",
            "l1FeeVaultRecipient",
            "sequencerFeeVaultRecipient",
            "operatorFeeVaultRecipient",
        ] {
            input[name] = json!(config.owner);
        }
        input["batchSenderAddress"] = json!(config.batcher);
        input["p2pSequencerAddress"] = json!(config.sequencer);
        input["teeProposer"] = json!(config.proposer);
        input["teeChallenger"] = json!(config.challenger);
        input["l1ChainId"] = json!(config.l1_chain_id);
        input["l2ChainId"] = json!(config.l2_chain_id);
        input["l2GenesisTimestamp"] = json!(config.timestamp);
        input["protocolVersionsInitialMinimumVersion"] = json!(config.minimum_protocol_version);
        input["protocolVersionsInitialSchedule"] = json!(Self::schedule(config)?);
        input["salt"] = json!(config.salt);
        input["fork"] =
            json!(if config.upgrades.isthmus.is_some_and(|block| block > 0) { 3 } else { 5 });
        Ok(input)
    }

    /// Merge deployed L1 state with funded development accounts.
    pub fn l1(
        config: &GenesisConfig,
        mut alloc: BTreeMap<Address, GenesisAccount>,
    ) -> Result<Genesis> {
        let mut genesis: Genesis = serde_json::from_str(include_str!("../assets/l1.json"))?;
        genesis.config.chain_id = config.l1_chain_id;
        genesis.timestamp = config.timestamp.unwrap_or_default();
        for (address, account) in &genesis.alloc {
            if let Some(deployed) = alloc.get_mut(address) {
                deployed.balance = account.balance;
            } else {
                alloc.insert(*address, account.clone());
            }
        }
        for role in
            [config.owner, config.sequencer, config.batcher, config.proposer, config.challenger]
        {
            alloc.entry(role).or_default().balance =
                U256::from(1_000_000u64) * U256::from(10u64).pow(U256::from(18));
        }
        genesis.alloc = alloc;
        Ok(genesis)
    }

    /// Construct a Base genesis with all fork patches applied before hashing it.
    pub fn l2(
        config: &GenesisConfig,
        alloc: BTreeMap<Address, GenesisAccount>,
    ) -> Result<BaseChainSpec> {
        let upgrades = Self::upgrades(config)?;
        let mut genesis: Value = serde_json::from_str(include_str!("../assets/l1.json"))?;
        let chain = genesis["config"]
            .as_object_mut()
            .ok_or_else(|| eyre::eyre!("invalid genesis template"))?;
        for name in ["blobSchedule", "bpo1Time", "bpo2Time", "osakaTime"] {
            chain.remove(name);
        }
        chain.insert("chainId".into(), json!(config.l2_chain_id));
        chain.insert("bedrockBlock".into(), json!(0));
        chain.insert("optimism".into(), json!(FeeConfig::BASE_MAINNET));
        chain.insert("activationAdminAddress".into(), json!(config.activation_admin));
        for upgrade in BaseUpgrade::CONTRACT_VARIANTS.into_iter().chain([BaseUpgrade::Zenith]) {
            if let Some(timestamp) = upgrades.activation_timestamp(upgrade) {
                if matches!(
                    upgrade,
                    BaseUpgrade::Azul
                        | BaseUpgrade::Beryl
                        | BaseUpgrade::Cobalt
                        | BaseUpgrade::Denim
                        | BaseUpgrade::Zenith
                ) {
                    chain.entry("base").or_insert_with(|| json!({}))[upgrade.contract_id()] =
                        json!(timestamp);
                } else if upgrade != BaseUpgrade::PectraBlobSchedule {
                    chain.insert(format!("{}Time", upgrade.contract_id()), json!(timestamp));
                }
            }
        }
        chain.insert("pragueTime".into(), json!(upgrades.isthmus_time));
        if let Some(azul) = upgrades.activation_timestamp(BaseUpgrade::Azul) {
            chain.insert("osakaTime".into(), json!(azul));
        }
        genesis["timestamp"] = json!(format!("{:#x}", config.timestamp.unwrap_or_default()));
        genesis["gasLimit"] = json!("0x3938700");
        genesis["coinbase"] = json!(address!("4200000000000000000000000000000000000011"));
        genesis["extraData"] = json!(if upgrades.jovian_time == Some(0) {
            "0x01000000fa000000060000000000000000"
        } else {
            "0x00000000fa00000006"
        });
        genesis["alloc"] = serde_json::to_value(alloc)?;
        BaseChainSpec::try_from_genesis(serde_json::from_value(genesis)?).map_err(Into::into)
    }

    /// Build rollup configuration referencing the final L1 and L2 genesis blocks.
    pub fn rollup(
        config: &GenesisConfig,
        l1: &ChainSpec,
        l2: &BaseChainSpec,
        addresses: &BTreeMap<String, Address>,
    ) -> Result<RollupConfig> {
        let get =
            |name: &str| addresses.get(name).copied().ok_or_else(|| eyre::eyre!("missing {name}"));
        let hash = keccak256(U256::from(config.l2_chain_id).to_be_bytes::<32>());
        let mut inbox = [0u8; 20];
        inbox[1..].copy_from_slice(&hash[..19]);
        Ok(RollupConfig {
            genesis: ChainGenesis {
                l1: BlockNumHash { number: 0, hash: l1.genesis_hash() },
                l2: BlockNumHash { number: 0, hash: l2.genesis_hash() },
                l2_time: config.timestamp.unwrap_or_default(),
                system_config: Some(SystemConfig {
                    batcher_address: config.batcher,
                    scalar: (U256::from(1) << 248) | (U256::from(801_949) << 32) | U256::from(1368),
                    gas_limit: 60_000_000,
                    ..Default::default()
                }),
            },
            block_time: 2,
            max_sequencer_drift: 600,
            seq_window_size: 3600,
            channel_timeout: 300,
            l1_chain_id: config.l1_chain_id,
            l2_chain_id: config.l2_chain_id.into(),
            upgrades: Self::upgrades(config)?,
            batch_inbox_address: Address::from(inbox),
            deposit_contract_address: get("OptimismPortalProxy")?,
            l1_system_config_address: get("SystemConfigProxy")?,
            protocol_versions_address: get("ProtocolVersionsProxy")?,
            ..Default::default()
        })
    }
}
