//! A program that aggregates the proofs of the multi-block program.

#![cfg_attr(target_os = "zkvm", no_main)]
#[cfg(target_os = "zkvm")]
sp1_zkvm::entrypoint!(main);

use alloy_consensus::Header;
use alloy_primitives::B256;
use alloy_sol_types::SolValue;
use std::collections::HashMap;

use op_succinct_client_utils::{boot::BootInfoStruct, types::AggregationInputs};
use sha2::{Digest, Sha256};

/// The verification key for the multi-block program.
///
/// Whenever the multi-block program changes, you will need to update this.
const MULTI_BLOCK_PROGRAM_VKEY_DIGEST: [u32; 8] =
    [1039893330, 1594505873, 415997013, 1198691665, 71280582, 651429912, 87063347, 1840814573];

pub fn main() {
    // Read in the public values corresponding to each multi-block proof.
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

        // The chain ID must be the same for all the boot infos, to ensure they're
        // from the same chain and span batch range.
        assert_eq!(prev_boot_info.chainId, boot_info.chainId);

        // The rollup config must be the same for all the boot infos, to ensure they're
        // from the same chain and span batch range.
        assert_eq!(prev_boot_info.rollupConfigHash, boot_info.rollupConfigHash);
    });

    // Verify each multi-block program proof.
    agg_inputs.boot_infos.iter().for_each(|boot_info| {
        // In the multi-block program, the public values digest is just the hash of the ABI encoded
        // boot info.
        let serialized_boot_info = bincode::serialize(&boot_info).unwrap();
        let pv_digest = Sha256::digest(serialized_boot_info);

        if cfg!(target_os = "zkvm") {
            sp1_lib::verify::verify_sp1_proof(&MULTI_BLOCK_PROGRAM_VKEY_DIGEST, &pv_digest.into());
        }
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
    for (l1_head, found) in l1_heads_map.iter() {
        assert!(*found, "l1 head {:?} not found in the provided header chain", l1_head);
    }

    let first_boot_info = &agg_inputs.boot_infos[0];
    let last_boot_info = &agg_inputs.boot_infos[agg_inputs.boot_infos.len() - 1];
    // Consolidate the boot info into a single BootInfo struct that represents the range proven.
    let final_boot_info = BootInfoStruct {
        // The first boot info's L2 output root is the L2 output root of the range.
        l2PreRoot: first_boot_info.l2PreRoot,
        l2BlockNumber: last_boot_info.l2BlockNumber,
        l2PostRoot: last_boot_info.l2PostRoot,
        l1Head: agg_inputs.latest_l1_checkpoint_head,
        chainId: last_boot_info.chainId,
        rollupConfigHash: last_boot_info.rollupConfigHash,
    };

    // Commit to the aggregated [`BootInfoStruct`].
    sp1_zkvm::io::commit_slice(&final_boot_info.abi_encode());
}
