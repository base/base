//! Contract deployment utilities for E2E tests.
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_rpc_types_eth::{transaction::request::TransactionInput, TransactionRequest};
use alloy_sol_types::SolConstructor;
use alloy_transport_http::reqwest::Url;
use anyhow::{anyhow, Context, Result};
use op_succinct_bindings::mock_permissioned_dispute_game::MockPermissionedDisputeGame;
use op_succinct_signer_utils::SignerLock;
use serde::{Deserialize, Serialize};
use tracing::info;

/// Container for deployed contracts
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployedContracts {
    pub factory: Address,
    pub portal: Address,
    pub access_manager: Address,
    pub game_implementation: Address,
}

/// Typed structure for forge script output
#[derive(Debug, Deserialize)]
struct ForgeOutput {
    returns: ForgeReturns,
    success: bool,
}

/// Forge returns structure containing contract addresses
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForgeReturns {
    game_implementation: AddressField,
    #[allow(dead_code)]
    sp1_verifier: AddressField,
    #[allow(dead_code)]
    anchor_state_registry: AddressField,
    access_manager: AddressField,
    optimism_portal2: AddressField,
    factory_proxy: AddressField,
}

/// Address field structure with custom deserialization
#[derive(Debug, Deserialize)]
struct AddressField {
    #[allow(dead_code)]
    internal_type: String,
    #[serde(deserialize_with = "deserialize_address")]
    value: Address,
}

/// Custom deserializer for Address from string
fn deserialize_address<'de, D>(deserializer: D) -> Result<Address, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    s.parse::<Address>().map_err(serde::de::Error::custom)
}

/// Parse forge script output to extract contract addresses
fn parse_forge_output(output: &str) -> Result<DeployedContracts> {
    // Get the first line which should be valid JSON
    let json_line = output.lines().next().ok_or_else(|| anyhow!("No output from forge script"))?;

    // Parse the forge output structure
    let forge_output: ForgeOutput = serde_json::from_str(json_line)
        .map_err(|e| anyhow!("Failed to parse forge output JSON: {}\n{}", e, json_line))?;

    if !forge_output.success {
        return Err(anyhow!("Forge script execution was not successful"));
    }

    // Extract addresses directly (already parsed by custom deserializer)
    Ok(DeployedContracts {
        factory: forge_output.returns.factory_proxy.value,
        portal: forge_output.returns.optimism_portal2.value,
        access_manager: forge_output.returns.access_manager.value,
        game_implementation: forge_output.returns.game_implementation.value,
    })
}

/// Deploy all contracts required for E2E testing
pub async fn deploy_test_contracts(rpc_url: &str, private_key: &str) -> Result<DeployedContracts> {
    info!("Deploying test contracts using forge script");

    // Run the forge script to deploy contracts
    let contracts_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("contracts");

    let output = std::process::Command::new("forge")
        .arg("script")
        .arg("script/fp/DeployOPSuccinctFDG.s.sol")
        .arg("--broadcast")
        .arg("--rpc-url")
        .arg(rpc_url)
        .arg("--private-key")
        .arg(private_key)
        .arg("--json")
        .env("RUST_LOG", "off")
        .current_dir(&contracts_dir)
        .output()
        .map_err(|e| anyhow!("Failed to execute forge script: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("Forge script failed: {}", stderr));
    }

    // Parse the JSON output to extract contract addresses
    let stdout = String::from_utf8_lossy(&output.stdout);
    let deployed_contracts = parse_forge_output(&stdout)?;

    info!("✓ Contracts deployed successfully");
    info!("  Factory: {}", deployed_contracts.factory);
    info!("  Portal: {}", deployed_contracts.portal);
    info!("  Access Manager: {}", deployed_contracts.access_manager);
    info!("  Game Implementation: {}", deployed_contracts.game_implementation);

    Ok(deployed_contracts)
}

pub async fn deploy_mock_permissioned_game(
    signer: &SignerLock,
    rpc: &Url,
    factory: Address,
) -> Result<Address> {
    let ctor_args =
        MockPermissionedDisputeGame::constructorCall { _disputeGameFactory: factory }.abi_encode();

    let mut creation_code = MockPermissionedDisputeGame::BYTECODE.to_vec();
    creation_code.extend_from_slice(&ctor_args);

    let request = TransactionRequest {
        input: TransactionInput::new(Bytes::from(creation_code)),
        ..Default::default()
    };

    let receipt = signer
        .send_transaction_request(rpc.clone(), request)
        .await
        .context("Failed to deploy mock permissioned dispute game")?;

    receipt.contract_address.context("Mock game deployment lacked contract address")
}

pub async fn send_contract_transaction(
    signer: &SignerLock,
    rpc: &Url,
    to: Address,
    data: Bytes,
    value: Option<U256>,
) -> Result<()> {
    let request = TransactionRequest {
        to: Some(TxKind::Call(to)),
        input: TransactionInput::new(data),
        value,
        ..Default::default()
    };

    signer
        .send_transaction_request(rpc.clone(), request)
        .await
        .context("Failed to send contract transaction")?;

    Ok(())
}
