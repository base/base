pub mod helpers;

use alloy_primitives::{keccak256, B256};
use ethers::{
    providers::{Http, Middleware, Provider},
    types::{BlockNumber, H160, U256},
};
use kona_host::HostCli;
use sp1_core::runtime::ExecutionReport;
use sp1_sdk::{ProverClient, SP1Stdin};
use zkvm_common::{BootInfoWithoutRollupConfig, BytesHasherBuilder};

use clap::Parser;
use std::{collections::HashMap, str::FromStr};

use alloy_sol_types::{sol, SolValue};
use anyhow::Result;
use std::{env, fs};

use rkyv::{
    ser::{
        serializers::{AlignedSerializer, CompositeSerializer, HeapScratch, SharedSerializeMap},
        Serializer,
    },
    AlignedVec, Archive, Deserialize, Serialize,
};

use crate::helpers::load_kv_store;

pub const KONA_ELF: &[u8] = include_bytes!("../../elf/riscv32im-succinct-zkvm-elf");

sol! {
    struct L2Claim {
        uint64 num;
        bytes32 l2_state_root;
        bytes32 l2_storage_hash;
        bytes32 l2_claim_hash;
    }

    struct L2Output {
        uint64 num;
        bytes32 l2_state_root;
        bytes32 l2_storage_hash;
        bytes32 l2_head;
    }
}

#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive_attr(derive(Debug))]
pub struct InMemoryOracle {
    cache: HashMap<[u8; 32], Vec<u8>, BytesHasherBuilder>,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct SP1KonaCliArgs {
    #[arg(long)]
    l1_head: String,

    #[arg(long)]
    l2_output_root: String,

    #[arg(long)]
    l2_claim: String,

    #[arg(long)]
    l2_claim_block: u64,

    #[arg(long)]
    chain_id: u64,
}

impl From<SP1KonaCliArgs> for BootInfoWithoutRollupConfig {
    fn from(args: SP1KonaCliArgs) -> Self {
        BootInfoWithoutRollupConfig {
            l1_head: B256::from_str(&args.l1_head).unwrap(),
            l2_output_root: B256::from_str(&args.l2_output_root).unwrap(),
            l2_claim: B256::from_str(&args.l2_claim).unwrap(),
            l2_claim_block: args.l2_claim_block,
            chain_id: args.chain_id,
        }
    }
}

/// Execute the Kona program and return the execution report.
pub fn execute_kona_program(boot_info: &BootInfoWithoutRollupConfig) -> ExecutionReport {
    let mut stdin = SP1Stdin::new();

    stdin.write(&boot_info);

    // Read KV store into raw bytes and pass to stdin.
    let kv_store = load_kv_store(&format!("../data/{}", boot_info.l2_claim_block));

    let mut serializer = CompositeSerializer::new(
        AlignedSerializer::new(AlignedVec::new()),
        // TODO: This value is hardcoded to minimum for this block.
        // Figure out how to compute it so it works on all blocks.
        HeapScratch::<8388608>::new(),
        SharedSerializeMap::new(),
    );
    serializer.serialize_value(&kv_store).unwrap();

    let buffer = serializer.into_serializer().into_inner();
    let kv_store_bytes = buffer.into_vec();
    stdin.write_slice(&kv_store_bytes);

    // First instantiate a mock prover client to just execute the program and get the estimation of
    // cycle count.
    let client = ProverClient::mock();

    let (mut _public_values, report) = client.execute(KONA_ELF, stdin).unwrap();
    report
}

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

            if block_timestamp == target_timestamp {
                return Ok(block.hash.unwrap().0.into());
            } else if block_timestamp < target_timestamp {
                low = mid + 1;
            } else {
                high = mid - 1;
            }
        }

        // Return the block hash of the closest block after the target timestamp
        let block = l1_provider.get_block(low).await?.unwrap();
        Ok(block.hash.unwrap().0.into())
    }

    /// Get the L2 output data for a given block number and save the boot info to a file in the data directory
    /// with block_number. Return the arguments to be passed to the native host for datagen.
    pub async fn get_native_execution_data(&self, l2_block_num: u64) -> Result<HostCli> {
        let l2_provider = Provider::<Http>::try_from(&self.l2_rpc)?;

        let l2_block_safe_head = l2_block_num - 1;

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
            num: l2_block_num,
            l2_state_root: l2_output_state_root.0.into(),
            l2_storage_hash: l2_output_storage_hash.0.into(),
            l2_head: l2_head.0.into(),
        };
        let l2_output_root = keccak256(&l2_output_encoded.abi_encode());

        // Get L2 claim data.
        let l2_claim_block = l2_provider.get_block(l2_block_num).await?.unwrap();
        let l2_claim_state_root = l2_claim_block.state_root;
        let l2_claim_hash = l2_claim_block.hash.expect("L2 claim hash is missing");
        let l2_claim_storage_hash = l2_provider
            .get_proof(
                H160::from_str("0x4200000000000000000000000000000000000016")?,
                Vec::new(),
                Some(l2_block_num.into()),
            )
            .await?
            .storage_hash;

        let l2_claim_encoded = L2Claim {
            num: l2_block_num,
            l2_state_root: l2_claim_state_root.0.into(),
            l2_storage_hash: l2_claim_storage_hash.0.into(),
            l2_claim_hash: l2_claim_hash.0.into(),
        };
        let l2_claim = keccak256(&l2_claim_encoded.abi_encode());

        // Get L1 head.
        let l2_block_timestamp = l2_claim_block.timestamp;
        let target_timestamp = l2_block_timestamp + 300;

        // TODO: Convert target_timestamp to a block number
        let l1_head = self.find_block_by_timestamp(target_timestamp).await?;

        let l2_chain_id = l2_provider.get_chainid().await?;
        let data_directory = format!("../../data/{}", l2_block_num);

        // Create data directory. Note: Native execution will need to be run to save the merkle proofs.
        fs::create_dir_all(&data_directory)?;

        Ok(HostCli {
            l1_head: l1_head.0.into(),
            l2_output_root: l2_output_root.0.into(),
            l2_claim: l2_claim.0.into(),
            l2_block_number: l2_block_num,
            l2_chain_id: l2_chain_id.as_u64(),
            l2_head: l2_head.0.into(),
            l2_node_address: Some(self.l2_rpc.clone()),
            l1_node_address: Some(self.l1_rpc.clone()),
            l1_beacon_address: Some(self.l1_beacon_rpc.clone()),
            data_dir: Some(data_directory.into()),
            // TODO: This is probably not correct, but it's a placeholder. How should we programmatically determine the
            // specified client program from here?
            exec: Some("./target/release-client-lto/zkvm-client".to_string()),
            server: false,
            v: 0,
        })
    }
}
