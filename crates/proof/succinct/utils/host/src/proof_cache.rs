//! Proof file caching module for saving/loading proofs to/from disk.
//!
//! Centralizes proof path construction and file I/O for range and aggregation proofs.
//! All proofs are stored under `data/{chain_id}/proofs/` with consistent naming.

use std::{fs, path::PathBuf};

use anyhow::Result;
use sp1_sdk::SP1ProofWithPublicValues;

/// Returns the directory for range proofs: `data/{chain_id}/proofs/range`.
pub fn get_range_proof_dir(chain_id: u64) -> PathBuf {
    PathBuf::from(format!("data/{chain_id}/proofs/range"))
}

/// Returns the file path for a range proof: `data/{chain_id}/proofs/range/{start}-{end}.bin`.
pub fn get_range_proof_path(chain_id: u64, start: u64, end: u64) -> PathBuf {
    get_range_proof_dir(chain_id).join(format!("{start}-{end}.bin"))
}

/// Returns the directory for aggregation proofs: `data/{chain_id}/proofs/agg`.
pub fn get_agg_proof_dir(chain_id: u64) -> PathBuf {
    PathBuf::from(format!("data/{chain_id}/proofs/agg"))
}

/// Returns the file path for an aggregation proof: `data/{chain_id}/proofs/agg/{name}.bin`.
pub fn get_agg_proof_path(chain_id: u64, name: &str) -> PathBuf {
    get_agg_proof_dir(chain_id).join(format!("{name}.bin"))
}

/// Save a range proof to disk, creating directories as needed.
pub fn save_range_proof(
    chain_id: u64,
    start: u64,
    end: u64,
    proof: &SP1ProofWithPublicValues,
) -> Result<PathBuf> {
    let dir = get_range_proof_dir(chain_id);
    fs::create_dir_all(&dir)?;
    let path = get_range_proof_path(chain_id, start, end);
    proof.save(&path).expect("saving range proof failed");
    Ok(path)
}

/// Save an aggregation proof to disk, creating directories as needed.
pub fn save_agg_proof(
    chain_id: u64,
    name: &str,
    proof: &SP1ProofWithPublicValues,
) -> Result<PathBuf> {
    let dir = get_agg_proof_dir(chain_id);
    fs::create_dir_all(&dir)?;
    let path = get_agg_proof_path(chain_id, name);
    proof.save(&path).expect("saving agg proof failed");
    Ok(path)
}
