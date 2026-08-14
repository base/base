//! SP1 stdin encoding for range and aggregation programs.

use alloy_consensus::Header;
use alloy_primitives::{Address, B256};
use anyhow::Result;
use base_proof_zk_utils::{
    boot::BootInfoStruct, types::AggregationInputs, witness::DefaultWitnessData,
};
use rkyv::to_bytes;
use sp1_sdk::{HashableKey, SP1Proof, SP1Stdin};

/// Build SP1 stdin from collected witness data.
///
/// The intermediate root sampling interval is sourced from `BootInfo` inside the zkVM
/// (preimage key 9) — the same channel the TEE enclave reads — so it is intentionally not
/// passed through stdin.
pub fn get_sp1_stdin(witness: DefaultWitnessData) -> Result<SP1Stdin> {
    let mut stdin = SP1Stdin::default();
    let buffer = to_bytes::<rkyv::rancor::Error>(&witness)?;
    stdin.write_slice(&buffer);
    Ok(stdin)
}

/// Build the SP1 stdin for the aggregation proof from range proofs and headers.
pub fn get_agg_proof_stdin(
    proofs: Vec<SP1Proof>,
    boot_infos: Vec<BootInfoStruct>,
    headers: Vec<Header>,
    multi_block_vkey: &sp1_sdk::SP1VerifyingKey,
    latest_checkpoint_head: B256,
    prover_address: Address,
) -> Result<SP1Stdin> {
    let mut stdin = SP1Stdin::default();
    for proof in proofs {
        let SP1Proof::Compressed(compressed_proof) = proof else {
            return Err(anyhow::anyhow!("Invalid proof passed as compressed proof!"));
        };
        stdin.write_proof(*compressed_proof, multi_block_vkey.vk.clone());
    }

    stdin.write(&AggregationInputs {
        boot_infos,
        latest_l1_checkpoint_head: latest_checkpoint_head,
        multi_block_vkey: multi_block_vkey.hash_u32(),
        prover_address,
    });
    // The headers have issues serializing with bincode, so use serde_json instead.
    let headers_bytes = serde_cbor::to_vec(&headers).unwrap();
    stdin.write_vec(headers_bytes);

    Ok(stdin)
}
