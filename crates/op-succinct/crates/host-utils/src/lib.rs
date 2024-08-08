pub mod fetcher;
pub mod helpers;

use client_utils::RawBootInfo;
use kona_host::HostCli;
use sp1_sdk::SP1Stdin;

use anyhow::Result;

use alloy_sol_types::sol;

use rkyv::{
    ser::{
        serializers::{AlignedSerializer, CompositeSerializer, HeapScratch, SharedSerializeMap},
        Serializer,
    },
    AlignedVec,
};

use crate::helpers::load_kv_store;

pub enum ProgramType {
    Single,
    Multi,
}

sol! {
    struct L2Output {
        uint64 zero;
        bytes32 l2_state_root;
        bytes32 l2_storage_hash;
        bytes32 l2_claim_hash;
    }
}

/// Get the stdin to generate a proof for the given L2 claim.
pub fn get_sp1_stdin(host_cli: &HostCli) -> Result<SP1Stdin> {
    let mut stdin = SP1Stdin::new();

    let boot_info = RawBootInfo {
        l1_head: host_cli.l1_head,
        l2_output_root: host_cli.l2_output_root,
        l2_claim: host_cli.l2_claim,
        l2_claim_block: host_cli.l2_block_number,
        chain_id: host_cli.l2_chain_id,
    };
    stdin.write(&boot_info);

    // Get the workspace root, which is where the data directory is.
    let data_dir = host_cli.data_dir.as_ref().expect("Data directory not set!");
    let kv_store = load_kv_store(data_dir);

    let mut serializer = CompositeSerializer::new(
        AlignedSerializer::new(AlignedVec::new()),
        // TODO: This value corresponds to the size of the space needed to
        // serialize the KV store. We should compute this dynamically.
        HeapScratch::<33554432>::new(),
        SharedSerializeMap::new(),
    );
    serializer.serialize_value(&kv_store)?;

    let buffer = serializer.into_serializer().into_inner();
    let kv_store_bytes = buffer.into_vec();
    stdin.write_slice(&kv_store_bytes);

    Ok(stdin)
}
