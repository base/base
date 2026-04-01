//! Live L1 deployment and L2 artifact extraction.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use alloy_network::Ethereum;
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use eyre::{Result, WrapErr, bail};
use serde::Serialize;
use serde_json::Value;
use url::Url;

use crate::{
    config::DeployerConfig,
    devnet::{BUILDER_ENODE_ID, l2_intent_toml, role_accounts, sequencer_p2p_keys},
    external::{capture_stdout_to_path, run_command},
    genesis::{patch_base_v1_activation_files, write_rollup_conductor_file},
};

/// Output metadata for a live L1 deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeployL1Output {
    /// Manifest file describing the live deployment.
    pub(crate) manifest_path: PathBuf,
    /// Persisted `op-deployer` workdir.
    pub(crate) workdir: PathBuf,
    /// Raw L1 address output from `op-deployer inspect l1`.
    pub(crate) l1_addresses_path: PathBuf,
}

/// Output metadata for extracted L2 artifacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeployL2Output {
    /// Generated L2 genesis file.
    pub(crate) genesis_path: PathBuf,
    /// Generated rollup config.
    pub(crate) rollup_path: PathBuf,
    /// Generated rollup config for op-conductor.
    pub(crate) rollup_conductor_path: PathBuf,
    /// Raw L1 address output from `op-deployer inspect l1`.
    pub(crate) l1_addresses_path: PathBuf,
}

/// Deploys OP Stack L1 contracts to a live L1.
pub(crate) async fn deploy_l1(
    config: DeployerConfig,
    output_dir_override: Option<PathBuf>,
    l1_rpc: &str,
) -> Result<DeployL1Output> {
    let l1 = inspect_l1_rpc(l1_rpc).await?;
    let resolved = config.resolve_with_l1_chain_id(output_dir_override, l1.chain_id)?;
    let paths = DeploymentPaths::new(&resolved.output_dir);
    paths.create_directories()?;
    paths.write_chain_ids(resolved.chain_ids())?;
    ensure_live_intent(&paths.l2_intent, resolved.l1_chain_id, resolved.l2_chain_id)?;

    if !paths.live_state.exists() {
        if paths.live_workdir.exists() {
            fs::remove_dir_all(&paths.live_workdir)
                .wrap_err_with(|| format!("Failed to remove {}", paths.live_workdir.display()))?;
        }
        fs::create_dir_all(&paths.live_workdir)
            .wrap_err_with(|| format!("Failed to create {}", paths.live_workdir.display()))?;

        run_command(
            Command::new("op-deployer")
                .arg("init")
                .arg("--l1-chain-id")
                .arg(resolved.l1_chain_id.to_string())
                .arg("--l2-chain-ids")
                .arg(resolved.l2_chain_id.to_string())
                .arg("--intent-type")
                .arg("custom")
                .arg("--workdir")
                .arg(&paths.live_workdir),
            "initialize live op-deployer workdir",
        )?;

        fs::copy(&paths.l2_intent, paths.live_workdir.join("intent.toml"))
            .wrap_err("Failed to copy intent.toml into live op-deployer workdir")?;

        run_command(
            Command::new("op-deployer")
                .arg("apply")
                .arg("--workdir")
                .arg(&paths.live_workdir)
                .arg("--deployment-target")
                .arg("live")
                .arg("--l1-rpc-url")
                .arg(l1_rpc)
                .arg("--private-key")
                .arg(paths.deployer_private_key_hex()),
            "deploy OP Stack contracts to L1",
        )?;
    }

    capture_stdout_to_path(
        Command::new("op-deployer")
            .arg("inspect")
            .arg("l1")
            .arg("--workdir")
            .arg(&paths.live_workdir)
            .arg(resolved.l2_chain_id.to_string()),
        &paths.l1_addresses,
        "inspect deployed L1 addresses",
    )?;

    let addresses = read_json(&paths.l1_addresses)?;
    let manifest = DeploymentManifest::new(
        l1_rpc,
        l1.chain_id,
        resolved.l2_chain_id,
        &paths,
        addresses,
    );
    write_json(&paths.deployment_manifest, &manifest)?;

    Ok(DeployL1Output {
        manifest_path: paths.deployment_manifest,
        workdir: paths.live_workdir,
        l1_addresses_path: paths.l1_addresses,
    })
}

/// Extracts L2 genesis and rollup configuration from a live deployment workdir.
pub(crate) async fn deploy_l2(
    config: DeployerConfig,
    output_dir_override: Option<PathBuf>,
    l1_rpc: Option<&str>,
) -> Result<DeployL2Output> {
    let resolved = match l1_rpc {
        Some(url) => {
            let l1 = inspect_l1_rpc(url).await?;
            config
                .clone()
                .resolve_with_l1_chain_id(output_dir_override.clone(), l1.chain_id)?
        }
        None => config.clone().resolve(output_dir_override.clone())?,
    };

    let paths = DeploymentPaths::new(&resolved.output_dir);
    paths.create_directories()?;

    if !paths.live_state.exists() {
        if let Some(url) = l1_rpc {
            deploy_l1(config, output_dir_override, url).await?;
        } else {
            bail!(
                "No live deployment state found at {}. Run `base-deployer deploy-l1 --l1-rpc <url>` first.",
                paths.live_workdir.display()
            );
        }
    }

    capture_stdout_to_path(
        Command::new("op-deployer")
            .arg("inspect")
            .arg("genesis")
            .arg("--workdir")
            .arg(&paths.live_workdir)
            .arg(resolved.l2_chain_id.to_string()),
        &paths.l2_genesis,
        "inspect live L2 genesis",
    )?;
    capture_stdout_to_path(
        Command::new("op-deployer")
            .arg("inspect")
            .arg("rollup")
            .arg("--workdir")
            .arg(&paths.live_workdir)
            .arg(resolved.l2_chain_id.to_string()),
        &paths.l2_rollup,
        "inspect live rollup config",
    )?;
    capture_stdout_to_path(
        Command::new("op-deployer")
            .arg("inspect")
            .arg("l1")
            .arg("--workdir")
            .arg(&paths.live_workdir)
            .arg(resolved.l2_chain_id.to_string()),
        &paths.l1_addresses,
        "inspect deployed L1 addresses",
    )?;

    if let Some(base_v1_block) = resolved.l2_base_v1_block {
        patch_base_v1_activation_files(&paths.l2_rollup, &paths.l2_genesis, base_v1_block)?;
    }
    write_rollup_conductor_file(&paths.l2_rollup, &paths.l2_rollup_conductor)?;
    write_p2p_material(&paths)?;

    Ok(DeployL2Output {
        genesis_path: paths.l2_genesis,
        rollup_path: paths.l2_rollup,
        rollup_conductor_path: paths.l2_rollup_conductor,
        l1_addresses_path: paths.l1_addresses,
    })
}

#[derive(Debug, Clone)]
struct DeploymentPaths {
    l1_dir: PathBuf,
    l2_dir: PathBuf,
    op_deployer_dir: PathBuf,
    live_workdir: PathBuf,
    live_state: PathBuf,
    deployment_manifest: PathBuf,
    l1_addresses: PathBuf,
    l2_intent: PathBuf,
    l2_genesis: PathBuf,
    l2_rollup: PathBuf,
    l2_rollup_conductor: PathBuf,
    chain_ids: PathBuf,
    builder_p2p_key: PathBuf,
    builder_enode_id: PathBuf,
    sequencer1_p2p_key: PathBuf,
    sequencer2_p2p_key: PathBuf,
}

impl DeploymentPaths {
    fn new(output_dir: &Path) -> Self {
        let l1_dir = output_dir.join("l1");
        let l2_dir = output_dir.join("l2");
        let op_deployer_dir = output_dir.join("op-deployer");
        let live_workdir = op_deployer_dir.join("live");

        Self {
            l1_dir: l1_dir.clone(),
            l2_dir: l2_dir.clone(),
            op_deployer_dir: op_deployer_dir.clone(),
            live_workdir: live_workdir.clone(),
            live_state: live_workdir.join("state.json"),
            deployment_manifest: l1_dir.join("deployment-manifest.json"),
            l1_addresses: l2_dir.join("l1-addresses.json"),
            l2_intent: l2_dir.join("intent.toml"),
            l2_genesis: l2_dir.join("genesis.json"),
            l2_rollup: l2_dir.join("rollup.json"),
            l2_rollup_conductor: l2_dir.join("rollup-conductor.json"),
            chain_ids: output_dir.join("chain-ids.json"),
            builder_p2p_key: l2_dir.join("builder-p2p-key.txt"),
            builder_enode_id: l2_dir.join("builder-enode-id.txt"),
            sequencer1_p2p_key: l2_dir.join("sequencer-1-p2p-key.txt"),
            sequencer2_p2p_key: l2_dir.join("sequencer-2-p2p-key.txt"),
        }
    }

    fn create_directories(&self) -> Result<()> {
        fs::create_dir_all(&self.l1_dir)
            .wrap_err_with(|| format!("Failed to create {}", self.l1_dir.display()))?;
        fs::create_dir_all(&self.l2_dir)
            .wrap_err_with(|| format!("Failed to create {}", self.l2_dir.display()))?;
        fs::create_dir_all(&self.op_deployer_dir)
            .wrap_err_with(|| format!("Failed to create {}", self.op_deployer_dir.display()))?;
        Ok(())
    }

    fn write_chain_ids(&self, chain_ids: crate::config::ChainIds) -> Result<()> {
        write_json(&self.chain_ids, &chain_ids)
    }

    fn deployer_private_key_hex(&self) -> String {
        format!("0x{}", hex::encode(role_accounts().deployer.private_key))
    }
}

#[derive(Debug, Clone, Serialize)]
struct DeploymentManifest {
    l1_rpc_url: String,
    l1_chain_id: u64,
    l2_chain_id: u64,
    deployer_address: String,
    workdir: String,
    contracts: StandardContracts,
    addresses: Value,
}

impl DeploymentManifest {
    fn new(
        l1_rpc_url: &str,
        l1_chain_id: u64,
        l2_chain_id: u64,
        paths: &DeploymentPaths,
        addresses: Value,
    ) -> Self {
        Self {
            l1_rpc_url: l1_rpc_url.to_string(),
            l1_chain_id,
            l2_chain_id,
            deployer_address: format!("{:#x}", role_accounts().deployer.address),
            workdir: paths.live_workdir.display().to_string(),
            contracts: StandardContracts::from_addresses(&addresses),
            addresses,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
struct StandardContracts {
    optimism_portal: Option<String>,
    l1_cross_domain_messenger: Option<String>,
    l1_standard_bridge: Option<String>,
    system_config: Option<String>,
    address_manager: Option<String>,
}

impl StandardContracts {
    fn from_addresses(addresses: &Value) -> Self {
        Self {
            optimism_portal: find_address(addresses, &["OptimismPortalProxy", "OptimismPortal"]),
            l1_cross_domain_messenger: find_address(
                addresses,
                &["L1CrossDomainMessengerProxy", "L1CrossDomainMessenger"],
            ),
            l1_standard_bridge: find_address(
                addresses,
                &["L1StandardBridgeProxy", "L1StandardBridge"],
            ),
            system_config: find_address(addresses, &["SystemConfigProxy", "SystemConfig"]),
            address_manager: find_address(addresses, &["AddressManager"]),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct L1RpcInfo {
    chain_id: u64,
}

async fn inspect_l1_rpc(l1_rpc: &str) -> Result<L1RpcInfo> {
    let url: Url = l1_rpc.parse().wrap_err("Failed to parse --l1-rpc URL")?;
    let provider = RootProvider::<Ethereum>::new(RpcClient::builder().http(url));
    let chain_id = provider
        .get_chain_id()
        .await
        .wrap_err("Failed to query chain ID from L1 RPC")?;

    provider
        .get_block_number()
        .await
        .wrap_err("Failed to query latest block from L1 RPC")?;

    Ok(L1RpcInfo { chain_id })
}

fn ensure_live_intent(path: &Path, l1_chain_id: u64, l2_chain_id: u64) -> Result<()> {
    fs::write(path, l2_intent_toml(l1_chain_id, l2_chain_id))
        .wrap_err_with(|| format!("Failed to write {}", path.display()))
}

fn write_p2p_material(paths: &DeploymentPaths) -> Result<()> {
    let roles = role_accounts();
    let [seq1_key, seq2_key] = sequencer_p2p_keys();
    fs::write(&paths.builder_p2p_key, format!("{:#x}\n", roles.builder.private_key))
        .wrap_err_with(|| format!("Failed to write {}", paths.builder_p2p_key.display()))?;
    fs::write(&paths.builder_enode_id, format!("{BUILDER_ENODE_ID}\n"))
        .wrap_err_with(|| format!("Failed to write {}", paths.builder_enode_id.display()))?;
    fs::write(&paths.sequencer1_p2p_key, format!("{:#x}\n", seq1_key))
        .wrap_err_with(|| format!("Failed to write {}", paths.sequencer1_p2p_key.display()))?;
    fs::write(&paths.sequencer2_p2p_key, format!("{:#x}\n", seq2_key))
        .wrap_err_with(|| format!("Failed to write {}", paths.sequencer2_p2p_key.display()))?;
    Ok(())
}

fn find_address(addresses: &Value, candidates: &[&str]) -> Option<String> {
    let object = addresses.as_object()?;

    for candidate in candidates {
        if let Some(address) = object.get(*candidate).and_then(extract_address) {
            return Some(address);
        }
    }

    for (key, value) in object {
        let normalized_key = key.to_ascii_lowercase();
        if candidates
            .iter()
            .any(|candidate| normalized_key.contains(&candidate.to_ascii_lowercase()))
        {
            if let Some(address) = extract_address(value) {
                return Some(address);
            }
        }
    }

    None
}

fn extract_address(value: &Value) -> Option<String> {
    match value {
        Value::String(address) if address.starts_with("0x") => Some(address.clone()),
        Value::Object(object) => object.get("address").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

fn read_json(path: &Path) -> Result<Value> {
    let contents = fs::read_to_string(path)
        .wrap_err_with(|| format!("Failed to read {}", path.display()))?;
    serde_json::from_str(&contents)
        .wrap_err_with(|| format!("Failed to parse JSON at {}", path.display()))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let contents = serde_json::to_string_pretty(value).wrap_err("Failed to serialize JSON")?;
    fs::write(path, format!("{contents}\n"))
        .wrap_err_with(|| format!("Failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::StandardContracts;

    #[test]
    fn extracts_standard_contracts_from_addresses() {
        let contracts = StandardContracts::from_addresses(&json!({
            "OptimismPortalProxy": "0x1000000000000000000000000000000000000001",
            "L1CrossDomainMessengerProxy": "0x1000000000000000000000000000000000000002",
            "L1StandardBridgeProxy": "0x1000000000000000000000000000000000000003",
            "SystemConfigProxy": "0x1000000000000000000000000000000000000004",
            "AddressManager": "0x1000000000000000000000000000000000000005",
        }));

        assert_eq!(
            contracts.optimism_portal.as_deref(),
            Some("0x1000000000000000000000000000000000000001")
        );
        assert_eq!(
            contracts.l1_cross_domain_messenger.as_deref(),
            Some("0x1000000000000000000000000000000000000002")
        );
        assert_eq!(
            contracts.l1_standard_bridge.as_deref(),
            Some("0x1000000000000000000000000000000000000003")
        );
        assert_eq!(
            contracts.system_config.as_deref(),
            Some("0x1000000000000000000000000000000000000004")
        );
        assert_eq!(
            contracts.address_manager.as_deref(),
            Some("0x1000000000000000000000000000000000000005")
        );
    }
}
