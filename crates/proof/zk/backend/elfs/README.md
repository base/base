# base-proof-succinct-elfs

Embeds the Succinct SP1 zkVM program ELF binaries (the range and aggregation proof programs) so
provers can load them at compile time.

The binaries live out of tree under `crates/proof/zk/programs/succinct/elf/` and are not
committed to git. `build.rs` resolves them from the local cache and exposes their absolute paths
via `cargo:rustc-env` so the constants in this crate can embed them at compile time.
