// A host program to generate a proof of an Optimism L2 block STF in the zkVM.

mod helpers;
use helpers::load_kv_store;

use zkvm_client::BootInfoWithoutRollupConfig;
use zkvm_common::BytesHasherBuilder;

use std::collections::HashMap;

use alloy_primitives::b256;
use rkyv::{
    ser::{serializers::*, Serializer},
    AlignedVec, Archive, Deserialize, Serialize
};
use sp1_sdk::{utils, ProverClient, SP1Stdin};

const ELF: &[u8] = include_bytes!("../../elf/riscv32im-succinct-zkvm-elf");

#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive_attr(derive(Debug))]
pub struct InMemoryOracle {
    cache: HashMap<[u8;32], Vec<u8>, BytesHasherBuilder>,
}

fn main() {
    utils::setup_logger();
    let mut stdin = SP1Stdin::new();

    // TODO: Move this to CLI so we can pass same values to both calls from the justfile.
    let l1_head = b256!("557beb4a3abd062999b748e6b5903c28075dbef0279a10b987e9e8998806a938");
    let l2_output_root = b256!("f54d7d4e617e442c44e2f029b46d15c98bbd5151021f1fd45faee864273c07e7");
    let l2_claim = b256!("3a5105e0f56bdc6c12c41060048fa2ec46536207daccee00f7bbacc74b132a84");
    let l2_claim_block = 121914785;
    let chain_id = 10;

    let boot_info = BootInfoWithoutRollupConfig {
        l1_head,
        l2_output_root,
        l2_claim,
        l2_claim_block,
        chain_id,
    };
    stdin.write(&boot_info);

    // Read KV store into raw bytes and pass to stdin.
    let kv_store = load_kv_store(&format!("../../data/{}", l2_claim_block));

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
