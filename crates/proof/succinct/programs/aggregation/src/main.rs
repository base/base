//! A program that aggregates the proofs of the range program.

#![cfg_attr(target_os = "zkvm", no_main)]
#[cfg(target_os = "zkvm")]
sp1_zkvm::entrypoint!(main);

use std::collections::HashMap;

use alloy_consensus::Header;
use alloy_primitives::{B256, Bytes, keccak256};
use alloy_sol_types::SolValue;
use base_proof_succinct_client_utils::{
    boot::BootInfoStruct,
    types::{AggregationInputs, AggregationOutputs, u32_to_u8},
};
use sha2::{Digest, Sha256};

/// Aggregation program entry point.
pub fn main() {
    // Read in the public values corresponding to each range proof.
    let agg_inputs = sp1_zkvm::io::read::<AggregationInputs>();
    // Note: The headers are in order from start to end. We use serde_cbor as bincode serialization
    // causes issues with the zkVM.
    let headers_bytes = sp1_zkvm::io::read_vec();
    let headers: Vec<Header> = serde_cbor::from_slice(&headers_bytes).unwrap();
    assert!(!agg_inputs.boot_infos.is_empty());

    // Confirm that the boot infos are sequential.
    agg_inputs.boot_infos.windows(2).for_each(|pair| {
        let (prev_boot_info, boot_info) = (&pair[0], &pair[1]);

        // The claimed block of the previous boot info must be the L2 output root of the current
        // boot.
        assert_eq!(prev_boot_info.l2PostRoot, boot_info.l2PreRoot);

        // The ending block of the previous range must match the starting block of the next range.
        assert_eq!(prev_boot_info.l2BlockNumber, boot_info.l2PreBlockNumber);

        // The rollup config must be the same for all the boot infos, to ensure they're
        // from the same chain and span batch range.
        assert_eq!(prev_boot_info.rollupConfigHash, boot_info.rollupConfigHash);
    });

    // Verify each range program proof.
    agg_inputs.boot_infos.iter().for_each(|boot_info| {
        // In the range program, the public values digest is just the hash of the ABI encoded
        // boot info.
        let serialized_boot_info = bincode::serialize(&boot_info).unwrap();
        let pv_digest = Sha256::digest(serialized_boot_info);

        sp1_lib::verify::verify_sp1_proof(&agg_inputs.multi_block_vkey, &pv_digest.into());
    });

    // Create a map of each l1 head in the [`BootInfoStruct`]'s to booleans
    let mut l1_heads_map: HashMap<B256, bool> =
        agg_inputs.boot_infos.iter().map(|boot_info| (boot_info.l1Head, false)).collect();

    // Iterate through the headers in reverse order. The headers should be sequentially linked and
    // include the l1 head of each boot info.
    let mut current_hash = agg_inputs.latest_l1_checkpoint_head;
    for header in headers.iter().rev() {
        assert_eq!(current_hash, header.hash_slow());

        // Mark the l1 head as found if it's in our map.
        if let Some(found) = l1_heads_map.get_mut(&current_hash) {
            *found = true;
        }

        current_hash = header.parent_hash;
    }

    // Check if all l1 heads were found in the chain.
    for (l1_head, found) in &l1_heads_map {
        assert!(*found, "l1 head {l1_head:?} not found in the provided header chain");
    }

    let first_boot_info = &agg_inputs.boot_infos[0];
    let last_boot_info = &agg_inputs.boot_infos[agg_inputs.boot_infos.len() - 1];

    // Consolidate the intermediate roots for all boot infos into a single Bytes.
    let intermediate_roots: Bytes = agg_inputs
        .boot_infos
        .iter()
        .flat_map(|boot_info| boot_info.intermediateRoots.iter().copied())
        .collect::<Vec<u8>>()
        .into();

    // Consolidate the boot info into a single BootInfo struct that represents the range proven.
    let final_boot_info = BootInfoStruct {
        l2PreRoot: first_boot_info.l2PreRoot,
        l2PreBlockNumber: first_boot_info.l2PreBlockNumber,
        l2BlockNumber: last_boot_info.l2BlockNumber,
        l2PostRoot: last_boot_info.l2PostRoot,
        l1Head: agg_inputs.latest_l1_checkpoint_head,
        rollupConfigHash: last_boot_info.rollupConfigHash,
        intermediateRoots: intermediate_roots,
    };

    // Convert the range vkey to a B256.
    let multi_block_vkey_b256 = B256::from(u32_to_u8(agg_inputs.multi_block_vkey));

    let agg_outputs = AggregationOutputs {
        proverAddress: agg_inputs.prover_address,
        l1Head: final_boot_info.l1Head,
        l2PreRoot: final_boot_info.l2PreRoot,
        startingL2SequenceNumber: final_boot_info.l2PreBlockNumber,
        l2PostRoot: final_boot_info.l2PostRoot,
        endingL2SequenceNumber: final_boot_info.l2BlockNumber,
        intermediateRoots: final_boot_info.intermediateRoots,
        rollupConfigHash: final_boot_info.rollupConfigHash,
        imageHash: multi_block_vkey_b256,
    };

    // Commit keccak256 of the packed encoding to match the on-chain verifier's
    // keccak256(abi.encodePacked(proposer, l1OriginHash, ...)) digest.
    let packed = agg_outputs.abi_encode_packed();
    let digest = keccak256(&packed);
    sp1_zkvm::io::commit_slice(digest.as_ref());
}
