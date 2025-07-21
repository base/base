use alloy_primitives::{hex, Address};
use alloy_signer_local::PrivateKeySigner;
use anyhow::Result;
use op_succinct_client_utils::{boot::hash_rollup_config, types::u32_to_u8};
use op_succinct_elfs::AGGREGATION_ELF;
use op_succinct_host_utils::fetcher::OPSuccinctDataFetcher;
use op_succinct_proof_utils::get_range_elf_embedded;
use sp1_sdk::{HashableKey, Prover, ProverClient};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

pub const TWO_WEEKS_IN_SECONDS: u64 = 14 * 24 * 60 * 60;

/// Shared configuration data that both L2OO and FDG configs use.
#[derive(Debug, Clone)]
pub struct SharedConfigData {
    pub rollup_config_hash: String,
    pub aggregation_vkey: String,
    pub range_vkey_commitment: String,
    pub verifier_address: String,
    pub use_sp1_mock_verifier: bool,
}

/// Returns an address based on environment variables and private key settings:
/// - If env_var exists, returns that address
/// - Otherwise if private_key_by_default=true and PRIVATE_KEY exists, returns address derived from
///   private key
/// - Otherwise returns zero address
pub fn get_address(env_var: &str, private_key_by_default: bool) -> String {
    // First try to get address directly from env var.
    if let Ok(addr) = env::var(env_var) {
        return addr;
    }

    // Next try to derive address from private key if enabled.
    if private_key_by_default {
        if let Ok(pk) = env::var("PRIVATE_KEY") {
            let signer: PrivateKeySigner = pk.parse().unwrap();
            return signer.address().to_string();
        }
    }

    // Fallback to zero address.
    Address::ZERO.to_string()
}

/// Parse comma-separated addresses from environment variable.
pub fn parse_addresses(env_var: &str) -> Vec<String> {
    env::var(env_var)
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .collect()
}

/// Get shared configuration data that both L2OO and FDG configs need.
pub async fn get_shared_config_data() -> Result<SharedConfigData> {
    let data_fetcher = OPSuccinctDataFetcher::new_with_rollup_config().await?;

    // Determine if we're using mock verifier.
    let use_sp1_mock_verifier = env::var("OP_SUCCINCT_MOCK")
        .unwrap_or("false".to_string())
        .parse::<bool>()
        .unwrap_or(false);

    // Set the verifier address.
    let verifier_address = env::var("VERIFIER_ADDRESS").unwrap_or_else(|_| {
        // Default to Groth16 VerifierGateway contract address.
        // Source: https://docs.succinct.xyz/docs/sp1/verification/contract-addresses
        "0x397A5f7f3dBd538f23DE225B51f532c34448dA9B".to_string()
    });

    let rollup_config = data_fetcher.rollup_config.as_ref().unwrap();
    let rollup_config_hash = format!("0x{:x}", hash_rollup_config(rollup_config));

    // Calculate verification keys.
    let prover = ProverClient::builder().cpu().build();
    let (_, agg_vkey) = prover.setup(AGGREGATION_ELF);
    let aggregation_vkey = agg_vkey.vk.bytes32();

    let (_, range_vkey) = prover.setup(get_range_elf_embedded());
    let range_vkey_commitment = format!("0x{}", hex::encode(u32_to_u8(range_vkey.vk.hash_u32())));

    Ok(SharedConfigData {
        rollup_config_hash,
        aggregation_vkey,
        range_vkey_commitment,
        verifier_address,
        use_sp1_mock_verifier,
    })
}

/// Write a JSON config to a file.
pub fn write_config_file<T: serde::Serialize>(
    config: &T,
    file_path: &Path,
    description: &str,
) -> Result<()> {
    // Create parent directories if they don't exist.
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Write the config to the file.
    fs::write(file_path, serde_json::to_string_pretty(config)?)?;

    println!("{} configuration written to: {}", description, file_path.display());

    Ok(())
}

/// Find the project root directory (where .git exists).
pub fn find_project_root() -> Option<PathBuf> {
    let mut path = std::env::current_dir().ok()?;
    while !path.join(".git").exists() {
        if !path.pop() {
            return None;
        }
    }
    Some(path)
}
