use alloy_primitives::B256;
use alloy_sol_types::SolValue;
use anyhow::Result;
use cargo_metadata::MetadataCommand;
use kona_host::HostCli;
use std::{cmp::Ordering, env, fs, path::Path, str::FromStr};

use alloy_primitives::keccak256;
use ethers::{
    providers::{Http, Middleware, Provider},
    types::{BlockNumber, H160, U256},
};

use crate::L2Output;

/// The SP1KonaDataFetcher struct is used to fetch the L2 output data and L2 claim data for a given block number.
/// It is used to generate the boot info for the native host program.
pub struct SP1KonaDataFetcher {
    pub l1_rpc: String,
    pub l1_beacon_rpc: String,
    pub l2_rpc: String,
}

impl Default for SP1KonaDataFetcher {
    fn default() -> Self {
        SP1KonaDataFetcher::new()
    }
}

impl SP1KonaDataFetcher {
    pub fn new() -> Self {
        SP1KonaDataFetcher {
            l1_rpc: env::var("CLABBY_RPC_L1")
                .unwrap_or_else(|_| "http://localhost:8545".to_string()),
            l1_beacon_rpc: env::var("ETH_BEACON_URL")
                .unwrap_or_else(|_| "http://localhost:5052".to_string()),
            l2_rpc: env::var("CLABBY_RPC_L2")
                .unwrap_or_else(|_| "http://localhost:9545".to_string()),
        }
    }

    async fn find_block_by_timestamp(&self, target_timestamp: U256) -> Result<B256> {
        let l1_provider = Provider::<Http>::try_from(&self.l1_rpc)?;
        let latest_block = l1_provider.get_block(BlockNumber::Latest).await?.unwrap();
        let mut low = 0;
        let mut high = latest_block.number.unwrap().as_u64();

        while low <= high {
            let mid = (low + high) / 2;
            let block = l1_provider.get_block(mid).await?.unwrap();
            let block_timestamp = block.timestamp;

            match block_timestamp.cmp(&target_timestamp) {
                Ordering::Equal => return Ok(block.hash.unwrap().0.into()),
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid - 1,
            }
        }

        // Return the block hash of the closest block after the target timestamp
        let block = l1_provider.get_block(low).await?.unwrap();
        Ok(block.hash.unwrap().0.into())
    }

    /// Get the L2 output data for a given block number and save the boot info to a file in the data directory
    /// with block_number. Return the arguments to be passed to the native host for datagen.
    pub async fn get_host_cli_args(
        &self,
        l2_block_safe_head: u64,
        l2_claim_block_nb: u64,
        verbosity: u8,
    ) -> Result<HostCli> {
        let l2_provider = Provider::<Http>::try_from(&self.l2_rpc)?;

        // Get L2 output data.
        let l2_output_block = l2_provider.get_block(l2_block_safe_head).await?.unwrap();
        let l2_output_state_root = l2_output_block.state_root;
        let l2_head = l2_output_block.hash.expect("L2 head is missing");
        let l2_output_storage_hash = l2_provider
            .get_proof(
                H160::from_str("0x4200000000000000000000000000000000000016")?,
                Vec::new(),
                Some(l2_block_safe_head.into()),
            )
            .await?
            .storage_hash;

        let l2_output_encoded = L2Output {
            zero: 0,
            l2_state_root: l2_output_state_root.0.into(),
            l2_storage_hash: l2_output_storage_hash.0.into(),
            l2_claim_hash: l2_head.0.into(),
        };
        let l2_output_root = keccak256(l2_output_encoded.abi_encode());

        // Get L2 claim data.
        let l2_claim_block = l2_provider.get_block(l2_claim_block_nb).await?.unwrap();
        let l2_claim_state_root = l2_claim_block.state_root;
        let l2_claim_hash = l2_claim_block.hash.expect("L2 claim hash is missing");
        let l2_claim_storage_hash = l2_provider
            .get_proof(
                H160::from_str("0x4200000000000000000000000000000000000016")?,
                Vec::new(),
                Some(l2_claim_block.number.unwrap().into()),
            )
            .await?
            .storage_hash;

        let l2_claim_encoded = L2Output {
            zero: 0,
            l2_state_root: l2_claim_state_root.0.into(),
            l2_storage_hash: l2_claim_storage_hash.0.into(),
            l2_claim_hash: l2_claim_hash.0.into(),
        };
        let l2_claim = keccak256(l2_claim_encoded.abi_encode());

        // Get L1 head.
        let l2_block_timestamp = l2_claim_block.timestamp;
        let target_timestamp = l2_block_timestamp + 300;
        let l1_head = self.find_block_by_timestamp(target_timestamp).await?;

        // Get the chain id.
        let l2_chain_id = l2_provider.get_chainid().await?;

        // Get the workspace root, which is where the data directory is.
        let metadata = MetadataCommand::new().exec().unwrap();
        let workspace_root = metadata.workspace_root;
        let data_directory = format!("{}/data/{}", workspace_root, l2_claim_block_nb);
        // In build.rs we compile with release-client-lto, so we need to use that.
        let exec_directory = format!("{}/target/release-client-lto/zkvm-client", workspace_root);

        // Create data directory. This will be used by the host program running in native execution
        // mode to save all preimages.
        if !Path::new(&data_directory).exists() {
            fs::create_dir_all(&data_directory)?;
        }

        Ok(HostCli {
            l1_head: l1_head.0.into(),
            l2_output_root: l2_output_root.0.into(),
            l2_claim: l2_claim.0.into(),
            l2_block_number: l2_claim_block_nb,
            l2_chain_id: l2_chain_id.as_u64(),
            l2_head: l2_head.0.into(),
            l2_node_address: Some(self.l2_rpc.clone()),
            l1_node_address: Some(self.l1_rpc.clone()),
            l1_beacon_address: Some(self.l1_beacon_rpc.clone()),
            data_dir: Some(data_directory.into()),
            exec: Some(exec_directory),
            server: false,
            v: verbosity,
        })
    }
}
