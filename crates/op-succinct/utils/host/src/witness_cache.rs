//! SP1Stdin caching module for saving/loading proving inputs to/from disk.
//!
//! This module provides functions to cache SP1Stdin (proving input), keyed by (chain_id,
//! start_block, end_block). Caching allows skipping the time-consuming witness generation step
//! (`host.run()`) on subsequent runs.
//!
//! Note: While SP1Stdin is the same type across all DA implementations, the serialized contents
//! (WitnessData) are DA-specific. Cache files are compatible between Ethereum DA and Celestia DA
//! (both use DefaultWitnessData), but NOT compatible with EigenDA (uses EigenDAWitnessData).

use std::{fs, path::PathBuf};

use anyhow::Result;
use sp1_sdk::SP1Stdin;

/// Returns the cache directory path for a given chain ID.
pub fn get_cache_dir(chain_id: u64) -> PathBuf {
    PathBuf::from(format!("data/{}/witness-cache", chain_id))
}

/// Returns the stdin cache file path for a given block range.
pub fn get_stdin_cache_path(chain_id: u64, start_block: u64, end_block: u64) -> PathBuf {
    get_cache_dir(chain_id).join(format!("{}-{}-stdin.bin", start_block, end_block))
}

/// Save SP1Stdin to cache using bincode.
///
/// Creates the cache directory if it doesn't exist and serializes the stdin using bincode.
/// Note: Cache files are only compatible within the same DA type family (see module docs).
pub fn save_stdin_to_cache(
    chain_id: u64,
    start_block: u64,
    end_block: u64,
    stdin: &SP1Stdin,
) -> Result<PathBuf> {
    let cache_dir = get_cache_dir(chain_id);
    if !cache_dir.exists() {
        fs::create_dir_all(&cache_dir)?;
    }

    let cache_path = get_stdin_cache_path(chain_id, start_block, end_block);
    let bytes = bincode::serialize(stdin)?;
    fs::write(&cache_path, &bytes)?;

    Ok(cache_path)
}

/// Load SP1Stdin from cache if it exists.
///
/// Returns `Ok(Some(stdin))` if the cache file exists and was successfully deserialized,
/// `Ok(None)` if the cache file doesn't exist, or an error if deserialization failed.
pub fn load_stdin_from_cache(
    chain_id: u64,
    start_block: u64,
    end_block: u64,
) -> Result<Option<SP1Stdin>> {
    let cache_path = get_stdin_cache_path(chain_id, start_block, end_block);

    if !cache_path.exists() {
        return Ok(None);
    }

    let bytes = fs::read(&cache_path)?;
    let stdin = bincode::deserialize(&bytes)?;

    Ok(Some(stdin))
}
