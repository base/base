//! A program that aggregates the proofs of the multi-block program.

#![cfg_attr(target_os = "zkvm", no_main)]
#[cfg(target_os = "zkvm")]
sp1_zkvm::entrypoint!(main);

use std::collections::HashMap;

use alloy_consensus::Header;
use alloy_primitives::B256;
use itertools::Itertools;
use op_succinct_client_utils::{types::AggregationInputs, RawBootInfo};
use sha2::{Digest, Sha256};

/// The verification key for the multi-block program.
///
/// Whenever the multi-block program changes, you will need to update this.
///
/// TODO: The aggregation program should take in an arbitrary vkey digest and the smart contract
/// should verify the proof matches the arbitrary vkey digest stored in the contract. This means
/// that the aggregate program would no longer need to update this value.
const MULTI_BLOCK_PROGRAM_VKEY_DIGEST: [u32; 8] =
    [1807316243, 1400630407, 873277975, 658999266, 422343326, 774525422, 1268417967, 711858264];

fn main() {
    // Read in the aggregation inputs corresponding to each multi-block proof.
    let agg_inputs = sp1_zkvm::io::read::<AggregationInputs>();

    // Read in the headers.
    //
    // Note: The headers are in order from start to end. We use [serde_cbor] as bincode
    // serialization causes issues with the zkVM.
    let headers_bytes = sp1_zkvm::io::read_vec();
    let headers: Vec<Header> = serde_cbor::from_slice(&headers_bytes).unwrap();
    assert!(!agg_inputs.boot_infos.is_empty());

    // Confirm that the boot infos are sequential.
    agg_inputs.boot_infos.iter().tuples().for_each(|(prev_boot_info, curr_boot_info)| {
        // The claimed block of the previous boot info must be the l2 output root of the current
        // boot.
        assert_eq!(prev_boot_info.l2_claim, curr_boot_info.l2_output_root);

        // The chain id must be the same for all the boot infos, to ensure they're
        // from the same chain and span batch range.
        assert_eq!(prev_boot_info.chain_id, curr_boot_info.chain_id);
    });

    // Verify each multi-block program proof.
    agg_inputs.boot_infos.iter().for_each(|boot_info| {
        // Compute the public values digest as the hash of the abi-encoded [`RawBootInfo`].
        let abi_encoded_boot_info = boot_info.abi_encode();
        let pv_digest = Sha256::digest(abi_encoded_boot_info);

        // Verify the proof against the public values digest.
        if cfg!(target_os = "zkvm") {
            sp1_lib::verify::verify_sp1_proof(&MULTI_BLOCK_PROGRAM_VKEY_DIGEST, &pv_digest.into());
        }
    });

    // Create a map of each l1 head in the [`RawBootInfo`]s to booleans
    let mut l1_heads_map: HashMap<B256, bool> =
        agg_inputs.boot_infos.iter().map(|boot_info| (boot_info.l1_head, false)).collect();

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
    for (l1_head, found) in l1_heads_map.iter() {
        assert!(*found, "l1 head {:?} not found in the provided header chain", l1_head);
    }

    let first_boot_info = &agg_inputs.boot_infos[0];
    let last_boot_info = &agg_inputs.boot_infos[agg_inputs.boot_infos.len() - 1];

    // Consolidate the boot info into an aggregated [`RawBootInfo`] that proves the range.
    let final_boot_info = RawBootInfo {
        l2_output_root: first_boot_info.l2_output_root,
        l2_claim_block: last_boot_info.l2_claim_block,
        l2_claim: last_boot_info.l2_claim,
        l1_head: agg_inputs.latest_l1_checkpoint_head,
        chain_id: last_boot_info.chain_id,
    };

    // Commit to the aggregated [`RawBootInfo`].
    sp1_zkvm::io::commit_slice(&final_boot_info.abi_encode());
}
