pub mod block_range;
pub mod fetcher;
pub mod rollup_config;
pub mod stats;

use alloy_consensus::Header;
use alloy_primitives::B256;
use alloy_sol_types::sol;
use anyhow::Result;
use kona_host::single::SingleChainHost;
use kona_preimage::{BidirectionalChannel, HintWriter, NativeChannel, OracleReader};
use log::info;
use op_succinct_client_utils::client::run_opsuccinct_client;
use op_succinct_client_utils::precompiles::zkvm_handle_register;
use op_succinct_client_utils::{boot::BootInfoStruct, types::AggregationInputs};
use op_succinct_client_utils::{InMemoryOracle, StoreOracle};
use rkyv::to_bytes;
use sp1_sdk::{HashableKey, SP1Proof, SP1Stdin};
use std::sync::Arc;

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

#[derive(Debug, Clone, Copy)]
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

#[derive(Debug, Clone)]
pub struct OPSuccinctHost {
    pub kona_args: SingleChainHost,
}

/// Get the stdin to generate a proof for the given L2 claim.
pub fn get_proof_stdin(oracle: InMemoryOracle) -> Result<SP1Stdin> {
    let mut stdin = SP1Stdin::new();

    // Serialize the underlying KV store.
    let buffer = to_bytes::<rkyv::rancor::Error>(&oracle)?;

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

/// Start the server and native client. Each server is tied to a single client.
pub async fn start_server_and_native_client(
    cfg: OPSuccinctHost,
) -> Result<InMemoryOracle, anyhow::Error> {
    info!("Starting preimage server and client program.");
    let in_memory_oracle = cfg.run().await?;

    Ok(in_memory_oracle)
}

impl OPSuccinctHost {
    /// Run the host and client program.
    ///
    /// Returns the in-memory oracle which can be supplied to the zkVM.
    pub async fn run(&self) -> Result<InMemoryOracle> {
        let hint = BidirectionalChannel::new()?;
        let preimage = BidirectionalChannel::new()?;

        let server_task = self
            .kona_args
            .start_server(hint.host, preimage.host)
            .await?;

        let in_memory_oracle = self
            .run_witnessgen_client(preimage.client, hint.client)
            .await?;
        // Unlike the upstream, manually abort the server task, as it will hang if you wait for both tasks to complete.
        server_task.abort();

        Ok(in_memory_oracle)
    }

    /// Run the witness generation client.
    pub async fn run_witnessgen_client(
        &self,
        preimage_chan: NativeChannel,
        hint_chan: NativeChannel,
    ) -> Result<InMemoryOracle> {
        let oracle = Arc::new(StoreOracle::new(
            OracleReader::new(preimage_chan),
            HintWriter::new(hint_chan),
        ));
        let _ = run_opsuccinct_client(oracle.clone(), Some(zkvm_handle_register)).await?;
        let in_memory_oracle = InMemoryOracle::populate_from_store(oracle.as_ref())?;
        Ok(in_memory_oracle)
    }
}
