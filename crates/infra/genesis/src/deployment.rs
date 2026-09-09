use std::collections::BTreeMap;

use alloy_dyn_abi::DynSolValue;
use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, B256, U256, address, keccak256};
use alloy_signer_local::{MnemonicBuilder, coins_bip39::English};
use base_common_genesis::{BaseUpgrade, RollupConfig};
use eyre::{Result, WrapErr, eyre};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{BeaconGenesis, ContractArtifacts, GenesisConfig, GenesisEvm};

/// Base's single-chain deployment, without the optional multiproof stack.
#[derive(Debug, Serialize, Deserialize)]
pub struct Deployment {
    /// L1 contract allocation state.
    pub l1: BTreeMap<Address, GenesisAccount>,
    /// L2 predeploy allocation state.
    pub l2: BTreeMap<Address, GenesisAccount>,
    /// Deployed chain addresses in the devnet's address document.
    pub addresses: BTreeMap<String, Address>,
}

impl Deployment {
    /// Executes the core Base `SystemDeploy` sequence using actual constructors and initializers.
    pub fn generate(
        config: &GenesisConfig,
        artifacts: &ContractArtifacts,
        salt: B256,
        rollup: &RollupConfig,
    ) -> Result<Self> {
        let schedule: Vec<_> = BaseUpgrade::CONTRACT_VARIANTS
            .iter()
            .map(|upgrade| {
                rollup
                    .upgrades
                    .activation(*upgrade)
                    .timestamp()
                    .map(|time| if time == 0 { rollup.genesis.l2_time } else { time })
                    .unwrap_or_default()
            })
            .collect();
        config.validate_schedule(&schedule, 0)?;
        let mut evm = GenesisEvm::new(artifacts, config.l1_chain_id)?;
        let caller = GenesisEvm::DEPLOYER;
        let owner = config.roles[0];
        let implementation_salt = Some(keccak256("op-stack-contract-impls-salt-v0"));
        let chain_salt = |name: &str| {
            Some(keccak256(
                DynSolValue::Tuple(vec![
                    DynSolValue::Uint(U256::from(config.l2_chain_id), 256),
                    DynSolValue::String(salt.to_string()),
                    DynSolValue::String(name.into()),
                ])
                .abi_encode_params(),
            ))
        };
        // Still required by Base SystemConfig for guardian/paused; no OPCM or Superchain registry.
        let admin = evm.deploy("ProxyAdmin", json!([caller]), caller, None)?;
        let superchain_impl = evm.deploy(
            "SuperchainConfig",
            json!([owner, Address::ZERO]),
            caller,
            implementation_salt,
        )?;
        let superchain = evm.deploy("Proxy", json!([admin]), caller, None)?;
        evm.call("ProxyAdmin", admin, "upgrade", json!([superchain, superchain_impl]), caller)?;
        evm.call("ProxyAdmin", admin, "transferOwnership", json!([owner]), caller)?;
        let mut addresses = BTreeMap::from([
            ("SuperchainProxyAdmin".into(), admin),
            ("SuperchainConfigImpl".into(), superchain_impl),
            ("SuperchainConfigProxy".into(), superchain),
        ]);
        for (name, args) in [
            ("ProtocolVersions", json!([])),
            ("SystemConfig", json!([])),
            ("L1CrossDomainMessenger", json!([])),
            ("L1ERC721Bridge", json!([])),
            ("L1StandardBridge", json!([])),
            ("OptimismMintableERC20Factory", json!([])),
            ("OptimismPortal2", json!([604800])),
            ("DelayedWETH", json!([302400])),
            ("DisputeGameFactory", json!([])),
            ("AnchorStateRegistry", json!([302400])),
        ] {
            let deployed = evm.deploy(name, args, caller, implementation_salt)?;
            if name == "ProtocolVersions" {
                let notice = evm.call(name, deployed, "MIN_NOTICE", json!([]), caller)?;
                let decoded = artifacts.get(name)?.decode("MIN_NOTICE", &notice)?;
                let [DynSolValue::Uint(notice, 64)] = decoded.as_slice() else {
                    return Err(eyre!("ProtocolVersions.MIN_NOTICE ABI changed"));
                };
                config.validate_schedule(&schedule, (*notice).try_into()?)?;
            }
            addresses.insert(format!("{}Impl", name.trim_end_matches('2')), deployed);
        }
        let admin = evm.deploy("ProxyAdmin", json!([caller]), caller, chain_salt("ProxyAdmin"))?;
        addresses.insert("ProxyAdmin".into(), admin);
        let helper = evm.deploy(
            "AddressManagerDeployer",
            json!([chain_salt("AddressManager"), admin]),
            caller,
            chain_salt("AddressManagerDeployer"),
        )?;
        let result =
            evm.call("AddressManagerDeployer", helper, "addressManager", json!([]), caller)?;
        let decoded = artifacts.get("AddressManagerDeployer")?.decode("addressManager", &result)?;
        let [DynSolValue::Address(manager)] = decoded.as_slice() else {
            return Err(eyre!("AddressManagerDeployer.addressManager ABI changed"));
        };
        let manager = *manager;
        addresses.insert("AddressManager".into(), manager);
        evm.call("ProxyAdmin", admin, "setAddressManager", json!([manager]), caller)?;
        for name in [
            "L1ERC721Bridge",
            "OptimismPortal",
            "SystemConfig",
            "OptimismMintableERC20Factory",
            "DisputeGameFactory",
            "AnchorStateRegistry",
            "DelayedWETH",
            "ProtocolVersions",
        ] {
            addresses.insert(
                format!("{name}Proxy"),
                evm.deploy("Proxy", json!([admin]), caller, chain_salt(name))?,
            );
        }
        let bridge = evm.deploy(
            "L1ChugSplashProxy",
            json!([admin]),
            caller,
            chain_salt("L1StandardBridge"),
        )?;
        addresses.insert("L1StandardBridgeProxy".into(), bridge);
        evm.call("ProxyAdmin", admin, "setProxyType", json!([bridge, 1]), caller)?;
        let messenger_name = "OVM_L1CrossDomainMessenger";
        let messenger = evm.deploy(
            "ResolvedDelegateProxy",
            json!([manager, messenger_name]),
            caller,
            chain_salt("L1CrossDomainMessenger"),
        )?;
        addresses.insert("L1CrossDomainMessengerProxy".into(), messenger);
        evm.call("ProxyAdmin", admin, "setProxyType", json!([messenger, 2]), caller)?;
        evm.call(
            "ProxyAdmin",
            admin,
            "setImplementationName",
            json!([messenger, messenger_name]),
            caller,
        )?;
        let system = addresses["SystemConfigProxy"];
        let system_addresses = json!({
            "l1CrossDomainMessenger": messenger, "l1ERC721Bridge": addresses["L1ERC721BridgeProxy"],
            "l1StandardBridge": bridge, "optimismPortal": addresses["OptimismPortalProxy"],
            "optimismMintableERC20Factory": addresses["OptimismMintableERC20FactoryProxy"],
            "delayedWETH": addresses["DelayedWETHProxy"],
        });
        for (name, args) in [
            ("L1ERC721Bridge", json!([messenger, system])),
            (
                "SystemConfig",
                json!([owner, 1368, 801949, config.roles[2].into_word(), 60000000,
                config.roles[1], {"maxResourceLimit": 20000000, "elasticityMultiplier": 10,
                "baseFeeMaxChangeDenominator": 8, "minimumBaseFee": 1000000000,
                "systemTxMaxGas": 1000000, "maximumBaseFee": u128::MAX.to_string()},
                rollup.batch_inbox_address, system_addresses, config.l2_chain_id, superchain]),
            ),
            ("OptimismPortal2", json!([system, addresses["AnchorStateRegistryProxy"]])),
            ("OptimismMintableERC20Factory", json!([bridge])),
            ("L1CrossDomainMessenger", json!([system, addresses["OptimismPortalProxy"]])),
            ("L1StandardBridge", json!([messenger, system])),
            ("DisputeGameFactory", json!([caller])),
            (
                "AnchorStateRegistry",
                json!([system, addresses["DisputeGameFactoryProxy"],
                {"root": B256::from(U256::from(1)), "l2SequenceNumber": 0}, 621]),
            ),
            ("ProtocolVersions", json!([Address::ZERO, schedule, config.minimum_version])),
            ("DelayedWETH", json!([system])),
        ] {
            let label = name.trim_end_matches('2');
            let data = artifacts
                .get(name)?
                .encode("initialize", &args)
                .wrap_err_with(|| format!("{name}.initialize arguments"))?;
            evm.call(
                "ProxyAdmin",
                admin,
                "upgradeAndCall",
                json!([
                    addresses[&format!("{label}Proxy")],
                    addresses[&format!("{label}Impl")],
                    data
                ]),
                caller,
            )
            .wrap_err_with(|| format!("initializing {name} through ProxyAdmin.upgradeAndCall"))?;
        }
        evm.call(
            "SystemConfig",
            system,
            "setMinBaseFee",
            json!([GenesisConfig::MIN_BASE_FEE]),
            owner,
        )?;
        evm.call(
            "DisputeGameFactory",
            addresses["DisputeGameFactoryProxy"],
            "transferOwnership",
            json!([owner]),
            caller,
        )?;
        evm.call("ProxyAdmin", admin, "transferOwnership", json!([owner]), caller)?;
        evm.preinstalls(config.l1_chain_id)?;
        let l2 = Self::l2(config, artifacts, &addresses, rollup)?;
        Ok(Self { l1: evm.allocs(), l2, addresses })
    }

    /// Implements Base's L2 genesis rules; runtime code is loaded from the same pinned contracts.
    pub fn l2(
        config: &GenesisConfig,
        artifacts: &ContractArtifacts,
        l1: &BTreeMap<String, Address>,
        rollup: &RollupConfig,
    ) -> Result<BTreeMap<Address, GenesisAccount>> {
        let mut evm = GenesisEvm::new(artifacts, config.l2_chain_id)?;
        evm.evm
            .ctx
            .journaled_state
            .database
            .cache
            .accounts
            .entry(GenesisEvm::L2_DEPLOYER)
            .or_default()
            .info
            .nonce = 1;
        let admin_slot =
            B256::from(U256::from_be_bytes(keccak256("eip1967.proxy.admin").0) - U256::from(1));
        let impl_slot = B256::from(
            U256::from_be_bytes(keccak256("eip1967.proxy.implementation").0) - U256::from(1),
        );
        let implementation = |address: Address| {
            let mut code = address!("c0d3c0d3c0d3c0d3c0d3c0d3c0d3c0d3c0d30000");
            code.as_mut_slice()[18..].copy_from_slice(&address.as_slice()[18..]);
            code
        };
        for index in 0u16..256 {
            let address = Address::from_word(B256::from(U256::from(index)));
            evm.evm
                .ctx
                .journaled_state
                .database
                .cache
                .accounts
                .entry(address)
                .or_default()
                .info
                .balance = U256::from(1);
        }
        let proxy_code = artifacts.get("Proxy")?.code(true)?;
        for index in 0u16..2048 {
            let mut address = address!("4200000000000000000000000000000000000000");
            address.as_mut_slice()[18..].copy_from_slice(&index.to_be_bytes());
            if address == artifacts.predeploy("WETH")? {
                continue;
            }
            evm.code(address, proxy_code.clone());
            evm.storage(address, admin_slot, artifacts.predeploy("PROXY_ADMIN")?.into_word())?;
        }
        for (name, key) in [
            ("WETH", "WETH"),
            ("L2CrossDomainMessenger", "L2_CROSS_DOMAIN_MESSENGER"),
            ("GasPriceOracle", "GAS_PRICE_ORACLE"),
            ("L2StandardBridge", "L2_STANDARD_BRIDGE"),
            ("ProxyAdmin", "PROXY_ADMIN"),
            ("SequencerFeeVault", "SEQUENCER_FEE_WALLET"),
            ("OptimismMintableERC20Factory", "OPTIMISM_MINTABLE_ERC20_FACTORY"),
            ("L2ERC721Bridge", "L2_ERC721_BRIDGE"),
            ("L1Block", "L1_BLOCK_ATTRIBUTES"),
            ("L2ToL1MessagePasser", "L2_TO_L1_MESSAGE_PASSER"),
            ("OptimismMintableERC721Factory", "OPTIMISM_MINTABLE_ERC721_FACTORY"),
            ("BaseFeeVault", "BASE_FEE_VAULT"),
            ("L1FeeVault", "L1_FEE_VAULT"),
            ("OperatorFeeVault", "OPERATOR_FEE_VAULT"),
            ("SchemaRegistry", "SCHEMA_REGISTRY"),
            ("EAS", "EAS"),
            ("BaseTime", "BASE_TIME"),
        ] {
            let proxy = artifacts.predeploy(key)?;
            let target = if name == "WETH" { proxy } else { implementation(proxy) };
            if proxy != target {
                evm.storage(proxy, impl_slot, target.into_word())?;
            }
            if ["OptimismMintableERC721Factory", "EAS"].contains(&name) {
                let args = if name == "EAS" {
                    json!([])
                } else {
                    json!([artifacts.predeploy("L2_ERC721_BRIDGE")?, config.l1_chain_id])
                };
                let temporary = evm.deploy(name, args, GenesisEvm::L2_DEPLOYER, None)?;
                let info = evm
                    .evm
                    .ctx
                    .journaled_state
                    .database
                    .cache
                    .accounts
                    .remove(&temporary)
                    .ok_or_else(|| eyre!("{name} constructor did not create account {temporary}"))?
                    .info;
                let code = evm
                    .evm
                    .ctx
                    .journaled_state
                    .database
                    .cache
                    .contracts
                    .get(&info.code_hash)
                    .ok_or_else(|| eyre!("{name} constructor did not produce runtime code"))?
                    .original_bytes();
                evm.code(target, code);
            } else {
                evm.code(target, artifacts.get(name)?.code(true)?);
            }
            if name == "ProxyAdmin" {
                for address in [proxy, target] {
                    evm.storage(address, B256::ZERO, config.roles[0].into_word())?;
                }
            } else if name.ends_with("FeeVault") {
                evm.storage(target, admin_slot, artifacts.predeploy("PROXY_ADMIN")?.into_word())?;
                evm.call(
                    name,
                    target,
                    "initialize",
                    json!([Address::ZERO, U256::MAX, 0]),
                    config.roles[0],
                )?;
                evm.call(
                    name,
                    proxy,
                    "initialize",
                    json!([config.roles[0], "10000000000000000000", 1]),
                    config.roles[0],
                )?;
            } else {
                let counterpart = match name {
                    "L2CrossDomainMessenger" => Some(
                        *l1.get("L1CrossDomainMessengerProxy")
                            .ok_or_else(|| eyre!("missing L1CrossDomainMessengerProxy"))?,
                    ),
                    "L2StandardBridge" => Some(
                        *l1.get("L1StandardBridgeProxy")
                            .ok_or_else(|| eyre!("missing L1StandardBridgeProxy"))?,
                    ),
                    "L2ERC721Bridge" => Some(
                        *l1.get("L1ERC721BridgeProxy")
                            .ok_or_else(|| eyre!("missing L1ERC721BridgeProxy"))?,
                    ),
                    "OptimismMintableERC20Factory" => {
                        Some(artifacts.predeploy("L2_STANDARD_BRIDGE")?)
                    }
                    _ => None,
                };
                if let Some(counterpart) = counterpart {
                    evm.call(
                        name,
                        target,
                        "initialize",
                        json!([Address::ZERO]),
                        GenesisEvm::L2_DEPLOYER,
                    )?;
                    evm.call(
                        name,
                        proxy,
                        "initialize",
                        json!([counterpart]),
                        GenesisEvm::L2_DEPLOYER,
                    )?;
                }
            }
        }
        evm.preinstalls(config.l2_chain_id)?;
        for index in 0..30 {
            let signer = MnemonicBuilder::<English>::default()
                .phrase(BeaconGenesis::MNEMONIC)
                .index(index)?
                .build()?;
            evm.evm
                .ctx
                .journaled_state
                .database
                .cache
                .accounts
                .entry(signer.address())
                .or_default()
                .info
                .balance = U256::from(10_000) * U256::from(10).pow(U256::from(18));
        }
        for (upgrade, method) in [
            (BaseUpgrade::Ecotone, "setEcotone"),
            (BaseUpgrade::Fjord, "setFjord"),
            (BaseUpgrade::Isthmus, "setIsthmus"),
            (BaseUpgrade::Jovian, "setJovian"),
        ] {
            if rollup
                .upgrades
                .activation(upgrade)
                .timestamp()
                .is_none_or(|time| time > rollup.genesis.l2_time)
            {
                continue;
            }
            evm.call(
                "GasPriceOracle",
                artifacts.predeploy("GAS_PRICE_ORACLE")?,
                method,
                json!([]),
                address!("DeaDDEaDDeAdDeAdDEAdDEaddeAddEAdDEAd0001"),
            )?;
        }
        Ok(evm.allocs())
    }
}

#[cfg(test)]
mod tests {
    use std::{env, path::Path};

    use super::*;

    #[test]
    #[ignore = "requires pinned contract artifacts; run just genesis-test"]
    fn deploys_real_registry_with_live_authorization_and_schedule_guards() -> Result<()> {
        let artifacts = ContractArtifacts::load(Path::new(&env::var("BASE_GENESIS_ARTIFACTS")?))?;
        let config = GenesisConfig::default();
        let timestamp = 1_800_000_000;
        let mut schedule = vec![timestamp; 14];
        schedule[7] = 0; // Permanently empty Pectra slot.
        schedule[13] = 0; // Leave trailing Denim mutable for this test.
        let mut rollup = RollupConfig::default();
        rollup.genesis.l2_time = timestamp;
        for (upgrade, time) in BaseUpgrade::CONTRACT_VARIANTS.into_iter().zip(&schedule) {
            if *time != 0 {
                rollup.upgrades.set_activation_timestamp(upgrade, *time);
            }
        }
        let deployment = Deployment::generate(&config, &artifacts, B256::repeat_byte(1), &rollup)?;
        let mut evm = GenesisEvm::new(&artifacts, config.l1_chain_id)?;
        for (address, account) in &deployment.l1 {
            if let Some(code) = &account.code {
                evm.code(*address, code.clone());
            }
            for (slot, value) in account.storage.iter().flatten() {
                evm.storage(*address, *slot, *value)?;
            }
        }
        evm.evm.ctx.block.timestamp = U256::from(timestamp);
        let registry = deployment.addresses["ProtocolVersionsProxy"];
        let owner = config.roles[0];
        let abi = artifacts.get("ProtocolVersions")?;
        let initial = evm.call("ProtocolVersions", registry, "scheduleId", json!([]), owner)?;
        let before = evm.call("ProtocolVersions", registry, "getSchedule", json!([]), owner)?;
        assert_eq!(
            abi.decode("getSchedule", &before)?,
            vec![DynSolValue::Array(
                schedule.iter().map(|time| DynSolValue::Uint(U256::from(*time), 64)).collect()
            )]
        );
        // Neither an unauthorized owner nor a short-notice change may modify the schedule.
        for (id, time, caller) in [
            (13, timestamp + 7200, config.roles[1]),
            (13, timestamp + 60, owner),
            (12, timestamp + 7200, owner),
        ] {
            assert!(
                evm.call("ProtocolVersions", registry, "setTimestamp", json!([id, time]), caller)
                    .is_err()
            );
        }
        assert_eq!(
            initial,
            evm.call("ProtocolVersions", registry, "scheduleId", json!([]), owner)?
        );
        evm.call(
            "ProtocolVersions",
            registry,
            "setTimestamp",
            json!([13, timestamp + 7200]),
            owner,
        )?;
        schedule[13] = timestamp + 7200;
        let expected = schedule.iter().enumerate().fold(B256::ZERO, |hash, (id, time)| {
            keccak256(
                DynSolValue::Tuple(vec![
                    DynSolValue::FixedBytes(hash, 32),
                    DynSolValue::Uint(U256::from(id), 256),
                    DynSolValue::Uint(U256::from(*time), 64),
                ])
                .abi_encode_params(),
            )
        });
        assert_eq!(
            evm.call("ProtocolVersions", registry, "scheduleId", json!([]), owner)?.as_ref(),
            expected.as_slice()
        );
        evm.call(
            "ProtocolVersions",
            registry,
            "setMinimumProtocolVersion",
            json!([4294967297u64]),
            owner,
        )?;
        let minimum =
            evm.call("ProtocolVersions", registry, "minimumProtocolVersion", json!([]), owner)?;
        assert_eq!(
            abi.decode("minimumProtocolVersion", &minimum)?,
            vec![DynSolValue::Uint(U256::from(4294967297u64), 256)]
        );
        // Reads traverse the real Base proxy/implementation graph.
        let paused = evm.call(
            "SystemConfig",
            deployment.addresses["SystemConfigProxy"],
            "paused",
            json!([]),
            owner,
        )?;
        assert_eq!(
            artifacts.get("SystemConfig")?.decode("paused", &paused)?,
            vec![DynSolValue::Bool(false)]
        );
        let l2_bridge = artifacts.predeploys["L2_STANDARD_BRIDGE"];
        assert!(!deployment.l2[&l2_bridge].code.as_ref().unwrap().is_empty());
        assert_eq!(deployment.l2[&owner].balance, U256::from(10_000_000_000_000_000_000_000u128));
        Ok(())
    }
}
