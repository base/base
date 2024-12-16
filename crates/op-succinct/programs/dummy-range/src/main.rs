//! A dummy replica of the `range` program.
//!
//! SAFETY: Does not perform any verification of the rollup state transition.

#![no_main]
sp1_zkvm::entrypoint!(main);

use op_succinct_client_utils::boot::BootInfoStruct;
use op_succinct_client_utils::BootInfoWithBytesConfig;

pub fn main() {
    let boot_info_with_bytes_config = sp1_zkvm::io::read::<BootInfoWithBytesConfig>();

    // BootInfoStruct is identical to BootInfoWithBytesConfig, except it replaces
    // the rollup_config_bytes with a hash of those bytes (rollupConfigHash). Securely
    // hashes the rollup config bytes.
    let boot_info_struct = BootInfoStruct::from(boot_info_with_bytes_config.clone());
    sp1_zkvm::io::commit::<BootInfoStruct>(&boot_info_struct);
}
