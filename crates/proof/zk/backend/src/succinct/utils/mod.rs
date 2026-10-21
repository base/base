//! SP1 stdin and ELF helpers used by the Succinct backends.

mod stdin;
pub use stdin::{get_agg_proof_stdin, get_sp1_stdin};

mod elf;
pub use elf::{
    cluster_setup_keys, cluster_setup_range_key, cluster_setup_vkeys, get_range_elf_embedded,
};
