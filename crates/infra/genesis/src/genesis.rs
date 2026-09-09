use std::time::{SystemTime, UNIX_EPOCH};

use alloy_eips::eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE};
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{B64, B256, U256, address, keccak256};
use base_common_consensus::{HoloceneExtraData, JovianExtraData};
use base_common_genesis::{ChainGenesis, FeeConfig, RollupConfig, SystemConfig, UpgradeConfig};
use base_execution_chainspec::BaseChainSpec;
use eyre::Result;
use reth_chainspec::{ChainSpec, EthChainSpec};
use serde_json::{Value, json};
use tracing::info;

use crate::{BeaconGenesis, ContractArtifacts, Deployment, GenesisConfig};

/// Final execution, consensus, and rollup genesis artifacts.
#[derive(Debug)]
pub struct GenesisArtifacts {
    /// Ethereum devnet genesis.
    pub l1: Genesis,
    /// Base devnet genesis.
    pub l2: Genesis,
    /// Base rollup configuration.
    pub rollup: RollupConfig,
    /// Existing contract-address document.
    pub addresses: Value,
    /// Consensus genesis and validator credentials.
    pub beacon: BeaconGenesis,
}

/// Fixed, offline, single-chain generation.
#[derive(Debug)]
pub struct GenesisGenerator;

impl GenesisGenerator {
    /// Generates all artifacts in-process, before writing any completion markers.
    pub fn generate(
        config: &GenesisConfig,
        contracts: &ContractArtifacts,
    ) -> Result<GenesisArtifacts> {
        config.validate()?;
        let timestamp =
            config.timestamp.unwrap_or(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs());
        let mut inbox = [0u8; 20];
        inbox[1..]
            .copy_from_slice(&keccak256(U256::from(config.l2_chain_id).to_be_bytes::<32>())[..19]);
        // Resolve the local fork schedule without consulting runtime overrides.
        let mut rollup = RollupConfig {
            genesis: ChainGenesis { l2_time: timestamp, ..Default::default() },
            batch_inbox_address: inbox.into(),
            block_time: 2,
            l1_chain_id: config.l1_chain_id,
            l2_chain_id: config.l2_chain_id.into(),
            max_sequencer_drift: 600,
            seq_window_size: 3600,
            channel_timeout: 300,
            chain_op_config: FeeConfig::BASE_MAINNET,
            upgrades: UpgradeConfig {
                regolith_time: Some(0),
                canyon_time: Some(0),
                delta_time: Some(0),
                ecotone_time: Some(0),
                fjord_time: Some(0),
                granite_time: Some(0),
                holocene_time: Some(0),
                isthmus_time: Some(0),
                jovian_time: Some(0),
                ..Default::default()
            },
            ..Default::default()
        };
        config.apply_upgrades(&mut rollup)?;
        let deployment = Deployment::generate(
            config,
            contracts,
            config.salt.unwrap_or_else(B256::random),
            &rollup,
        )?;
        info!(
            l1_accounts = deployment.l1.len(),
            l2_accounts = deployment.l2.len(),
            "initialized Base devnet contracts"
        );
        let template = include_str!("../assets/l1-el-genesis.json.template")
            .replace("${CHAIN_ID}", &config.l1_chain_id.to_string())
            .replace("${GENESIS_TIME_HEX}", &format!("0x{timestamp:x}"))
            .replace("${BALANCE}", "0xd3c21bcecceda1000000");
        let mut l1: Genesis = serde_json::from_str(&template)?;
        let prefund = std::mem::replace(&mut l1.alloc, deployment.l1);
        for (address, account) in prefund {
            // Preserve deployed code, storage and nonce when accounts overlap.
            l1.alloc.entry(address).or_insert_with(|| account.clone()).balance = account.balance;
        }
        rollup.deposit_contract_address = deployment.addresses["OptimismPortalProxy"];
        rollup.l1_system_config_address = deployment.addresses["SystemConfigProxy"];
        rollup.protocol_versions_address = deployment.addresses["ProtocolVersionsProxy"];
        rollup.genesis.system_config = Some(SystemConfig {
            batcher_address: config.roles[2],
            gas_limit: 60_000_000,
            scalar: (U256::from(1) << 248) | (U256::from(801949) << 32) | U256::from(1368),
            operator_fee_scalar: Some(0),
            operator_fee_constant: Some(0),
            min_base_fee: Some(GenesisConfig::MIN_BASE_FEE),
            da_footprint_gas_scalar: Some(0),
            ..Default::default()
        });
        let mut chain_config = json!({
            "chainId": config.l2_chain_id, "homesteadBlock": 0, "eip150Block": 0, "eip155Block": 0,
            "eip158Block": 0, "byzantiumBlock": 0, "constantinopleBlock": 0, "petersburgBlock": 0,
            "istanbulBlock": 0, "muirGlacierBlock": 0, "berlinBlock": 0, "londonBlock": 0,
            "arrowGlacierBlock": 0, "grayGlacierBlock": 0, "mergeNetsplitBlock": 0,
            "terminalTotalDifficulty": 0, "bedrockBlock": 0, "regolithTime": 0,
            "canyonTime": 0, "shanghaiTime": 0, "ecotoneTime": 0, "cancunTime": 0,
            "fjordTime": 0, "graniteTime": 0, "holoceneTime": 0,
            "jovianTime": rollup.upgrades.jovian_time,
            "isthmusTime": rollup.upgrades.isthmus_time, "pragueTime": rollup.upgrades.isthmus_time,
            "optimism": FeeConfig::BASE_MAINNET, "base": rollup.upgrades.base,
            "activationAdminAddress": config.activation_admin,
        });
        if let Some(azul) = rollup.upgrades.base.azul {
            chain_config["osakaTime"] = json!(azul);
        }
        let mut l2 = Genesis {
            config: serde_json::from_value(chain_config)?,
            timestamp,
            gas_limit: 60_000_000,
            coinbase: address!("4200000000000000000000000000000000000011"),
            base_fee_per_gas: Some(1_000_000_000),
            extra_data: if rollup.upgrades.jovian_time.is_some_and(|time| time <= timestamp) {
                JovianExtraData::encode(
                    B64::ZERO,
                    FeeConfig::BASE_MAINNET.post_canyon_params(),
                    GenesisConfig::MIN_BASE_FEE,
                )?
            } else {
                HoloceneExtraData::encode(B64::ZERO, FeeConfig::BASE_MAINNET.post_canyon_params())?
            },
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            alloc: deployment.l2,
            ..Default::default()
        };
        l2.alloc.entry(HISTORY_STORAGE_ADDRESS).or_insert_with(|| GenesisAccount {
            nonce: Some(1),
            code: Some(HISTORY_STORAGE_CODE.clone()),
            ..Default::default()
        });
        let l1_spec = ChainSpec::from(l1.clone());
        rollup.genesis.l1.hash = l1_spec.genesis_hash();
        rollup.genesis.l2.hash = BaseChainSpec::try_from_genesis(l2.clone())?.genesis_hash();
        let beacon = BeaconGenesis::generate(
            l1_spec.genesis_header(),
            config.l1_chain_id,
            config.slot_duration,
        )?;
        Ok(GenesisArtifacts {
            l1,
            l2,
            rollup,
            addresses: serde_json::to_value(deployment.addresses)?,
            beacon,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_dyn_abi::DynSolValue;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_genesis::BaseUpgrade;
    use base_execution_chainspec::{compute_jovian_base_fee, decode_holocene_base_fee};
    use tempfile::TempDir;

    use super::*;
    use crate::{GenesisEvm, GenesisOutput};

    #[test]
    #[ignore = "requires pinned contract artifacts; run just genesis-test"]
    fn rejects_invalid_registry_inputs_with_actionable_errors() -> Result<()> {
        let contracts = ContractArtifacts::load(std::path::Path::new(&std::env::var(
            "BASE_GENESIS_ARTIFACTS",
        )?))?;
        let base = GenesisConfig {
            timestamp: Some(1_800_000_000),
            salt: Some(B256::repeat_byte(1)),
            ..Default::default()
        };
        for (config, expected) in [
            (GenesisConfig { minimum_version: U256::ZERO, ..base.clone() }, "nonzero"),
            (GenesisConfig { timestamp: Some(3599), ..base.clone() }, "notice"),
            (
                GenesisConfig {
                    upgrades: [("azul".into(), 27), ("cobalt".into(), 22)].into(),
                    ..base
                },
                "cobalt activation",
            ),
        ] {
            let error = GenesisGenerator::generate(&config, &contracts).unwrap_err();
            assert!(error.to_string().contains(expected), "{error:?}");
            assert!(!error.to_string().contains("upgradeAndCall"), "{error:?}");
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires pinned contract artifacts; run just genesis-test"]
    fn generates_consistent_genesis_with_deferred_isthmus() -> Result<()> {
        let contracts = ContractArtifacts::load(std::path::Path::new(&std::env::var(
            "BASE_GENESIS_ARTIFACTS",
        )?))?;
        for preinstall_upgrade_signal in [true, false] {
            let config = GenesisConfig {
                timestamp: Some(1_800_000_000),
                salt: Some(B256::repeat_byte(1)),
                upgrades: [("isthmus".into(), 10)].into(),
                preinstall_upgrade_signal,
                ..Default::default()
            };
            let artifacts = GenesisGenerator::generate(&config, &contracts)?;
            let activation = 1_800_000_020;
            assert_eq!(artifacts.rollup.upgrades.isthmus_time, Some(activation));
            assert_eq!(artifacts.rollup.upgrades.jovian_time, Some(activation));
            assert_eq!(artifacts.l2.config.prague_time, Some(activation));
            let spec = BaseChainSpec::try_from_genesis(artifacts.l2.clone())?;
            for upgrade in [BaseUpgrade::Isthmus, BaseUpgrade::Jovian] {
                assert!(!spec.hardforks.fork(upgrade).active_at_timestamp(activation - 1));
                assert!(spec.hardforks.fork(upgrade).active_at_timestamp(activation));
            }
            // The first post-genesis block must be able to decode its parent's
            // pre-Jovian fee parameters.
            let next_base_fee =
                decode_holocene_base_fee(&spec, spec.genesis_header(), artifacts.l2.timestamp + 2)?;
            assert!(next_base_fee > 0);
            assert_eq!(artifacts.rollup.genesis.l2.hash, spec.genesis_hash());

            let mut evm = GenesisEvm::new(&contracts, config.l1_chain_id)?;
            for (address, account) in &artifacts.l1.alloc {
                if let Some(code) = &account.code {
                    evm.code(*address, code.clone());
                }
                for (slot, value) in account.storage.iter().flatten() {
                    evm.storage(*address, *slot, *value)?;
                }
            }
            let schedule = evm.call(
                "ProtocolVersions",
                artifacts.rollup.protocol_versions_address,
                "getSchedule",
                json!([]),
                config.roles[0],
            )?;
            let expected = [
                1_800_000_000,
                1_800_000_000,
                1_800_000_000,
                1_800_000_000,
                1_800_000_000,
                1_800_000_000,
                1_800_000_000,
                0,
                activation,
                activation,
                0,
                0,
                0,
                0,
            ];
            assert_eq!(
                contracts.get("ProtocolVersions")?.decode("getSchedule", &schedule)?,
                vec![DynSolValue::Array(
                    expected
                        .into_iter()
                        .map(|time| DynSolValue::Uint(U256::from(time), 64))
                        .collect()
                )]
            );
            let mut evm = GenesisEvm::new(&contracts, config.l2_chain_id)?;
            for (address, account) in &artifacts.l2.alloc {
                if let Some(code) = &account.code {
                    evm.code(*address, code.clone());
                }
                for (slot, value) in account.storage.iter().flatten() {
                    evm.storage(*address, *slot, *value)?;
                }
            }
            let oracle = contracts.predeploy("GAS_PRICE_ORACLE")?;
            for (getter, setter) in [("isIsthmus", "setIsthmus"), ("isJovian", "setJovian")] {
                let before =
                    evm.call("GasPriceOracle", oracle, getter, json!([]), config.roles[0])?;
                assert_eq!(
                    contracts.get("GasPriceOracle")?.decode(getter, &before)?,
                    vec![DynSolValue::Bool(false)]
                );
                evm.evm.ctx.block.timestamp = U256::from(activation);
                evm.call(
                    "GasPriceOracle",
                    oracle,
                    setter,
                    json!([]),
                    address!("DeaDDEaDDeAdDeAdDEAdDEaddeAddEAdDEAd0001"),
                )?;
                let after =
                    evm.call("GasPriceOracle", oracle, getter, json!([]), config.roles[0])?;
                assert_eq!(
                    contracts.get("GasPriceOracle")?.decode(getter, &after)?,
                    vec![DynSolValue::Bool(true)]
                );
            }
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires pinned contract artifacts; run just genesis-test"]
    fn generates_consistent_genesis_and_reuses_complete_outputs() -> Result<()> {
        let config = GenesisConfig {
            timestamp: Some(1800000000),
            salt: Some(B256::repeat_byte(1)),
            ..Default::default()
        };
        let contracts = ContractArtifacts::load(std::path::Path::new(&std::env::var(
            "BASE_GENESIS_ARTIFACTS",
        )?))?;
        let artifacts = GenesisGenerator::generate(&config, &contracts)?;
        let spec = BaseChainSpec::try_from_genesis(artifacts.l2.clone())?;
        assert_eq!(
            compute_jovian_base_fee(&spec, spec.genesis_header(), artifacts.l2.timestamp + 2)?,
            GenesisConfig::MIN_BASE_FEE
        );
        assert_eq!(
            artifacts.rollup.genesis.system_config.unwrap().min_base_fee,
            Some(GenesisConfig::MIN_BASE_FEE)
        );
        let mut evm = GenesisEvm::new(&contracts, config.l1_chain_id)?;
        for (address, account) in &artifacts.l1.alloc {
            if let Some(code) = &account.code {
                evm.code(*address, code.clone());
            }
            for (slot, value) in account.storage.iter().flatten() {
                evm.storage(*address, *slot, *value)?;
            }
        }
        let floor = evm.call(
            "SystemConfig",
            artifacts.rollup.l1_system_config_address,
            "minBaseFee",
            json!([]),
            config.roles[0],
        )?;
        assert_eq!(
            contracts.get("SystemConfig")?.decode("minBaseFee", &floor)?,
            vec![DynSolValue::Uint(U256::from(GenesisConfig::MIN_BASE_FEE), 64)]
        );
        assert_eq!(
            artifacts.rollup.genesis.l1.hash,
            ChainSpec::from(artifacts.l1.clone()).genesis_hash()
        );
        assert_eq!(
            artifacts.rollup.genesis.l2.hash,
            BaseChainSpec::try_from_genesis(artifacts.l2.clone())?.genesis_hash()
        );
        let key = artifacts
            .beacon
            .keystore
            .decrypt_keypair(artifacts.beacon.password.as_bytes())
            .map_err(|e| eyre::eyre!("{e:?}"))?;
        assert_eq!(key.pk.as_hex_string(), format!("0x{}", artifacts.beacon.keystore.pubkey()));
        let root = TempDir::new()?;
        let output = GenesisOutput::new(root.path());
        output.generate(&config, &contracts)?;
        let deposit_block_path = root.path().join("cl/deposit_contract_block.txt");
        let deposit_block = std::fs::read(&deposit_block_path)?;
        assert_eq!(serde_yaml::from_slice::<u64>(&deposit_block)?, 0);
        std::fs::remove_file(&deposit_block_path)?;
        assert!(
            output
                .generate(&config, &contracts)
                .unwrap_err()
                .to_string()
                .contains("missing genesis file")
        );
        std::fs::write(deposit_block_path, deposit_block)?;
        for name in ["el-bootnode-p2p-key.txt", "cl-bootnode-p2p-key.txt"] {
            let key = std::fs::read_to_string(root.path().join("l2").join(name))?;
            let _: PrivateKeySigner = key.parse()?;
        }
        let before = std::fs::read(root.path().join("jwt.hex"))?;
        output.generate(&config, &contracts)?;
        assert_eq!(before, std::fs::read(root.path().join("jwt.hex"))?);
        let changed = GenesisConfig { l2_chain_id: 42, ..config.clone() };
        assert!(output.generate(&changed, &contracts).is_err());
        let l1_marker = root.path().join(".setup-complete");
        let current = std::fs::read(&l1_marker)?;
        let mut old: Value = serde_json::from_slice(&current)?;
        old["version"] = json!(2);
        std::fs::write(&l1_marker, serde_json::to_vec(&old)?)?;
        assert!(
            output
                .generate(&config, &contracts)
                .unwrap_err()
                .to_string()
                .contains("genesis format")
        );
        std::fs::write(l1_marker, current)?;
        let marker = root.path().join("l2/.setup-complete");
        let completion = std::fs::read(&marker)?;
        std::fs::remove_file(&marker)?;
        let error = output.generate(&config, &contracts).unwrap_err();
        assert!(error.to_string().contains("regenerate both L1 and L2"));
        assert_eq!(before, std::fs::read(root.path().join("jwt.hex"))?);
        std::fs::write(marker, completion)?;
        std::fs::write(root.path().join("l2/rollup.json"), "{}")?;
        assert!(output.generate(&config, &contracts).is_err());
        Ok(())
    }
}
