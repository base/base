// A host program to generate a proof of an Optimism L2 block STF in the zkVM.

mod helpers;
use helpers::load_kv_store;

use zkvm_common::{BootInfoWithoutRollupConfig, BytesHasherBuilder};

use clap::Parser;
use std::{collections::HashMap, str::FromStr};

use alloy_primitives::B256;
use rkyv::{
    ser::{serializers::*, Serializer},
    AlignedVec, Archive, Deserialize, Serialize,
};
use sp1_sdk::{utils, ProverClient, SP1Stdin};

const ELF: &[u8] = include_bytes!("../../elf/riscv32im-succinct-zkvm-elf");

#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive_attr(derive(Debug))]
pub struct InMemoryOracle {
    cache: HashMap<[u8; 32], Vec<u8>, BytesHasherBuilder>,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
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

fn main() {
    utils::setup_logger();
    let mut stdin = SP1Stdin::new();

    let args = Args::parse();

    // TODO: Move this to CLI so we can pass same values to both calls from the justfile.
    let l1_head = B256::from_str(&args.l1_head).unwrap();
    let l2_output_root = B256::from_str(&args.l2_output_root).unwrap();
    let l2_claim = B256::from_str(&args.l2_claim).unwrap();

    let boot_info = BootInfoWithoutRollupConfig {
        l1_head,
        l2_output_root,
        l2_claim,
        l2_claim_block: args.l2_claim_block,
        chain_id: args.chain_id,
    };
    stdin.write(&boot_info);

    // Read KV store into raw bytes and pass to stdin.
    let kv_store = load_kv_store(&format!("../data/{}", args.l2_claim_block));

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

    let (mut _public_values, report) = client.execute(ELF, stdin).unwrap();
    println!("Report: {}", report);

    // Then generate the real proof.
    // let (pk, vk) = client.setup(ELF);
    // let mut proof = client.prove(&pk, stdin).unwrap();

    println!("generated valid zk proof");
}
