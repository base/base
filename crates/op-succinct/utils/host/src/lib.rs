pub mod block_range;
pub mod fetcher;
pub mod helpers;
pub mod rollup_config;
pub mod stats;
pub mod witnessgen;

use alloy::sol;
use alloy_consensus::Header;
use alloy_primitives::B256;
use kona_host::{
    kv::{DiskKeyValueStore, MemoryKeyValueStore},
    HostCli,
};
use op_alloy_genesis::RollupConfig;
use op_succinct_client_utils::{
    boot::BootInfoStruct, types::AggregationInputs, BootInfoWithBytesConfig, InMemoryOracle,
};
use sp1_sdk::{HashableKey, SP1Proof, SP1Stdin};
use std::{fs::File, io::Read};

use anyhow::Result;

use rkyv::{
    ser::{
        serializers::{AlignedSerializer, CompositeSerializer, HeapScratch, SharedSerializeMap},
        Serializer,
    },
    AlignedVec,
};

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    contract L2OutputOracle {
        bytes32 public aggregationVkey;
        bytes32 public rangeVkeyCommitment;
        bytes32 public rollupConfigHash;

        function updateAggregationVKey(bytes32 _aggregationVKey) external onlyOwner;

        function updateRangeVkeyCommitment(bytes32 _rangeVkeyCommitment) external onlyOwner;
    }
}

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
pub fn get_proof_stdin(host_cli: &HostCli) -> Result<SP1Stdin> {
    let mut stdin = SP1Stdin::new();

    // Read the rollup config.
    let mut rollup_config_file = File::open(host_cli.rollup_config_path.as_ref().unwrap())?;
    let mut rollup_config_bytes = Vec::new();
    rollup_config_file.read_to_end(&mut rollup_config_bytes)?;

    let ser_config = std::fs::read_to_string(host_cli.rollup_config_path.as_ref().unwrap())?;
    let rollup_config: RollupConfig = serde_json::from_str(&ser_config)?;

    let boot_info = BootInfoWithBytesConfig {
        l1_head: host_cli.l1_head,
        l2_output_root: host_cli.agreed_l2_output_root,
        l2_claim: host_cli.claimed_l2_output_root,
        l2_claim_block: host_cli.claimed_l2_block_number,
        chain_id: rollup_config.l2_chain_id,
        rollup_config_bytes,
    };
    stdin.write(&boot_info);

    // Get the disk KV store.
    let disk_kv_store = DiskKeyValueStore::new(host_cli.data_dir.clone().unwrap());

    // Convert the disk KV store to a memory KV store.
    let mem_kv_store: MemoryKeyValueStore = disk_kv_store.try_into().map_err(|_| {
        anyhow::anyhow!("Failed to convert DiskKeyValueStore to MemoryKeyValueStore")
    })?;

    let mut serializer = CompositeSerializer::new(
        AlignedSerializer::new(AlignedVec::new()),
        // Note: This value corresponds to the size of the heap needed to serialize the KV store.
        // Increase this value if we start running into serialization issues.
        HeapScratch::<67108864>::new(),
        SharedSerializeMap::new(),
    );
    // Serialize the underlying KV store.
    serializer.serialize_value(&InMemoryOracle::from_b256_hashmap(mem_kv_store.store))?;

    let buffer = serializer.into_serializer().into_inner();
    let kv_store_bytes = buffer.into_vec();
    stdin.write_slice(&kv_store_bytes);

    Ok(stdin)
}

/// Get the stdin for the aggregation proof.
pub fn get_agg_proof_stdin(
    proofs: Vec<SP1Proof>,
    boot_infos: Vec<BootInfoStruct>,
    headers: Vec<Header>,
    multi_block_vkey: &sp1_sdk::SP1VerifyingKey,
    latest_checkpoint_head: B256,
) -> Result<SP1Stdin> {
    let mut stdin = SP1Stdin::new();
    for proof in proofs {
        let SP1Proof::Compressed(compressed_proof) = proof else {
            panic!();
        };
        stdin.write_proof(*compressed_proof, multi_block_vkey.vk.clone());
    }

    // Write the aggregation inputs to the stdin.
    stdin.write(&AggregationInputs {
        boot_infos,
        latest_l1_checkpoint_head: latest_checkpoint_head,
        multi_block_vkey: multi_block_vkey.hash_u32(),
    });
    // The headers have issues serializing with bincode, so use serde_json instead.
    let headers_bytes = serde_cbor::to_vec(&headers).unwrap();
    stdin.write_vec(headers_bytes);

    Ok(stdin)
}
