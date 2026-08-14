//! SP1 stdin encoding for range-program witness data.

use anyhow::Result;
use base_proof_zk_utils::witness::DefaultWitnessData;
use rkyv::to_bytes;
use sp1_sdk::SP1Stdin;

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
